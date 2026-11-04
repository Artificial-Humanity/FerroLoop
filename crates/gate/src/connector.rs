//! Declarative API-to-MCP connector manifests (`.fl/gate/*.toml` & `~/.config/fl/gate/*.toml`),
//! OpenAPI 3.x importer (`import_openapi_to_connector`), and tool invocation execution (spec §4).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::GateError;
use crate::bridge::BridgeConfig;
use crate::contract::{
    ResponseContract, ResponseSchema, check_pre_parse_depth, count_tokens_bounded,
    count_unit_string,
};
use crate::proxy::{
    DispatchPhaseError, PLACEHOLDER_PREFIX, ProxyRequest, ProxyResponse,
    execute_proxied_request_with_provider_and_contract_phased_with_deadline,
};
use crate::vault::Vault;

fn default_true() -> bool {
    true
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// Declarative `.fl/gate/<id>.toml` or `~/.config/fl/gate/<id>.toml` connector manifest (spec §4.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectorManifest {
    pub id: String,
    pub title: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub base_url: String,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub default_headers: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tools: Vec<ConnectorToolDef>,
    #[serde(default)]
    pub bridges: Vec<BridgeConfig>,
}

/// Declarative HTTP-to-MCP tool mapping inside a [`ConnectorManifest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectorToolDef {
    pub name: String,
    pub description: String,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub path_params: Vec<String>,
    #[serde(default)]
    pub query_params: Vec<String>,
    #[serde(default)]
    pub body_params: Vec<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub response_fields: Vec<String>,
    #[serde(default = "default_input_schema")]
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_contract: Option<ResponseContract>,
}

/// Outcome of evaluating a tool response against its contract, if any.
/// A failed contract carries no body, only its digest, size, and violation.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutcome {
    ContractOk {
        value: Value,
        token_count: Option<u64>,
        count_unit: Option<String>,
    },
    ContractFailedAfterExecution {
        body_sha256: String,
        body_bytes: u64,
        violation: String,
    },
    Uncontracted(Value),
}

impl ToolOutcome {
    pub fn value(&self) -> Option<&Value> {
        match self {
            Self::ContractOk { value, .. } => Some(value),
            Self::Uncontracted(v) => Some(v),
            Self::ContractFailedAfterExecution { .. } => None,
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = String::with_capacity(digest.as_ref().len() * 2);
    for b in digest.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn contract_failed(raw_body: &str, violation: String) -> ToolOutcome {
    contract_failed_bytes(raw_body.as_bytes(), violation)
}

fn contract_failed_bytes(raw_bytes: &[u8], violation: String) -> ToolOutcome {
    ToolOutcome::ContractFailedAfterExecution {
        body_sha256: sha256_hex(raw_bytes),
        body_bytes: raw_bytes.len() as u64,
        violation,
    }
}

/// Map a typed contracted-transport error to a body-free contract failure.
pub fn contract_outcome_for_transport_error(err: &GateError) -> Option<ToolOutcome> {
    match err {
        GateError::ContractResponseTooLarge {
            prefix_sha256,
            read_bytes,
            max_bytes,
            ..
        } => Some(ToolOutcome::ContractFailedAfterExecution {
            // These slots carry the read-prefix digest/count (cap+1 bytes),
            // not a full-body digest/count; the violation text says so.
            body_sha256: prefix_sha256.clone(),
            body_bytes: *read_bytes,
            violation: format!(
                "contract_violation at $: max_response_bytes_exceeded read_prefix_bytes {read_bytes} above max_response_bytes {max_bytes}"
            ),
        }),
        GateError::ContractInvalidUtf8 {
            body_sha256,
            body_bytes,
            ..
        } => Some(ToolOutcome::ContractFailedAfterExecution {
            body_sha256: body_sha256.clone(),
            body_bytes: *body_bytes,
            violation: "contract_violation at $: invalid_utf8 body is not valid UTF-8".to_string(),
        }),
        _ => None,
    }
}

fn transport_error_status(err: &GateError) -> u16 {
    match err {
        GateError::ContractResponseTooLarge { status, .. }
        | GateError::ContractInvalidUtf8 { status, .. } => *status,
        _ => 0,
    }
}

/// Pure evaluation: project `raw_body`, then enforce `contract` if present.
pub fn evaluate_tool_response(
    contract: Option<&ResponseContract>,
    raw_body: &str,
    response_fields: &[String],
) -> ToolOutcome {
    evaluate_tool_response_bytes(contract, raw_body.as_bytes(), response_fields)
}

/// Bytes form of [`evaluate_tool_response`]. Under a contract the raw bytes
/// are size-checked before projection, decoded strictly as UTF-8, depth-
/// scanned before parsing, and only then projected and validated. The
/// legacy (uncontracted) path keeps lossy decoding.
pub fn evaluate_tool_response_bytes(
    contract: Option<&ResponseContract>,
    raw_bytes: &[u8],
    response_fields: &[String],
) -> ToolOutcome {
    let Some(contract) = contract else {
        let raw_body = String::from_utf8_lossy(raw_bytes).into_owned();
        return ToolOutcome::Uncontracted(project_response_body(&raw_body, response_fields));
    };
    if raw_bytes.len() as u64 > contract.max_response_bytes {
        return contract_failed_bytes(
            raw_bytes,
            format!(
                "contract_violation at $: max_response_bytes_exceeded bytes {} above max_response_bytes {}",
                raw_bytes.len(),
                contract.max_response_bytes
            ),
        );
    }
    let raw_body = match std::str::from_utf8(raw_bytes) {
        Ok(s) => s,
        Err(_) => {
            return contract_failed_bytes(
                raw_bytes,
                "contract_violation at $: invalid_utf8 body is not valid UTF-8".to_string(),
            );
        }
    };
    if let Err(v) = check_pre_parse_depth(contract, raw_body) {
        return contract_failed(raw_body, v.to_string());
    }
    if serde_json::from_str::<Value>(raw_body).is_err() {
        return contract_failed(
            raw_body,
            "contract_violation at $: invalid_json body is not valid JSON".to_string(),
        );
    }
    let projected = project_response_body(raw_body, response_fields);
    if let Err(v) = contract.validate_value(&projected) {
        return contract_failed(raw_body, v.to_string());
    }
    let serialized = serde_json::to_string(&projected).unwrap_or_default();
    if serialized.len() as u64 > contract.max_response_bytes {
        return contract_failed(
            raw_body,
            format!(
                "contract_violation at $: max_response_bytes_exceeded bytes {} above max_response_bytes {}",
                serialized.len(),
                contract.max_response_bytes
            ),
        );
    }
    let token_count = match count_tokens_bounded(contract, &serialized) {
        Ok(c) => c,
        Err(v) => return contract_failed(raw_body, v.to_string()),
    };
    let count_unit = if token_count.is_some() {
        count_unit_string()
    } else {
        None
    };
    ToolOutcome::ContractOk {
        value: projected,
        token_count,
        count_unit,
    }
}

/// A parse error as `line L, column C: message`, with no source text.
///
/// `toml::de::Error`'s `Display` quotes the offending source line, so a manifest that is (or
/// links to) another file would print that file's content into an error an agent can read. This
/// keeps the position and the parser's message, and hides the key or value the message quotes
/// (`unknown field `…``, `invalid type: string "…"`): either can be a pasted token. What follows
/// `, expected` and `missing field` name this crate's own schema, so they stay.
fn toml_error_reason(raw: &str, e: &toml::de::Error) -> String {
    let at = e.span().map_or(0, |s| s.start);
    let (mut line, mut column) = (1usize, 1usize);
    for (_, c) in raw.char_indices().take_while(|&(i, _)| i < at) {
        if c == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    format!(
        "line {line}, column {column}: {}",
        redact_toml_message(e.message().trim_end())
    )
}

fn redact_toml_message(message: &str) -> String {
    if message.starts_with("missing field ") {
        return message.to_string();
    }
    let split = message.rfind(", expected").unwrap_or(message.len());
    let (head, tail) = message.split_at(split);
    // From the first quote to the last, so a value that holds a quote of its own is hidden whole.
    match (head.find(['`', '"']), head.rfind(['`', '"'])) {
        (Some(first), Some(last)) if last > first => {
            format!("{}<value>{}{tail}", &head[..first], &head[last + 1..])
        }
        (Some(first), _) => format!("{}<value>{tail}", &head[..first]),
        _ => message.to_string(),
    }
}

fn default_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {}
    })
}

/// A connector id is lowercase ASCII alphanumeric, `-`, or `_`, and never empty. Checked as written,
/// without trimming.
pub(crate) fn is_valid_connector_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

impl ConnectorManifest {
    /// Parse and validate a [`ConnectorManifest`] from TOML text.
    pub fn parse_toml(raw: &str, source_path: &Path) -> Result<Self, GateError> {
        let manifest: Self = toml::from_str(raw).map_err(|e| GateError::InvalidConnector {
            path: source_path.to_path_buf(),
            reason: toml_error_reason(raw, &e),
        })?;
        manifest.validate(source_path)?;
        Ok(manifest)
    }

