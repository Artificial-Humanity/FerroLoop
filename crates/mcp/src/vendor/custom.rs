//! MCP targets for vendors beyond the five built in: a plugin vendor declares where its MCP file
//! is and how its entries look, once, and `fl-mcp` can render and open it like a built-in.
//!
//! A [`CustomTarget`] is declarative: a project-relative file, a JSON object key that holds the
//! servers (`mcpServers` for most agents), and whether the vendor expands `${VAR}` in `env` and
//! headers. Its entries have the shape Claude Code's `.mcp.json` uses. A vendor whose file is
//! TOML, or shaped differently, is not covered by this registry.
//!
//! A registered target is a vendor like the five built in. [`VendorName::Custom`] names it, so
//! `fl mcp sync` plans and writes its file with the same ownership records, refusals, race
//! detection and atomic write as a built-in's, and a catalog server's `vendors` list may name it.
//! [`register_target`] is the only way to make such a name, which is what lets
//! [`vendor`](super::vendor) answer for every name without a failure case. Targets are never
//! unregistered.

use super::Kind;
use super::{
    Fields, FileRefusal, Rendered, Vendor, VendorFile, VendorRefusal, args, command, env, headers,
    json, url,
};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, VendorName};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock, RwLock};

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
    #[error("MCP target `{id}`: {problem}")]
    Invalid { id: String, problem: String },
}

/// A server one custom target cannot run as the catalog gives it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{title} cannot run server `{server}`: {reason}. It is left out of {file}; the other vendors \
     still get it. To say so in the catalog, give the server a `vendors` list without `{id}`"
)]
pub struct CustomRefusal {
    pub id: String,
    pub title: String,
    /// The target's file, relative to the project root.
    pub file: String,
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

/// A project-relative file as a key for "is this the same file": `\` read as `/`, `.` and empty
/// parts dropped (`./a//b` is `a/b`), and the case folded, since the file system may not tell
/// `.MCP.json` from `.mcp.json`. Two targets whose keys are equal are taken to write one file.
pub fn target_file_key(rel: &str) -> String {
    rel.split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join("/")
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
            file: self.target.clone(),
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

/// A registered target as a vendor: the target, and the name that stands for it.
struct Registered {
    target: CustomTarget,
    name: VendorName,
}

impl Vendor for Registered {
    fn name(&self) -> VendorName {
        self.name
    }

    fn title(&self) -> &str {
        self.target.title()
    }

    fn target(&self) -> &str {
        self.target.target()
    }

    fn render(&self, name: &str, server: &Server) -> Result<Rendered, VendorRefusal> {
        (self.target.render(name, server)).map_err(|r| VendorRefusal {
            vendor: self.name,
            server: r.server,
            reason: r.reason,
        })
    }

    fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal> {
        self.target.open(path, bytes)
    }
}

/// The name of a registered custom target: [`VendorName::Custom`] holds one. Only
/// [`register_target`] and [`VendorName::parse`] make one, and only for an id in the registry,
/// which never forgets a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CustomId(&'static str);

impl CustomId {
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

type Targets = RwLock<BTreeMap<String, &'static Registered>>;

fn targets() -> &'static Targets {
    static TARGETS: OnceLock<Targets> = OnceLock::new();
    TARGETS.get_or_init(Default::default)
}

/// Registers a target. Refused for a built-in vendor's id and for an id already registered.
///
/// `expands_env_refs` is the caller's choice: whether the vendor's CLI expands `${VAR}`, which
/// decides whether a secret server can be written to its file at all (as a reference). `fl`
/// itself declares the targets of config-defined vendors with `false`, so a secret server is
/// left out of such files; pass `true` only for a CLI known to expand references.
///
/// This does not check that the file is not another target's. The planner (`sync::plan`) refuses
/// two targets that resolve to one file, and a caller that declares targets checks first with
/// [`target_file_key`].
///
/// `fl-mcp` does not depend on `fl-core`'s vendor registry (the architecture check forbids it), so
/// the caller that holds both declares a registered vendor's target from its descriptor, e.g.
/// `CustomTarget::new(&d.id, &d.display_name, d.mcp_config_rel_path.as_deref()?, "mcpServers", false)`.
pub fn register_target(target: CustomTarget) -> Result<(), TargetError> {
    if VendorName::ALL.iter().any(|v| v.as_str() == target.id) {
        return Err(TargetError::BuiltIn(target.id));
    }
    let mut map = targets().write().unwrap_or_else(|e| e.into_inner());
    if map.contains_key(&target.id) {
        return Err(TargetError::Duplicate(target.id));
    }
    let id = target.id.clone();
    // Leaked: a target lives as long as the process, like its interned key.
    let name = VendorName::Custom(CustomId(intern(&id)));
    map.insert(id, Box::leak(Box::new(Registered { target, name })));
    Ok(())
}

/// The registered target with this id.
pub fn custom_target(id: &str) -> Option<&'static CustomTarget> {
    let map = targets().read().unwrap_or_else(|e| e.into_inner());
    map.get(&id.trim().to_ascii_lowercase()).map(|r| &r.target)
}

/// Every registered target, by id.
pub fn custom_targets() -> Vec<&'static CustomTarget> {
    let map = targets().read().unwrap_or_else(|e| e.into_inner());
    map.values().map(|r| &r.target).collect()
}

/// The name of the registered target with exactly this id.
pub(crate) fn custom_name(id: &str) -> Option<VendorName> {
    let map = targets().read().unwrap_or_else(|e| e.into_inner());
    map.get(id).map(|r| r.name)
}

/// The registered target's vendor.
pub(crate) fn custom_vendor(id: CustomId) -> &'static dyn Vendor {
    let map = targets().read().unwrap_or_else(|e| e.into_inner());
    *map.get(id.0).expect("a CustomId names a registered target")
}

/// The names of every registered target, by id.
pub(crate) fn custom_names() -> Vec<VendorName> {
    let map = targets().read().unwrap_or_else(|e| e.into_inner());
    map.values().map(|r| r.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::Claude;
    use crate::vendor::fixtures::*;

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
    fn a_registered_target_is_found_by_id_and_never_twice() {
        let id = "mcp-test-plugin";
        register_target(
            CustomTarget::new(id, "Plugin Agent", ".plugin/mcp.json", "mcpServers", true).unwrap(),
        )
        .unwrap();
        let t = custom_target(" MCP-Test-Plugin ").expect("registered");
        assert_eq!(
            (t.title(), t.target()),
            ("Plugin Agent", ".plugin/mcp.json")
        );
        assert!(custom_targets().iter().any(|t| t.id() == id));
        assert_eq!(
            register_target(claude_shaped(id, true)),
            Err(TargetError::Duplicate(id.into()))
        );
    }

    #[test]
    fn a_built_in_vendor_gets_no_custom_target() {
        for v in VendorName::ALL {
            assert_eq!(
                register_target(claude_shaped(v.as_str(), true)),
                Err(TargetError::BuiltIn(v.as_str().into()))
            );
            assert!(custom_target(v.as_str()).is_none());
        }
    }
}
