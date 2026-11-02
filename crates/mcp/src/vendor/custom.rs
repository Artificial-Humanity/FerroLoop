//! MCP targets for vendors beyond the five built in: a plugin vendor declares where its MCP file
//! is and how its entries look, once, and `fl-mcp` can render and open it like a built-in.
//!
//! A [`CustomTarget`] is declarative: a project-relative file, a JSON object key that holds the
//! servers (`mcpServers` for most agents), and whether the vendor expands `${VAR}` in `env` and
//! headers. Its entries have the shape Claude Code's `.mcp.json` uses. A vendor whose file is
//! TOML, or shaped differently, is not covered by this registry.
//!
//! ⚠ These targets are looked up and rendered here; `fl mcp sync` still plans and writes only
//! the five built-in vendors, because its plan, ownership records and `vendors` filter are keyed
//! on the closed [`VendorName`](crate::catalog::VendorName). Wiring custom targets into `sync`
//! is a separate change.

use super::Kind;
use super::{Fields, FileRefusal, Rendered, VendorFile, args, command, env, headers, json, url};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport};
use fl_core::vendor::VendorRegistry;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// A vendor's MCP file, declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomTarget {
    id: String,
    title: String,
    target: String,
    servers_key: &'static str,
    expands_env_refs: bool,
}

/// Why a target was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    #[error("`{0}` is a built-in vendor; its MCP file cannot be redeclared")]
    BuiltIn(String),
    #[error("an MCP target for `{0}` is already registered")]
    Duplicate(String),
    #[error("vendor `{0}` is not in the vendor registry; register it with `register_vendor` first")]
    UnknownVendor(String),
    #[error("vendor `{0}` declares no `mcp_config_rel_path`, so it has no MCP file")]
    NoMcpPath(String),
    #[error("MCP target `{id}`: {problem}")]
    Invalid { id: String, problem: String },
}

/// A server one custom target cannot run as the catalog gives it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{title} cannot run server `{server}`: {reason}")]
pub struct CustomRefusal {
    pub id: String,
    pub title: String,
    pub server: String,
    /// The reason, as a clause.
    pub reason: String,
}

/// Servers keys live for the whole process: [`json::open_with`] takes a `'static` key, and a
/// registry's worth of keys is a handful, interned once each.
fn intern(key: &str) -> &'static str {
    static KEYS: OnceLock<Mutex<BTreeSet<&'static str>>> = OnceLock::new();
    let mut keys = KEYS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = keys.get(key) {
        return existing;
    }
    let leaked: &'static str = Box::leak(key.to_string().into_boxed_str());
    keys.insert(leaked);
    leaked
}