    pub fn validate(&self, source_path: &Path) -> Result<(), GateError> {
        // Checked as written: a padded id passed `trim()` yet was stored, matched, and published
        // with its padding.
        let id = self.id.as_str();
        if !is_valid_connector_id(id) {
            return Err(GateError::InvalidConnector {
                path: source_path.to_path_buf(),
                reason: format!(
                    "connector id `{id}` must be lowercase ASCII alphanumeric, `-`, or `_`"
                ),
            });
        }
        if self.base_url.trim().is_empty() {
            return Err(GateError::InvalidConnector {
                path: source_path.to_path_buf(),
                reason: "connector `base_url` must not be empty".to_string(),
            });
        }
        for bridge in &self.bridges {
            for entry in &bridge.allowed_inline_connectors {
                let entry = entry.trim();
                if entry == "*" {
                    return Err(GateError::InvalidConnector {
                        path: source_path.to_path_buf(),
                        reason: format!(
                            "bridge `{}` allowed_inline_connectors must list connector ids; `*` is \
                             not a wildcard",
                            bridge.id
                        ),
                    });
                }
                if !is_valid_connector_id(entry) {
                    return Err(GateError::InvalidConnector {
                        path: source_path.to_path_buf(),
                        reason: format!(
                            "bridge `{}` allowed_inline_connectors entry `{entry}` is not a valid \
                             connector id (lowercase ASCII alphanumeric, `-`, or `_`)",
                            bridge.id
                        ),
                    });
                }
            }
        }
        let mut seen_tools = std::collections::BTreeSet::new();
        for tool in &self.tools {
            if !seen_tools.insert(tool.name.as_str()) {
                return Err(GateError::InvalidConnector {
                    path: source_path.to_path_buf(),
                    reason: format!(
                        "tool `{}` is declared twice; the later one could never be called",
                        tool.name
                    ),
                });
            }
            if tool.name.trim().is_empty() {
                return Err(GateError::InvalidConnector {
                    path: source_path.to_path_buf(),
                    reason: "tool `name` must not be empty".to_string(),
                });
            }
            if !tool.path.starts_with('/') {
                return Err(GateError::InvalidConnector {
                    path: source_path.to_path_buf(),
                    reason: format!(
                        "tool `{}` path `{}` must start with `/`",
                        tool.name, tool.path
                    ),
                });
            }
            if let Some(contract) = &tool.response_contract {
                // Re-check the stored schema value (programmatic construction
                // can bypass Deserialize) and the load-time enforceability.
                if let Err(e) = ResponseSchema::from_value(contract.schema.as_value().clone()) {
                    return Err(GateError::InvalidConnector {
                        path: source_path.to_path_buf(),
                        reason: format!("tool `{}` response_contract: {e}", tool.name),
                    });
                }
                if let Err(e) = contract.validate_load() {
                    return Err(GateError::InvalidConnector {
                        path: source_path.to_path_buf(),
                        reason: format!("tool `{}` response_contract: {e}", tool.name),
                    });
                }
            }
        }
        Ok(())
    }

