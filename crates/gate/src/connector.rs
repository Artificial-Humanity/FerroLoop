//! Declarative API-to-MCP connector manifests (`.fl/gate/*.toml` & `~/.config/fl/gate/*.toml`),
//! OpenAPI 3.x importer (`import_openapi_to_connector`), and tool invocation execution (spec §4).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::GateError;
use crate::bridge::BridgeConfig;
use crate::proxy::{PLACEHOLDER_PREFIX, ProxyRequest, ProxyResponse, execute_proxied_request};
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
}

fn default_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {}
    })
}

impl ConnectorManifest {
    /// Parse and validate a [`ConnectorManifest`] from TOML text.
    pub fn parse_toml(raw: &str, source_path: &Path) -> Result<Self, GateError> {
        let manifest: Self = toml::from_str(raw).map_err(|e| GateError::InvalidConnector {
            path: source_path.to_path_buf(),
            reason: e.to_string(),
        })?;
        manifest.validate(source_path)?;
        Ok(manifest)
    }

    pub fn validate(&self, source_path: &Path) -> Result<(), GateError> {
        // Checked as written: a padded id passed `trim()` yet was stored, matched, and published
        // with its padding.
        let id = self.id.as_str();
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
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

    /// Execute a declared tool against `vault` and apply optional `response_fields` projection.
    pub fn execute_tool(
        &self,
        vault: &Vault,
        tool_name: &str,
        arguments: &Value,
        caller_agent: Option<String>,
    ) -> Result<(ProxyResponse, Value), GateError> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.name == tool_name)
            .ok_or_else(|| GateError::NoSuchTool {
                tool: tool_name.to_string(),
            })?;
        let req = self.build_tool_request(tool, arguments, caller_agent)?;
        let resp = execute_proxied_request(vault, &req)?;
        let projected = project_response_body(&resp.body, &tool.response_fields);
        Ok((resp, projected))
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
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for f in fields {
                if let Some(v) = map.get(f) {
                    out.insert(f.clone(), v.clone());
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

/// Discover all `.fl/gate/*.toml` (project) and `~/.config/fl/gate/*.toml` (user) connector manifests.
/// Project-level connectors take precedence over user-level connectors with the same `id`.
pub fn discover_connectors(
    project_root: Option<&Path>,
    home_override: Option<&Path>,
) -> Result<Vec<ConnectorManifest>, GateError> {
    let mut by_id = BTreeMap::new();

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

    if user_dir.is_dir() {
        for manifest in load_connectors_from_dir(&user_dir)? {
            by_id.insert(manifest.id.clone(), manifest);
        }
    }

    if let Some(root) = project_root {
        let proj_dir = root.join(".fl").join("gate");
        if proj_dir.is_dir() {
            for manifest in load_connectors_from_dir(&proj_dir)? {
                by_id.insert(manifest.id.clone(), manifest);
            }
        }
    }

    Ok(by_id.into_values().collect())
}

fn load_connectors_from_dir(dir: &Path) -> Result<Vec<ConnectorManifest>, GateError> {
    let mut paths = Vec::new();
    let entries = fs::read_dir(dir).map_err(|e| GateError::Io {
        op: "read_dir",
        path: dir.to_path_buf(),
        cause: e.to_string(),
    })?;
    for entry in entries.flatten() {
        let p = entry.path();
        // `symlink_metadata` so a link is still listed, and then refused, rather than skipped.
        let is_manifest_file =
            fs::symlink_metadata(&p).is_ok_and(|m| m.is_file() || m.file_type().is_symlink());
        if is_manifest_file && p.extension().and_then(|s| s.to_str()) == Some("toml") {
            paths.push(p);
        }
    }
    paths.sort();

    let mut manifests = Vec::with_capacity(paths.len());
    for p in paths {
        // `toml` quotes the offending line in its error, so a manifest symlinked to a key file
        // would print that file. The vault refuses symlinks for the same reason.
        crate::vault::reject_if_symlink(&p)?;
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

    #[cfg(unix)]
    #[test]
    fn a_manifest_symlink_is_refused_and_its_target_is_never_quoted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let key = tmp.path().join("vault.key");
        fs::write(&key, "0123456789abcdef-not-toml-master-key-material\n").unwrap();
        let gate = tmp.path().join("repo/.fl/gate");
        fs::create_dir_all(&gate).unwrap();
        std::os::unix::fs::symlink(&key, gate.join("evil.toml")).unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();

        let err = discover_connectors(Some(&tmp.path().join("repo")), Some(&home)).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, GateError::Io { .. }), "{text}");
        assert!(!text.contains("master-key-material"), "{text}");
    }
}