impl CustomTarget {
    /// `target` is the file relative to the project root, `servers_key` the JSON object that holds
    /// the servers, and `expands_env_refs` whether the vendor expands `${VAR}` (a secret can only
    /// be written as such a reference, never as its value).
    pub fn new(
        id: &str,
        title: &str,
        target: &str,
        servers_key: &str,
        expands_env_refs: bool,
    ) -> Result<Self, TargetError> {
        let id = id.trim().to_ascii_lowercase();
        let bad = |problem: &str| TargetError::Invalid {
            id: id.clone(),
            problem: problem.to_string(),
        };
        if id.is_empty() || title.trim().is_empty() {
            return Err(bad("the id and the title must not be empty"));
        }
        let escapes = target.trim().is_empty()
            || target.starts_with('/')
            || target.starts_with('\\')
            || target.split(['/', '\\']).any(|p| p == ".." || p.is_empty());
        if escapes {
            return Err(bad("the file must be a path inside the project"));
        }
        if servers_key.is_empty()
            || !servers_key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        {
            return Err(bad("the servers key must be letters, digits, `_` or `-`"));
        }
        Ok(Self {
            id,
            title: title.trim().to_string(),
            target: target.to_string(),
            servers_key: intern(servers_key),
            expands_env_refs,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// The vendor's name for a person.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The file, relative to the project root.
    pub fn target(&self) -> &str {
        &self.target
    }

    fn refuse(&self, server: &str, reason: String) -> CustomRefusal {
        CustomRefusal {
            id: self.id.clone(),
            title: self.title.clone(),
            server: server.to_string(),
            reason,
        }
    }

    /// The entry exactly as fl writes it for the server `name`, or why this vendor cannot run it.
    pub fn render(&self, name: &str, server: &Server) -> Result<Rendered, CustomRefusal> {
        let mut fields = Fields::default();
        match server.transport {
            Transport::Stdio => {
                fields.text("command", command(server));
                fields.list("args", args(server));
                let mut entries = Vec::new();
                for (key, value) in env(server) {
                    let value = match value {
                        EnvValue::Literal(v) => v.clone(),
                        EnvValue::Secret { .. } if self.expands_env_refs => {
                            let var = value.secret_var(key).expect("a secret names a variable");
                            format!("${{{var}}}")
                        }
                        EnvValue::Secret { .. } => {
                            return Err(self.refuse(name, self.no_refs(key)));
                        }
                    };
                    entries.push((key.clone(), value));
                }
                fields.map("env", entries);
            }
            Transport::Http | Transport::Sse => {
                fields.text("type", server.transport.as_str());
                fields.text("url", url(server));
                let mut entries = Vec::new();
                for (header, value) in headers(server) {
                    let value = match value {
                        HeaderValue::Literal(v) => v.clone(),
                        HeaderValue::Secret { .. } if !self.expands_env_refs => {
                            return Err(self.refuse(name, self.no_refs(header)));
                        }
                        HeaderValue::Secret { env, scheme: None } => format!("${{{env}}}"),
                        HeaderValue::Secret {
                            env,
                            scheme: Some(scheme),
                        } => format!("{scheme} ${{{env}}}"),
                    };
                    entries.push((header.clone(), value));
                }
                fields.map("headers", entries);
            }
        }
        Ok(json::render(&fields.0))
    }

    fn no_refs(&self, key: &str) -> String {
        format!(
            "`{key}` is a secret, and {} does not expand `${{VAR}}`, so it could only be written \
             as its value, which fl never does",
            self.title
        )
    }

    /// The file as it stands: `None` when it does not exist yet. `path` only names the file in a
    /// refusal.
    pub fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal> {
        let file = json::open_with(path, bytes, self.servers_key, &[])?;
        Ok(VendorFile {
            kind: Kind::Json(file),
        })
    }
}

type Targets = RwLock<BTreeMap<String, Arc<CustomTarget>>>;

fn targets() -> &'static Targets {
    static TARGETS: OnceLock<Targets> = OnceLock::new();
    TARGETS.get_or_init(Default::default)
}

/// Registers a target. Refused for a built-in vendor's id and for an id already registered.
pub fn register_target(target: CustomTarget) -> Result<(), TargetError> {
    if VendorRegistry::is_builtin(&target.id) {
        return Err(TargetError::BuiltIn(target.id));
    }
    let mut map = targets().write().unwrap_or_else(|e| e.into_inner());
    if map.contains_key(&target.id) {
        return Err(TargetError::Duplicate(target.id));
    }
    map.insert(target.id.clone(), Arc::new(target));
    Ok(())
}

/// Registers the MCP target of a vendor already in the vendor registry, taking its name and its
/// file (`mcp_config_rel_path`) from the descriptor, so the vendor is declared in one place.
pub fn register_target_for_vendor(
    vendor_id: &str,
    servers_key: &str,
    expands_env_refs: bool,
) -> Result<(), TargetError> {
    if VendorRegistry::is_builtin(vendor_id) {
        return Err(TargetError::BuiltIn(vendor_id.trim().to_ascii_lowercase()));
    }
    let descriptor = VendorRegistry::global()
        .get(vendor_id)
        .ok_or_else(|| TargetError::UnknownVendor(vendor_id.to_string()))?;
    let path = descriptor
        .mcp_config_rel_path
        .as_deref()
        .ok_or_else(|| TargetError::NoMcpPath(descriptor.id.clone()))?;
    register_target(CustomTarget::new(
        &descriptor.id,
        &descriptor.display_name,
        path,
        servers_key,
        expands_env_refs,
    )?)
}

/// The registered target with this id.
pub fn custom_target(id: &str) -> Option<Arc<CustomTarget>> {
    targets()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id.trim().to_ascii_lowercase())
        .cloned()
}

/// Every registered target, by id.
pub fn custom_targets() -> Vec<Arc<CustomTarget>> {
    targets()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::Claude;
    use crate::vendor::fixtures::*;
    use fl_core::vendor::{VendorDescriptor, VendorTransport, lookup_vendor, register_vendor};

    fn claude_shaped(id: &str, expands: bool) -> CustomTarget {
        CustomTarget::new(id, "Plugin", ".plugin/mcp.json", "mcpServers", expands).unwrap()
    }

    fn written(target: &CustomTarget, name: &str, body: &str) -> String {
        let entry = target.render(name, &server(body)).unwrap();
        let mut file = target.open(Path::new("f"), None).unwrap();
        file.set(name, &entry);
        String::from_utf8(file.to_bytes()).unwrap()
    }

    #[test]
    fn a_target_that_expands_references_writes_what_claude_code_writes() {
        // The shape is Claude Code's, so the two must not drift apart.
        let t = claude_shaped("shape-check", true);
        for body in [
            STDIO,
            ONLY_SECRET,
            RENAMED,
            BEARER,
            WHOLE,
            PLAIN_REMOTE,
            SSE,
        ] {
            assert_eq!(
                written(&t, "files", body),
                file_with(&Claude, "files", body),
                "{body}"
            );
        }
    }

    #[test]
    fn a_target_that_does_not_expand_references_refuses_a_secret_and_keeps_the_rest() {
        let t = claude_shaped("no-refs", false);
        let err = t.render("files", &server(STDIO)).unwrap_err();
        assert!(err.reason.contains("FILES_TOKEN"), "{err}");
        assert!(err.reason.contains("never does"), "{err}");
        assert_eq!(err.server, "files");
        assert!(t.render("gh", &server(BEARER)).is_err());
        // Literals are fine.
        assert!(written(&t, "docs", PLAIN_REMOTE).contains("https://mcp.example.com/mcp"));
    }

    #[test]
    fn another_servers_key_is_honoured_and_foreign_entries_are_kept() {
        let t = CustomTarget::new("keyed", "Keyed", "k/cfg.json", "tools", true).unwrap();
        let existing = br#"{"theme":"dark","tools":{"theirs":{"command":"x"}}}"#;
        let mut file = t.open(Path::new("k/cfg.json"), Some(existing)).unwrap();
        assert_eq!(file.names(), vec!["theirs".to_string()]);
        file.set("files", &t.render("files", &server(STDIO)).unwrap());
        let text = String::from_utf8(file.to_bytes()).unwrap();
        assert!(text.contains("\"theme\": \"dark\""), "{text}");
        assert!(text.contains("\"theirs\""), "{text}");
        assert!(text.contains("\"tools\""), "{text}");
        assert_eq!(
            file.names(),
            vec!["theirs".to_string(), "files".to_string()]
        );
        // The entry reads back as the bytes fl rendered.
        assert_eq!(
            file.entry("files").unwrap(),
            t.render("files", &server(STDIO)).unwrap().bytes()
        );
        // A file fl cannot rewrite faithfully is refused, not clobbered.
        assert!(t.open(Path::new("f"), Some(b"{ // comment\n}")).is_err());
    }

    #[test]
    fn a_target_must_name_a_file_inside_the_project_and_a_plain_key() {
        for (path, key) in [
            ("../x.json", "mcpServers"),
            ("/etc/x.json", "mcpServers"),
            ("a//b.json", "mcpServers"),
            ("", "mcpServers"),
            ("ok.json", ""),
            ("ok.json", "bad key"),
        ] {
            assert!(
                matches!(
                    CustomTarget::new("x", "X", path, key, true),
                    Err(TargetError::Invalid { .. })
                ),
                "{path:?} {key:?}"
            );
        }
        assert!(CustomTarget::new("", "X", "a.json", "k", true).is_err());
        assert!(CustomTarget::new("x", " ", "a.json", "k", true).is_err());
    }

    #[test]
    fn a_vendor_declares_its_mcp_target_once_through_the_vendor_registry() {
        let id = "mcp-test-plugin";
        let mut d: VendorDescriptor = lookup_vendor("ollama").unwrap();
        d.id = id.into();
        d.display_name = "Plugin Agent".into();
        d.transport = VendorTransport::SubscriptionCli {
            default_binary: "plugin-agent".into(),
        };
        d.mcp_config_rel_path = Some(".plugin/mcp.json".into());
        register_vendor(d).unwrap();
        register_target_for_vendor(id, "mcpServers", true).unwrap();
        let t = custom_target(id).expect("registered");
        assert_eq!(
            (t.title(), t.target()),
            ("Plugin Agent", ".plugin/mcp.json")
        );
        assert!(custom_targets().iter().any(|t| t.id() == id));
        assert_eq!(
            register_target_for_vendor(id, "mcpServers", true),
            Err(TargetError::Duplicate(id.into()))
        );
    }

    #[test]
    fn a_built_in_unknown_or_pathless_vendor_gets_no_target() {
        assert_eq!(
            register_target(claude_shaped("claude", true)),
            Err(TargetError::BuiltIn("claude".into()))
        );
        assert_eq!(
            register_target_for_vendor("kimi-api", "mcpServers", true),
            Err(TargetError::BuiltIn("kimi-api".into()))
        );
        assert_eq!(
            register_target_for_vendor("nobody-registered-this", "mcpServers", true),
            Err(TargetError::UnknownVendor("nobody-registered-this".into()))
        );
        let mut d = lookup_vendor("ollama").unwrap();
        d.id = "mcp-test-pathless".into();
        register_vendor(d).unwrap();
        assert_eq!(
            register_target_for_vendor("mcp-test-pathless", "mcpServers", true),
            Err(TargetError::NoMcpPath("mcp-test-pathless".into()))
        );
        // The built-in writers are untouched.
        assert!(custom_target("claude").is_none());
    }
}