    /// Build a [`ProxyRequest`] from `tool` and JSON `arguments`.
    pub fn build_tool_request(
        &self,
        tool: &ConnectorToolDef,
        arguments: &Value,
        caller_agent: Option<String>,
    ) -> Result<ProxyRequest, GateError> {
        // Only the manifest, which the operator wrote, may name a secret. A placeholder inside
        // an agent-supplied argument would be substituted into the request body and sent upstream.
        if let Some(found) = find_placeholder(arguments) {
            return Err(GateError::InvalidToolArgs {
                tool: tool.name.clone(),
                reason: format!("arguments must not contain `{PLACEHOLDER_PREFIX}`: {found}"),
            });
        }
        let args_obj = arguments.as_object();
        let mut rendered_path = tool.path.clone();

        for param in &tool.path_params {
            let val_str = args_obj
                .and_then(|m| m.get(param))
                .and_then(value_to_param_string)
                .ok_or_else(|| GateError::InvalidToolArgs {
                    tool: tool.name.clone(),
                    reason: format!("missing required path parameter `{param}`"),
                })?;
            // `.` and `..` survive percent-encoding and are dot-segments to the server.
            if matches!(val_str.as_str(), "" | "." | "..") {
                return Err(GateError::InvalidToolArgs {
                    tool: tool.name.clone(),
                    reason: format!("path parameter `{param}` must not be empty, `.`, or `..`"),
                });
            }
            let token = format!("{{{param}}}");
            rendered_path = rendered_path.replace(&token, &encode_uri_component(&val_str));
        }

        let mut query_pairs = Vec::new();
        for qparam in &tool.query_params {
            if let Some(v) = args_obj.and_then(|m| m.get(qparam))
                && !v.is_null()
                && let Some(s) = value_to_param_string(v)
            {
                query_pairs.push(format!(
                    "{}={}",
                    encode_uri_component(qparam),
                    encode_uri_component(&s)
                ));
            }
        }

        let base = self.base_url.trim_end_matches('/');
        let mut url = format!("{base}{rendered_path}");
        if !query_pairs.is_empty() {
            let sep = if url.contains('?') { '&' } else { '?' };
            url.push(sep);
            url.push_str(&query_pairs.join("&"));
        }

        let (_scheme, target_host, _port) = crate::proxy::parse_url_scheme_host_port(&url)?;
        if !self.allowed_hosts.is_empty()
            && !self
                .allowed_hosts
                .iter()
                .any(|pat| crate::proxy::host_matches_pattern(&target_host, pat))
        {
            return Err(GateError::PolicyDenied {
                secret: format!("connector:{}", self.id),
                reason: format!(
                    "target host `{target_host}` is not in connector `{}` allowed_hosts {:?}",
                    self.id, self.allowed_hosts
                ),
            });
        }

        let mut headers = self.default_headers.clone();
        for (hk, hv) in &tool.headers {
            headers.insert(hk.clone(), hv.clone());
        }

        let method = tool.method.trim().to_ascii_uppercase();
        let body = if matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
            if !tool.body_params.is_empty() {
                let mut body_map = serde_json::Map::new();
                if let Some(obj) = args_obj {
                    for bparam in &tool.body_params {
                        if let Some(v) = obj.get(bparam) {
                            body_map.insert(bparam.clone(), v.clone());
                        }
                    }
                }
                Some(Value::Object(body_map).to_string())
            } else if let Some(obj) = args_obj {
                let mut body_map = serde_json::Map::new();
                for (k, v) in obj {
                    if !tool.path_params.contains(k) && !tool.query_params.contains(k) {
                        body_map.insert(k.clone(), v.clone());
                    }
                }
                if body_map.is_empty() {
                    None
                } else {
                    Some(Value::Object(body_map).to_string())
                }
            } else {
                None
            }
        } else {
            None
        };

        Ok(ProxyRequest {
            method,
            url,
            headers,
            body,
            caller_agent,
            timeout_secs: Some(30),
        })
    }

    /// Strict sibling of [`Self::execute_tool`]: after execution (already
    /// scrubbed by the proxy) and projection, enforce the tool's contract.
    /// Contract failures are reported as a body-free [`ToolOutcome`], not an error.
    pub fn execute_tool_outcome(
        &self,
        vault: &Vault,
        tool_name: &str,
        arguments: &Value,
        caller_agent: Option<String>,
    ) -> Result<(ProxyResponse, ToolOutcome), GateError> {
        self.execute_tool_outcome_with_timeout(vault, tool_name, arguments, caller_agent, None)
    }

    /// [`Self::execute_tool_outcome`] with the HTTP timeout overridden
    /// (governed dispatch clamps it to the permit's remaining deadline).
    pub fn execute_tool_outcome_with_timeout(
        &self,
        vault: &Vault,
        tool_name: &str,
        arguments: &Value,
        caller_agent: Option<String>,
        timeout_secs: Option<u64>,
    ) -> Result<(ProxyResponse, ToolOutcome), GateError> {
        self.execute_tool_outcome_phased(
            vault,
            tool_name,
            arguments,
            caller_agent,
            timeout_secs,
            None,
        )
        .map_err(DispatchPhaseError::into_gate_error)
    }

    /// Phased dispatch: request-build and proxy-preparation failures are
    /// `DispatchPhaseError::BeforeSend` (no byte sent); failures once send
    /// has been initiated are `AfterSend`. Contract transport failures are
    /// an executed outcome, not an error.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_tool_outcome_phased(
        &self,
        vault: &Vault,
        tool_name: &str,
        arguments: &Value,
        caller_agent: Option<String>,
        timeout_secs: Option<u64>,
        provider: Option<&dyn crate::adapter::SecretProviderAdapter>,
    ) -> Result<(ProxyResponse, ToolOutcome), DispatchPhaseError> {
        self.execute_tool_outcome_phased_with_deadline(
            vault,
            tool_name,
            arguments,
            caller_agent,
            timeout_secs,
            provider,
            None,
            &crate::vault::now_unix_millis,
        )
    }

    /// [`Self::execute_tool_outcome_phased`] with a permit deadline re-checked
    /// (fresh `now_fn()`) after preparation, before any byte is sent.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_tool_outcome_phased_with_deadline(
        &self,
        vault: &Vault,
        tool_name: &str,
        arguments: &Value,
        caller_agent: Option<String>,
        timeout_secs: Option<u64>,
        provider: Option<&dyn crate::adapter::SecretProviderAdapter>,
        deadline_mono_ms: Option<u64>,
        now_fn: &dyn Fn() -> u64,
    ) -> Result<(ProxyResponse, ToolOutcome), DispatchPhaseError> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.name == tool_name)
            .ok_or_else(|| {
                DispatchPhaseError::BeforeSend(GateError::NoSuchTool {
                    tool: tool_name.to_string(),
                })
            })?;
        let mut req = self
            .build_tool_request(tool, arguments, caller_agent)
            .map_err(DispatchPhaseError::BeforeSend)?;
        if let Some(t) = timeout_secs {
            req.timeout_secs = Some(t);
        }
        let resp = match execute_proxied_request_with_provider_and_contract_phased_with_deadline(
            vault,
            &req,
            provider,
            tool.response_contract.as_ref(),
            deadline_mono_ms,
            now_fn,
        ) {
            Ok(r) => r,
            Err(e) => {
                // Contract transport errors only arise after execution.
                if let DispatchPhaseError::AfterSend(ge) = &e
                    && let Some(outcome) = contract_outcome_for_transport_error(ge)
                {
                    let synthetic = ProxyResponse {
                        status: transport_error_status(ge),
                        headers: BTreeMap::new(),
                        body: String::new(),
                        scrubbed_secrets: vec![],
                    };
                    return Ok((synthetic, outcome));
                }
                return Err(e);
            }
        };
        let outcome = evaluate_tool_response_bytes(
            tool.response_contract.as_ref(),
            resp.body.as_bytes(),
            &tool.response_fields,
        );
        Ok((resp, outcome))
    }

    /// Provider variant of [`Self::execute_tool_outcome`] (MCP provider path).
    pub fn execute_tool_outcome_with_provider(
        &self,
        vault: &Vault,
        tool_name: &str,
        arguments: &Value,
        caller_agent: Option<String>,
        provider: Option<&dyn crate::adapter::SecretProviderAdapter>,
    ) -> Result<(ProxyResponse, ToolOutcome), GateError> {
        self.execute_tool_outcome_phased(vault, tool_name, arguments, caller_agent, None, provider)
            .map_err(DispatchPhaseError::into_gate_error)
    }

    /// Alias for [`Self::execute_tool_outcome`].
    pub fn invoke_tool_outcome(
        &self,
        vault: &Vault,
        tool_name: &str,
        arguments: &Value,
        caller_agent: Option<String>,
    ) -> Result<(ProxyResponse, ToolOutcome), GateError> {
        self.execute_tool_outcome(vault, tool_name, arguments, caller_agent)
    }

    /// [`Self::invoke_tool_outcome`] with an HTTP timeout override.
    pub fn invoke_tool_outcome_with_timeout(
        &self,
        vault: &Vault,
        tool_name: &str,
        arguments: &Value,
        caller_agent: Option<String>,
        timeout_secs: Option<u64>,
    ) -> Result<(ProxyResponse, ToolOutcome), GateError> {
        self.execute_tool_outcome_with_timeout(
            vault,
            tool_name,
            arguments,
            caller_agent,
            timeout_secs,
        )
    }

    /// Alias for [`Self::execute_tool`].
    pub fn invoke_tool(
        &self,
        vault: &Vault,
        tool_name: &str,
        arguments: &Value,
        caller_agent: Option<String>,
    ) -> Result<(ProxyResponse, Value), GateError> {
        self.execute_tool(vault, tool_name, arguments, caller_agent)
    }

    /// Legacy entry point, now delegating to [`Self::execute_tool_outcome`]:
    /// a contract failure is a body-free [`GateError::ContractFailed`]
    /// refusal, never the projected body.
    pub fn execute_tool(
        &self,
        vault: &Vault,
        tool_name: &str,
        arguments: &Value,
        caller_agent: Option<String>,
    ) -> Result<(ProxyResponse, Value), GateError> {
        let (resp, outcome) =
            self.execute_tool_outcome(vault, tool_name, arguments, caller_agent)?;
        match outcome {
            ToolOutcome::ContractOk { value, .. } => Ok((resp, value)),
            ToolOutcome::Uncontracted(value) => Ok((resp, value)),
            ToolOutcome::ContractFailedAfterExecution {
                body_sha256,
                body_bytes,
                violation,
            } => Err(GateError::ContractFailed {
                violation,
                body_sha256,
                body_bytes,
            }),
        }
    }
}

/// Apply `response_fields` projection to a JSON response body (if non-empty) so LLM context
/// receives only the curated fields (spec §4.1).
pub fn project_response_body(raw_body: &str, response_fields: &[String]) -> Value {
    let Ok(parsed) = serde_json::from_str::<Value>(raw_body) else {
        return Value::String(raw_body.to_string());
    };
    if response_fields.is_empty() {
        return parsed;
    }
    project_value(&parsed, response_fields)
}

fn project_value(val: &Value, fields: &[String]) -> Value {
    match val {
        Value::Object(_) => {
            let mut out = serde_json::Map::new();
            for f in fields {
                let segments = parse_field_path(f);
                if let Some(fragment) = project_path(val, &segments)
                    && let Value::Object(frag) = fragment
                {
                    for (k, v) in frag {
                        merge_projected(&mut out, k, v);
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|item| project_value(item, fields)).collect())
        }
        other => other.clone(),
    }
}

/// One path segment: a field name and whether `[]` follows it.
fn parse_field_path(field: &str) -> Vec<(String, bool)> {
    field
        .split('.')
        .filter(|s| !s.is_empty())
        .map(|seg| match seg.strip_suffix("[]") {
            Some(name) => (name.to_string(), true),
            None => (seg.to_string(), false),
        })
        .collect()
}

/// Projected fragment (an object) for `segments` under `val`, or `None` if absent.
fn project_path(val: &Value, segments: &[(String, bool)]) -> Option<Value> {
    let ((name, is_array), rest) = segments.split_first()?;
    let child = val.as_object()?.get(name)?;
    let projected_child = if *is_array {
        let arr = child.as_array()?;
        let items: Vec<Value> = arr
            .iter()
            .map(|item| {
                if rest.is_empty() {
                    item.clone()
                } else {
                    project_path(item, rest)
                        .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
                }
            })
            .collect();
        Value::Array(items)
    } else if rest.is_empty() {
        child.clone()
    } else {
        project_path(child, rest)?
    };
    let mut out = serde_json::Map::new();
    out.insert(name.clone(), projected_child);
    Some(Value::Object(out))
}

fn merge_projected(out: &mut serde_json::Map<String, Value>, key: String, value: Value) {
    match out.get_mut(&key) {
        None => {
            out.insert(key, value);
        }
        Some(existing) => merge_value(existing, value),
    }
}

fn merge_value(existing: &mut Value, incoming: Value) {
    match (existing, incoming) {
        (Value::Object(dst), Value::Object(src)) => {
            for (k, v) in src {
                match dst.get_mut(&k) {
                    Some(e) => merge_value(e, v),
                    None => {
                        dst.insert(k, v);
                    }
                }
            }
        }
        (Value::Array(dst), Value::Array(src)) => {
            for (i, v) in src.into_iter().enumerate() {
                if i < dst.len() {
                    // An empty object is a missing-field placeholder; it must not erase data.
                    if v.as_object().is_some_and(|o| o.is_empty())
                        && !dst[i].as_object().is_some_and(|o| o.is_empty())
                    {
                        continue;
                    }
                    merge_value(&mut dst[i], v);
                } else {
                    dst.push(v);
                }
            }
        }
        (dst, src) => *dst = src,
    }
}

/// Describe the first string or object key in `v` that holds a secret placeholder.
fn find_placeholder(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if s.contains(PLACEHOLDER_PREFIX) => Some("a string value".to_string()),
        Value::Array(items) => items.iter().find_map(find_placeholder),
        Value::Object(map) => map.iter().find_map(|(k, v)| {
            if k.contains(PLACEHOLDER_PREFIX) {
                Some("an object key".to_string())
            } else {
                find_placeholder(v)
            }
        }),
        _ => None,
    }
}

fn value_to_param_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            use std::fmt::Write as _;
            let _ = write!(&mut out, "%{b:02X}");
        }
    }
    out
}

/// Connectors found by [`discover_connectors_with_warnings`], and what it skipped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnectorDiscovery {
    pub connectors: Vec<ConnectorManifest>,
    /// One line per directory or manifest that was skipped on purpose (a symbolic link). Each
    /// names the path only: never the link's target and never any file content.
    pub warnings: Vec<String>,
}

/// Discover all `.fl/gate/*.toml` (project) and `~/.config/fl/gate/*.toml` (user) connector manifests.
/// Project-level connectors take precedence over user-level connectors with the same `id`.
///
/// Symbolic links are never followed: a manifest that is a link is skipped, and so is a connector
/// directory that is a link (see [`discover_connectors_with_warnings`]). Each skip is reported once
/// per process on stderr; call [`discover_connectors_with_warnings`] to receive the warnings.
pub fn discover_connectors(
    project_root: Option<&Path>,
    home_override: Option<&Path>,
) -> Result<Vec<ConnectorManifest>, GateError> {
    let found = discover_connectors_with_warnings(project_root, home_override)?;
    warn_once(&found.warnings);
    Ok(found.connectors)
}

/// [`discover_connectors`] that returns the skip warnings instead of printing them.
///
/// A symlinked manifest, a symlinked connector directory (`.fl/gate`, `~/.config/fl/gate`) and a
/// symlinked `.fl` inside a project are skipped with a warning and the rest still load. They are
/// not errors, and nothing behind the link is opened: `toml` quotes source in its errors and the
/// link can point at a key file or another tool's config. A manifest that is a regular file but
/// malformed is still an error, and its text names the position and the parser's message only.
pub fn discover_connectors_with_warnings(
    project_root: Option<&Path>,
    home_override: Option<&Path>,
) -> Result<ConnectorDiscovery, GateError> {
    let mut by_id = BTreeMap::new();
    let mut warnings = Vec::new();

    let user_dir = if let Some(home) = home_override {
        home.join(".config").join("fl").join("gate")
    } else if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        PathBuf::from(xdg).join("fl").join("gate")
    } else {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".config").join("fl").join("gate")
    };

    if connector_dir_usable(&user_dir, &[], &mut warnings) {
        for manifest in load_connectors_from_dir(&user_dir, &mut warnings)? {
            by_id.insert(manifest.id.clone(), manifest);
        }
    }

    if let Some(root) = project_root {
        let fl_dir = root.join(".fl");
        let proj_dir = fl_dir.join("gate");
        if connector_dir_usable(&proj_dir, &[&fl_dir], &mut warnings) {
            for manifest in load_connectors_from_dir(&proj_dir, &mut warnings)? {
                by_id.insert(manifest.id.clone(), manifest);
            }
        }
    }

    Ok(ConnectorDiscovery {
        connectors: by_id.into_values().collect(),
        warnings,
    })
}

/// Whether `dir` is a real directory to scan. A missing one is skipped silently; a symbolic link,
/// either `dir` itself or one of `parents` (the project's `.fl`, which a repository controls), is
/// skipped with a warning. `Path::is_dir` follows links, so it cannot make this decision.
fn connector_dir_usable(dir: &Path, parents: &[&Path], warnings: &mut Vec<String>) -> bool {
    for link in parents.iter().copied().chain(std::iter::once(dir)) {
        if let Ok(meta) = fs::symlink_metadata(link)
            && meta.file_type().is_symlink()
        {
            warnings.push(format!(
                "skipped connector directory {}: it is a symbolic link, which fl does not follow",
                link.display()
            ));
            return false;
        }
    }
    fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
}

/// Print each distinct warning once per process: the hub calls discovery on every registry request.
fn warn_once(warnings: &[String]) {
    use std::collections::BTreeSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(BTreeSet::new()));
    let Ok(mut seen) = seen.lock() else {
        return;
    };
    for w in warnings {
        if seen.insert(w.clone()) {
            eprintln!("fl gate: {w}");
        }
    }
}

fn load_connectors_from_dir(
    dir: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<ConnectorManifest>, GateError> {
    let mut paths = Vec::new();
    let entries = fs::read_dir(dir).map_err(|e| GateError::Io {
        op: "read_dir",
        path: dir.to_path_buf(),
        cause: e.to_string(),
    })?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        // `symlink_metadata`, so a link is seen as a link. A link is skipped with a warning (it
        // must not abort the other manifests); regular files only are read, so a FIFO is not opened.
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => warnings.push(format!(
                "skipped connector manifest {}: it is a symbolic link, which fl does not follow",
                p.display()
            )),
            Ok(m) if m.is_file() => paths.push(p),
            _ => {}
        }
    }
    paths.sort();
    warnings.sort();

    let mut manifests = Vec::with_capacity(paths.len());
    for p in paths {
        let raw = fs::read_to_string(&p).map_err(|e| GateError::Io {
            op: "read",
            path: p.clone(),
            cause: e.to_string(),
        })?;
        manifests.push(ConnectorManifest::parse_toml(&raw, &p)?);
    }
    Ok(manifests)
}

/// Convert an OpenAPI 3.x JSON specification into a declarative [`ConnectorManifest`] (spec §4.2).
pub fn import_openapi_to_connector(
    openapi_json: &str,
    connector_id: &str,
    secret_name: Option<&str>,
) -> Result<ConnectorManifest, GateError> {
    let doc: Value =
        serde_json::from_str(openapi_json).map_err(|e| GateError::InvalidConnector {
            path: PathBuf::from("<openapi>"),
            reason: format!("invalid OpenAPI JSON: {e}"),
        })?;

    let title = doc
        .pointer("/info/title")
        .and_then(Value::as_str)
        .unwrap_or(connector_id)
        .to_string();
    let version = doc
        .pointer("/info/version")
        .and_then(Value::as_str)
        .unwrap_or("0.1.0")
        .to_string();
    let description = doc
        .pointer("/info/description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let base_url = doc
        .pointer("/servers/0/url")
        .and_then(Value::as_str)
        .unwrap_or("https://api.example.com")
        .to_string();

    let mut allowed_hosts = Vec::new();
    if let Ok((_scheme, host, _port)) = crate::proxy::parse_url_scheme_host_port(&base_url) {
        allowed_hosts.push(host);
    }

    let mut default_headers = BTreeMap::new();
    if let Some(sec) = secret_name {
        let norm_sec = crate::vault::normalize_secret_name(sec)?;
        default_headers.insert(
            "Authorization".to_string(),
            format!("Bearer {{{{FERROGATE:{norm_sec}}}}}"),
        );
    }

    let mut tools = Vec::new();
    if let Some(paths_obj) = doc.get("paths").and_then(Value::as_object) {
        for (path_str, methods_val) in paths_obj {
            let Some(methods_obj) = methods_val.as_object() else {
                continue;
            };
            for (http_method, op_val) in methods_obj {
                let m_upper = http_method.to_ascii_uppercase();
                if !matches!(
                    m_upper.as_str(),
                    "GET" | "POST" | "PUT" | "PATCH" | "DELETE"
                ) {
                    continue;
                }
                let Some(op_obj) = op_val.as_object() else {
                    continue;
                };
                let raw_op_id = op_obj
                    .get("operationId")
                    .and_then(Value::as_str)
                    .map(sanitize_tool_name)
                    .unwrap_or_else(|| {
                        let slug = sanitize_tool_name(path_str);
                        format!(
                            "{}_{}_{}",
                            connector_id,
                            http_method.to_ascii_lowercase(),
                            slug
                        )
                    });
                let tool_name = if raw_op_id.starts_with(connector_id) {
                    raw_op_id
                } else {
                    format!("{connector_id}_{raw_op_id}")
                };
                let tool_name = unique_tool_name(&tools, tool_name);
                let tool_desc = op_obj
                    .get("summary")
                    .or_else(|| op_obj.get("description"))
                    .and_then(Value::as_str)
                    .unwrap_or(&tool_name)
                    .to_string();

                let mut path_params = Vec::new();
                let mut query_params = Vec::new();
                let mut body_params = Vec::new();
                let mut props = serde_json::Map::new();
                let mut required = Vec::new();

                if let Some(params_arr) = op_obj.get("parameters").and_then(Value::as_array) {
                    for p in params_arr {
                        let Some(p_name) = p.get("name").and_then(Value::as_str) else {
                            continue;
                        };
                        let p_in = p.get("in").and_then(Value::as_str).unwrap_or("query");
                        let p_req = p.get("required").and_then(Value::as_bool).unwrap_or(false);
                        let p_schema = p
                            .get("schema")
                            .cloned()
                            .unwrap_or_else(|| json!({ "type": "string" }));
                        props.insert(p_name.to_string(), p_schema);
                        if p_req {
                            required.push(Value::String(p_name.to_string()));
                        }
                        match p_in {
                            "path" => path_params.push(p_name.to_string()),
                            "query" => query_params.push(p_name.to_string()),
                            _ => {}
                        }
                    }
                }

                if let Some(rb_props) = op_val
                    .pointer("/requestBody/content/application~1json/schema/properties")
                    .and_then(Value::as_object)
                {
                    for (bk, bv) in rb_props {
                        body_params.push(bk.clone());
                        props.insert(bk.clone(), bv.clone());
                    }
                }
                if let Some(rb_req) = op_val
                    .pointer("/requestBody/content/application~1json/schema/required")
                    .and_then(Value::as_array)
                {
                    for r in rb_req {
                        if let Some(rs) = r.as_str() {
                            let val = Value::String(rs.to_string());
                            if !required.contains(&val) {
                                required.push(val);
                            }
                        }
                    }
                }

                let input_schema = json!({
                    "type": "object",
                    "properties": props,
                    "required": required,
                });

                tools.push(ConnectorToolDef {
                    name: tool_name,
                    description: tool_desc,
                    method: m_upper,
                    path: path_str.clone(),
                    path_params,
                    query_params,
                    body_params,
                    headers: BTreeMap::new(),
                    response_fields: Vec::new(),
                    input_schema,
                    response_contract: None,
                });
            }
        }
    }

    let manifest = ConnectorManifest {
        id: connector_id.to_ascii_lowercase(),
        title,
        version,
        description,
        base_url,
        allowed_hosts,
        default_headers,
        enabled: true,
        tools,
        bridges: Vec::new(),
    };
    manifest.validate(Path::new("<openapi>"))?;
    Ok(manifest)
}

/// `name`, or `name_2`, `name_3`, … when an earlier tool took it. Distinct paths can sanitize to one name.
fn unique_tool_name(tools: &[ConnectorToolDef], name: String) -> String {
    if !tools.iter().any(|t| t.name == name) {
        return name;
    }
    (2..)
        .map(|n| format!("{name}_{n}"))
        .find(|candidate| !tools.iter().any(|t| &t.name == candidate))
        .unwrap_or(name)
}

fn sanitize_tool_name(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "op".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(extra_tool: &str) -> ConnectorManifest {
        let toml = format!(
            r#"
id = "chat"
title = "Chat"
base_url = "https://api.example.com/v1"
default_headers = {{ Authorization = "Bearer {{{{FERROGATE:CHAT_TOKEN}}}}" }}

[[tools]]
name = "chat_get_channel"
description = "Read a channel"
method = "GET"
path = "/channels/{{channel_id}}"
path_params = ["channel_id"]
query_params = ["filter"]

[[tools]]
name = "chat_post"
description = "Post"
method = "POST"
path = "/channels/{{channel_id}}/messages"
path_params = ["channel_id"]
body_params = ["content"]
{extra_tool}
"#
        );
        ConnectorManifest::parse_toml(&toml, Path::new("chat.toml")).unwrap()
    }

    fn call(m: &ConnectorManifest, tool: &str, args: Value) -> Result<ProxyRequest, GateError> {
        let tool = m.tools.iter().find(|t| t.name == tool).unwrap();
        m.build_tool_request(tool, &args, Some("agent".into()))
    }

    #[test]
    fn an_argument_cannot_name_a_secret() {
        let m = manifest("");
        for args in [
            json!({"channel_id": "c1", "content": "{{FERROGATE:CHAT_TOKEN}}"}),
            json!({"channel_id": "c1", "content": ["ok", {"deep": "x {{FERROGATE:CHAT_TOKEN}}"}]}),
            json!({"channel_id": "{{FERROGATE:CHAT_TOKEN}}"}),
            json!({"channel_id": "c1", "{{FERROGATE:CHAT_TOKEN}}": 1}),
        ] {
            assert!(
                matches!(
                    call(&m, "chat_post", args.clone()),
                    Err(GateError::InvalidToolArgs { .. })
                ),
                "{args}"
            );
        }
        let ok = call(
            &m,
            "chat_post",
            json!({"channel_id": "c1", "content": "hello {braces}"}),
        )
        .unwrap();
        assert_eq!(ok.body.as_deref(), Some(r#"{"content":"hello {braces}"}"#));
        // The manifest's own placeholder is untouched.
        assert!(ok.headers["Authorization"].contains("{{FERROGATE:CHAT_TOKEN}}"));
    }

    #[test]
    fn a_path_parameter_cannot_climb_out_of_its_segment() {
        let m = manifest("");
        for bad in ["..", ".", ""] {
            assert!(
                matches!(
                    call(&m, "chat_get_channel", json!({"channel_id": bad})),
                    Err(GateError::InvalidToolArgs { .. })
                ),
                "{bad:?}"
            );
        }
        let req = call(
            &m,
            "chat_get_channel",
            json!({"channel_id": "a/../b", "filter": "x y"}),
        )
        .unwrap();
        assert_eq!(
            req.url,
            "https://api.example.com/v1/channels/a%2F..%2Fb?filter=x%20y"
        );
        let req = call(&m, "chat_get_channel", json!({"channel_id": "..a"})).unwrap();
        assert!(req.url.ends_with("/channels/..a"), "{}", req.url);
    }

    fn test_contract(schema: Value, max_bytes: u64) -> ResponseContract {
        ResponseContract {
            schema: ResponseSchema::from_value(schema).unwrap(),
            max_response_bytes: max_bytes,
            max_response_tokens: None,
            max_depth: 32,
            max_array_len: 1000,
            max_string_len: 100_000,
        }
    }

    #[test]
    fn non_json_under_contract_refused() {
        let c = test_contract(
            json!({"type":"object","additionalProperties":false,"properties":{"id":{"type":"string"}}}),
            1_000_000,
        );
        let out = evaluate_tool_response(Some(&c), "not json at all", &[]);
        assert!(
            matches!(out, ToolOutcome::ContractFailedAfterExecution { .. }),
            "{out:?}"
        );
    }

    #[test]
    fn contract_failure_returns_no_body() {
        let c = test_contract(
            json!({"type":"object","additionalProperties":false,"required":["id"],"properties":{"id":{"type":"string"}}}),
            1_000_000,
        );
        let body = r#"{"id":123,"secret_marker":"should-not-leak"}"#;
        let out = evaluate_tool_response(Some(&c), body, &[]);
        match out {
            ToolOutcome::ContractFailedAfterExecution {
                body_sha256,
                body_bytes,
                violation,
            } => {
                assert_eq!(body_bytes, body.len() as u64);
                assert_eq!(body_sha256.len(), 64);
                assert!(!violation.contains("should-not-leak"), "{violation}");
                assert!(!violation.contains("123"), "{violation}");
            }
            other => panic!("expected contract failure, got {other:?}"),
        }
        // Over-size envelope also fails with no body.
        let small = test_contract(json!({"type":"string"}), 4);
        assert!(matches!(
            evaluate_tool_response(Some(&small), r#""hello world""#, &[]),
            ToolOutcome::ContractFailedAfterExecution { .. }
        ));
    }

    #[test]
    fn uncontracted_flagged_not_silent() {
        let out = evaluate_tool_response(None, r#"{"id":1}"#, &[]);
        assert_eq!(out, ToolOutcome::Uncontracted(json!({"id": 1})));
        let c = test_contract(
            json!({"type":"object","additionalProperties":false,"properties":{"id":{"type":"integer"}}}),
            1_000_000,
        );
        assert_eq!(
            evaluate_tool_response(Some(&c), r#"{"id":1}"#, &[]),
            ToolOutcome::ContractOk {
                value: json!({"id": 1}),
                token_count: None,
                count_unit: None
            }
        );
    }

    #[test]
    fn oversize_raw_with_tiny_projection_fails() {
        // Raw is over the byte bound even though projection `id` is tiny.
        let mut c = test_contract(
            json!({"type":"object","additionalProperties":false,"properties":{"id":{"type":"integer"}}}),
            64,
        );
        c.max_response_bytes = 64;
        let body = format!(r#"{{"id":1,"pad":"{}"}}"#, "x".repeat(200));
        let out = evaluate_tool_response(Some(&c), &body, &["id".to_string()]);
        match out {
            ToolOutcome::ContractFailedAfterExecution {
                violation,
                body_bytes,
                ..
            } => {
                assert!(
                    violation.contains("max_response_bytes_exceeded"),
                    "{violation}"
                );
                assert_eq!(body_bytes, body.len() as u64);
            }
            other => panic!("expected oversize failure, got {other:?}"),
        }
    }

    #[test]
    fn contract_ok_records_token_count_and_unit_when_bound_present() {
        let mut c = test_contract(json!({"type":"string"}), 1_000_000);
        c.max_response_tokens = Some(1000);
        let out = evaluate_tool_response(Some(&c), r#""hello""#, &[]);
        if fl_core::token_count::count_unit().is_some() {
            match out {
                ToolOutcome::ContractOk {
                    token_count,
                    count_unit,
                    ..
                } => {
                    assert!(token_count.unwrap() > 0);
                    assert_eq!(count_unit.as_deref(), Some("o200k_base"));
                }
                other => panic!("expected ContractOk, got {other:?}"),
            }
        } else {
            assert!(matches!(
                out,
                ToolOutcome::ContractFailedAfterExecution { .. }
            ));
        }
    }

    #[test]
    fn malformed_utf8_and_deep_nesting_fail_pre_parse() {
        let c = test_contract(
            json!({"type":"object","additionalProperties":false}),
            1_000_000,
        );
        let out = evaluate_tool_response_bytes(Some(&c), b"{\"id\":\xff\xfe}", &[]);
        match &out {
            ToolOutcome::ContractFailedAfterExecution { violation, .. } => {
                assert!(violation.contains("invalid_utf8"), "{violation}")
            }
            other => panic!("expected utf8 failure, got {other:?}"),
        }
        let mut deep = test_contract(json!({"type":"array","items":{"type":"string"}}), 1_000_000);
        deep.max_depth = 3;
        let body = "[[[[\"SENTINEL\"]]]]";
        let out = evaluate_tool_response(Some(&deep), body, &[]);
        match out {
            ToolOutcome::ContractFailedAfterExecution { violation, .. } => {
                assert!(violation.contains("max_depth_exceeded"), "{violation}");
                assert!(!violation.contains("SENTINEL"), "{violation}");
            }
            other => panic!("expected depth failure, got {other:?}"),
        }
        // Legacy path stays lossy/uncontracted on the same malformed bytes.
        assert!(matches!(
            evaluate_tool_response_bytes(None, b"\xff", &[]),
            ToolOutcome::Uncontracted(_)
        ));
    }

    #[test]
    fn legacy_invoke_tool_refuses_on_contract_failure() {
        use crate::vault::SetSecretArgs;
        let tmp = tempfile::TempDir::new().unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                if let Ok(req) = server.recv() {
                    let _ = req.respond(tiny_http::Response::from_string(
                        r#"{"id":42,"marker":"LEGACY_SENTINEL"}"#,
                    ));
                }
            }
        });
        let mut vault = Vault::open(tmp.path().join("v.enc"), [3u8; 32]).unwrap();
        vault
            .set(SetSecretArgs {
                name: "T".into(),
                value: "tok".into(),
                description: None,
                allowed_hosts: vec!["127.0.0.1".into()],
                allowed_methods: vec!["GET".into()],
                allowed_agents: vec![],
                allow_private_network: true,
                lease_ttl_secs: None,
            })
            .unwrap();
        let toml = format!(
            r#"
id = "svc"
title = "Svc"
base_url = "http://127.0.0.1:{port}"
allowed_hosts = ["127.0.0.1"]
default_headers = {{ Authorization = "Bearer {{{{FERROGATE:T}}}}" }}
[[tools]]
name = "get_one"
description = "Get"
method = "GET"
path = "/one"
[tools.response_contract]
max_response_bytes = 100000
max_depth = 32
max_array_len = 100
max_string_len = 10000
[tools.response_contract.schema]
type = "object"
additionalProperties = false
required = ["id"]
[tools.response_contract.schema.properties]
id = {{ type = "string" }}
"#
        );
        let m = ConnectorManifest::parse_toml(&toml, Path::new("svc.toml")).unwrap();
        for legacy in [true, false] {
            let res = if legacy {
                m.execute_tool(&vault, "get_one", &json!({}), None)
            } else {
                m.invoke_tool(&vault, "get_one", &json!({}), None)
            };
            let err = res.unwrap_err();
            assert!(matches!(err, GateError::ContractFailed { .. }), "{err}");
            assert!(!err.to_string().contains("LEGACY_SENTINEL"), "{err}");
        }
        handle.join().unwrap();
    }

    #[test]
    fn dotted_and_array_projection_paths() {
        let body = r#"{"user":{"name":"ada","pw":"x"},"items":[{"id":1,"v":9},{"id":2}],"top":1}"#;
        let fields = vec![
            "user.name".to_string(),
            "items[].id".to_string(),
            "missing.deep".to_string(),
        ];
        assert_eq!(
            project_response_body(body, &fields),
            json!({"user":{"name":"ada"},"items":[{"id":1},{"id":2}]})
        );
        // Plain fields and top-level arrays keep legacy behaviour.
        let arr = r#"[{"a":{"b":1,"c":2}}]"#;
        assert_eq!(
            project_response_body(arr, &["a.b".to_string()]),
            json!([{"a":{"b":1}}])
        );
        assert_eq!(
            project_response_body(body, &["top".to_string()]),
            json!({"top":1})
        );
    }

    #[test]
    fn response_contract_parses_from_toml() {
        let toml = r#"
id = "chat"
title = "Chat"
base_url = "https://api.example.com/v1"
[[tools]]
name = "get_one"
description = "Get"
method = "GET"
path = "/one"
[tools.response_contract]
max_response_bytes = 1000
max_depth = 32
max_array_len = 10
max_string_len = 100
[tools.response_contract.schema]
type = "object"
additionalProperties = false
[tools.response_contract.schema.properties]
id = { type = "integer" }
"#;
        let m = ConnectorManifest::parse_toml(toml, Path::new("c.toml")).unwrap();
        let c = m.tools[0].response_contract.as_ref().unwrap();
        assert_eq!(c.max_response_bytes, 1000);
    }

    #[test]
    fn contract_in_manifest_validation_names_contract_error() {
        let mut m = manifest("");
        let mut c = test_contract(json!({"type":"string"}), 100);
        c.max_response_bytes = 0;
        m.tools[0].response_contract = Some(c);
        let err = m.validate(Path::new("x.toml")).unwrap_err();
        assert!(matches!(err, GateError::InvalidConnector { .. }), "{err}");
        assert!(err.to_string().contains("contract_unenforceable"), "{err}");
    }

    #[test]
    fn ids_and_tool_names_must_be_unambiguous() {
        let mut m = manifest("");
        m.id = " chat".into();
        assert!(m.validate(Path::new("x")).is_err());
        m.id = "chat".into();
        m.tools.push(m.tools[0].clone());
        assert!(m.validate(Path::new("x")).is_err());
    }

    #[test]
    fn openapi_operations_that_sanitize_alike_get_distinct_tool_names() {
        let spec = r#"{"openapi":"3.0.0","info":{"title":"t","version":"1"},
            "servers":[{"url":"https://api.example.com"}],
            "paths":{"/a-b":{"get":{}},"/a_b":{"get":{}},"/a b":{"get":{}}}}"#;
        let m = import_openapi_to_connector(spec, "svc", None).unwrap();
        let mut names: Vec<_> = m.tools.iter().map(|t| t.name.clone()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 3, "{names:?}");
    }

    const VALID: &str = "id = \"ok\"\ntitle = \"Ok\"\nbase_url = \"https://api.example.com\"\n";

    #[cfg(unix)]
    fn tree_with_symlinked_manifest() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let key = tmp.path().join("vault.key");
        fs::write(&key, "0123456789abcdef-not-toml-master-key-material\n").unwrap();
        let gate = tmp.path().join("repo/.fl/gate");
        fs::create_dir_all(&gate).unwrap();
        fs::write(gate.join("good.toml"), VALID).unwrap();
        std::os::unix::fs::symlink(&key, gate.join("evil.toml")).unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let repo = tmp.path().join("repo");
        (tmp, repo, home)
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_manifest_is_skipped_with_a_warning_and_the_rest_still_load() {
        let (_tmp, repo, home) = tree_with_symlinked_manifest();
        let found = discover_connectors_with_warnings(Some(&repo), Some(&home)).unwrap();
        let ids: Vec<_> = found.connectors.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["ok"]);
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
        let w = &found.warnings[0];
        assert!(
            w.contains("evil.toml") && w.contains("symbolic link"),
            "{w}"
        );
        assert!(
            !w.contains("master-key-material") && !w.contains("vault.key"),
            "the link target must not appear: {w}"
        );
        // The plain entry point agrees and does not fail either.
        let plain = discover_connectors(Some(&repo), Some(&home)).unwrap();
        assert_eq!(plain, found.connectors);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_connector_directory_is_not_followed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(
            elsewhere.join("other-tool.toml"),
            "token = = \"hunter2-sentinel\"\n",
        )
        .unwrap();
        fs::write(elsewhere.join("good.toml"), VALID).unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".fl")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, repo.join(".fl/gate")).unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".config/fl")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, home.join(".config/fl/gate")).unwrap();

        let found = discover_connectors_with_warnings(Some(&repo), Some(&home)).unwrap();
        assert!(found.connectors.is_empty(), "{:?}", found.connectors);
        assert_eq!(found.warnings.len(), 2, "{:?}", found.warnings);
        for w in &found.warnings {
            assert!(w.contains("symbolic link") && !w.contains("hunter2"), "{w}");
        }

        // A project whose `.fl` is the link is refused the same way.
        let repo2 = tmp.path().join("repo2");
        fs::create_dir_all(&repo2).unwrap();
        fs::create_dir_all(tmp.path().join("real-fl")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.parent().unwrap().join("real-fl"),
            repo2.join(".fl"),
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("real-fl/gate")).unwrap();
        fs::write(tmp.path().join("real-fl/gate/good.toml"), VALID).unwrap();
        let found = discover_connectors_with_warnings(Some(&repo2), Some(&home)).unwrap();
        assert!(found.connectors.is_empty());
        assert!(
            found.warnings.iter().any(|w| w.contains(".fl")),
            "{:?}",
            found.warnings
        );
    }

    #[test]
    fn a_malformed_manifest_error_quotes_no_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let gate = tmp.path().join("repo/.fl/gate");
        fs::create_dir_all(&gate).unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        for (body, line) in [
            ("id = \"ok\"\ntitle = SOURCE-LINE-SENTINEL-1 nope\n", 2),
            ("id = 42424242\ntitle = \"t\"\nbase_url = \"u\"\n", 1),
            (
                "id = \"ok\"\ntitle = \"t\"\nbase_url = \"u\"\nSENTINEL-KEY = \"pasted-token-9\"\ntitle = \"x\"\n",
                5,
            ),
            ("pasted-token-9 = 1\n[tools]\n", 0),
        ] {
            fs::write(gate.join("bad.toml"), body).unwrap();
            let err =
                discover_connectors_with_warnings(Some(&tmp.path().join("repo")), Some(&home))
                    .unwrap_err();
            let text = err.to_string();
            assert!(matches!(err, GateError::InvalidConnector { .. }), "{text}");
            assert!(text.contains("line "), "{text}");
            if line > 0 {
                assert!(text.contains(&format!("line {line},")), "{text}");
            }
            for leaked in [
                "SOURCE-LINE-SENTINEL",
                "SENTINEL-KEY",
                "pasted-token-9",
                "42424242",
                " | ",
            ] {
                assert!(!text.contains(leaked), "`{leaked}` leaked: {text}");
            }
        }
    }
}
