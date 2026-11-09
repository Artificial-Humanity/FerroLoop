//! Codex's `<root>/.codex/config.toml`, edited with `toml_edit` so that its
//! comments and layout survive (MCP spec §4.3). Each server is a
//! `[mcp_servers.<name>]` table. Codex expands no `${NAME}`: a secret
//! variable is forwarded by name (`env_vars`), a secret header is read whole
//! from its variable (`env_http_headers`) or is the Bearer token
//! (`bearer_token_env_var`). Codex speaks streamable HTTP only, and has no
//! `type` field.

use super::{Field, Fields, FileRefusal, Rendered, Vendor, VendorFile, VendorRefusal};
use super::{Kind, args, command, env, headers, refuse, url};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, VendorName};
use std::path::{Path, PathBuf};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

const SERVERS: &str = "mcp_servers";

/// Canonical launch directory and the project entry that granted trust.
#[derive(Debug)]
pub struct CodexProjectTrust {
    pub cwd: PathBuf,
    pub trust_key: PathBuf,
}

/// Safe, actionable refusal; never contains configuration text.
#[derive(Debug, thiserror::Error)]
#[error(
    "Codex trust check for {cwd}: {reason} ({config_path}). To trust this project, add or correct in {config_path}:\n[projects.{key}]\ntrust_level = \"trusted\""
)]
pub struct CodexTrustError {
    cwd: PathBuf,
    config_path: PathBuf,
    key: toml::Value,
    reason: String,
}

/// Check explicit inputs without reading or changing process environment.
pub fn check_codex_project_trust(
    cwd: &Path,
    config_path: &Path,
) -> Result<CodexProjectTrust, CodexTrustError> {
    let mut error = CodexTrustError {
        cwd: cwd.into(),
        config_path: config_path.into(),
        key: toml::Value::String(cwd.display().to_string()),
        reason: String::new(),
    };
    let mut check = || -> Result<CodexProjectTrust, String> {
        let cwd = cwd
            .canonicalize()
            .map_err(|_| "cwd cannot be resolved".to_string())?;
        if !cwd.is_dir() {
            return Err("cwd is not a directory".into());
        }
        error.cwd = cwd.clone();
        let mut project: &Path = &cwd;
        let mut marker_exists = false;
        for ancestor in cwd.ancestors() {
            match ancestor.join(".git").symlink_metadata() {
                Ok(_) => {
                    project = ancestor;
                    marker_exists = true;
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("cannot inspect Git directory marker".into()),
            }
        }
        error.key = toml::Value::String(project.display().to_string());
        let mut keys = vec![project.to_path_buf()];
        let marker = project.join(".git");
        if marker_exists && !marker.is_dir() {
            let text = read_trust_file(&marker, 8192)
                .map_err(|why| format!("Git directory marker: {why}"))?;
            let gitdir = text
                .strip_prefix("gitdir:")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("invalid Git directory marker")?;
            let gitdir = project
                .join(gitdir)
                .canonicalize()
                .map_err(|_| "cannot resolve Git directory")?;
            if !gitdir.is_dir() {
                return Err("Git directory is not a directory".into());
            }
            let common_marker = gitdir.join("commondir");
            match common_marker.symlink_metadata() {
                // Submodules and --separate-git-dir repositories do not need
                // commondir. Its absence leaves only the explicit project key.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("cannot inspect Git common directory marker".into()),
                Ok(_) => {
                    // Inspect without following symlinks above, so a dangling
                    // marker is refused rather than treated as absent.
                    let common = read_trust_file(&common_marker, 8192)
                        .map_err(|why| format!("Git common directory marker: {why}"))?;
                    if common.trim().is_empty() {
                        return Err("invalid Git common directory".into());
                    }
                    let common = gitdir
                        .join(common.trim())
                        .canonicalize()
                        .map_err(|_| "cannot resolve Git common directory")?;
                    if !common.is_dir() {
                        return Err("Git common directory is not a directory".into());
                    }
                    let main = common
                        .parent()
                        .ok_or("Git common directory has no parent")?;
                    if main != project {
                        keys.push(main.into());
                    }
                }
            }
        }
        let text = read_trust_file(config_path, 1024 * 1024)?;
        let config = text.parse::<toml::Table>().map_err(|e| {
            let at = e.span().map_or(0, |s| s.start).min(text.len());
            let line = text.as_bytes()[..at]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
                + 1;
            format!("is not valid TOML (line {line})")
        })?;
        let projects = config.get("projects").and_then(toml::Value::as_table);
        for key in keys {
            if let Some(entry) = key.to_str().and_then(|key| projects?.get(key)) {
                if entry.get("trust_level").and_then(toml::Value::as_str) == Some("trusted") {
                    return Ok(CodexProjectTrust {
                        cwd,
                        trust_key: key,
                    });
                }
                break;
            }
        }
        Err("does not trust this project".into())
    };
    check().map_err(|reason| {
        error.reason = reason;
        error
    })
}

/// Metadata prevents opening devices/FIFOs; the limited reader also detects growth or
/// understated metadata, without ever allocating an unbounded configuration string.
fn read_trust_file(path: &Path, limit: u64) -> Result<String, String> {
    use std::io::Read;
    let unreadable = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "input does not exist".to_string()
        } else {
            "fl could not read input".to_string()
        }
    };
    let metadata = path.metadata().map_err(unreadable)?;
    if !metadata.is_file() {
        return Err("fl could not read input: not a regular file".into());
    }
    if metadata.len() > limit {
        return Err(format!("input exceeds {limit} byte limit"));
    }
    let file = std::fs::File::open(path).map_err(unreadable)?;
    if !file.metadata().map_err(unreadable)?.is_file() {
        return Err("input is not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > limit {
        return Err(format!("input exceeds {limit} byte limit"));
    }
    String::from_utf8(bytes).map_err(|_| "input is not UTF-8".into())
}

pub struct Codex;

impl Vendor for Codex {
    fn name(&self) -> VendorName {
        VendorName::Codex
    }

    fn title(&self) -> &'static str {
        "Codex"
    }

    fn target(&self) -> &'static str {
        ".codex/config.toml"
    }

    fn render(&self, name: &str, server: &Server) -> Result<Rendered, VendorRefusal> {
        let refuse = |reason: String| refuse(VendorName::Codex, name, reason);
        let mut fields = Fields::default();
        match server.transport {
            Transport::Stdio => {
                fields.text("command", command(server));
                fields.list("args", args(server));
                let (mut literals, mut forwarded) = (Vec::new(), Vec::new());
                for (key, value) in env(server) {
                    match value {
                        EnvValue::Literal(v) => literals.push((key.clone(), v.clone())),
                        EnvValue::Secret { .. } => {
                            let var = value.secret_var(key).expect("a secret names a variable");
                            if var != key {
                                return Err(refuse(format!(
                                    "`env.{key}` reads the variable `{var}`, and Codex forwards \
                                     a secret variable under its own name"
                                )));
                            }
                            forwarded.push(var.to_string());
                        }
                    }
                }
                fields.map("env", literals);
                fields.list("env_vars", forwarded);
            }
            Transport::Sse => {
                return Err(refuse(
                    "Codex connects to streamable HTTP servers only, not SSE".into(),
                ));
            }
            Transport::Http => {
                fields.text("url", url(server));
                let (mut literals, mut from_env, mut bearer) = (Vec::new(), Vec::new(), None);
                for (header, value) in headers(server) {
                    match value {
                        HeaderValue::Literal(v) => literals.push((header.clone(), v.clone())),
                        HeaderValue::Secret { env, scheme: None } => {
                            from_env.push((header.clone(), env.clone()));
                        }
                        HeaderValue::Secret {
                            env,
                            scheme: Some(scheme),
                        } if header.eq_ignore_ascii_case("authorization")
                            && scheme.eq_ignore_ascii_case("bearer") =>
                        {
                            bearer = Some(env);
                        }
                        HeaderValue::Secret {
                            scheme: Some(scheme),
                            ..
                        } => {
                            return Err(refuse(format!(
                                "the secret header `{header}` has the scheme `{scheme}`, and \
                                 Codex sends a secret header either whole or as \
                                 `Authorization: Bearer`"
                            )));
                        }
                    }
                }
                if let Some(env) = bearer {
                    fields.text("bearer_token_env_var", env);
                }
                fields.map("http_headers", literals);
                fields.map("env_http_headers", from_env);
            }
        }
        let mut table = Table::new();
        for (key, field) in fields.0 {
            let value = match field {
                Field::Text(s) => Value::from(s),
                Field::List(values) => Value::Array(values.into_iter().collect::<Array>()),
                Field::Map(values) => {
                    Value::InlineTable(values.into_iter().collect::<InlineTable>())
                }
            };
            table.insert(key, Item::Value(value));
        }
        Ok(Rendered {
            text: alone(name, &Item::Table(table)),
        })
    }

    fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal> {
        let kind = Kind::Toml(open(path, bytes)?);
        Ok(VendorFile { kind })
    }
}

/// One entry as a file of its own: `[mcp_servers.<name>]` and its body,
/// without what stands above its header. This is both what fl writes and
/// what it reads back, so the two can be compared.
pub(super) fn alone(name: &str, entry: &Item) -> String {
    let mut entry = entry.clone();
    if let Item::Table(t) = &mut entry {
        t.decor_mut().clear();
    }
    let mut servers = Table::new();
    servers.set_implicit(true);
    servers.insert(name, entry);
    let mut doc = DocumentMut::new();
    doc.insert(SERVERS, Item::Table(servers));
    doc.to_string()
}

#[derive(Debug, Clone)]
pub(super) struct TomlFile {
    doc: DocumentMut,
}

pub(super) fn open(path: &Path, bytes: Option<&[u8]>) -> Result<TomlFile, FileRefusal> {
    let Some(bytes) = bytes else {
        return Ok(TomlFile {
            doc: DocumentMut::new(),
        });
    };
    let refuse = |problem: String| FileRefusal {
        path: path.to_path_buf(),
        problem,
        next: "Fix it by hand, then run `fl mcp sync` again".into(),
    };
    let text =
        std::str::from_utf8(bytes).map_err(|e| refuse(format!("it is not valid TOML ({e})")))?;
    // The parser's own message quotes the line; a line can hold a value, so
    // only its line number and message are shown.
    let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        let at = e.span().map_or(0, |s| s.start);
        let line = text[..at].matches('\n').count() + 1;
        refuse(format!(
            "it is not valid TOML (line {line}: {})",
            e.message().trim_end()
        ))
    })?;
    if doc.get(SERVERS).is_some_and(|item| !item.is_table()) {
        return Err(refuse(format!("`{SERVERS}` is not a table")));
    }
    Ok(TomlFile { doc })
}

impl TomlFile {
    fn servers(&self) -> Option<&Table> {
        self.doc.get(SERVERS).and_then(Item::as_table)
    }

    /// `mcp_servers`, added with no header of its own when the file has none.
    fn servers_mut(&mut self) -> &mut Table {
        if !self.doc.contains_key(SERVERS) {
            let mut servers = Table::new();
            servers.set_implicit(true);
            self.doc.insert(SERVERS, Item::Table(servers));
        }
        self.doc[SERVERS]
            .as_table_mut()
            .expect("checked when the file was read")
    }

    pub(super) fn names(&self) -> Vec<String> {
        let servers = self.servers().into_iter();
        servers
            .flat_map(|s| s.iter().map(|(name, _)| name.to_string()))
            .collect()
    }

    pub(super) fn entry(&self, name: &str) -> Option<Vec<u8>> {
        let entry = self.servers()?.get(name)?;
        Some(alone(name, entry).into_bytes())
    }

    pub(super) fn set(&mut self, name: &str, entry: &Rendered) {
        let mut doc: DocumentMut = entry.text.parse().expect("fl renders valid TOML");
        let mut new = std::mem::take(&mut doc[SERVERS][name])
            .into_table()
            .expect("fl renders a table");
        let first = self.doc.as_table().is_empty();
        let servers = self.servers_mut();
        match servers.get(name) {
            // In place, under the same lines above it.
            Some(Item::Table(old)) => {
                new.set_position(old.position());
                *new.decor_mut() = old.decor().clone();
            }
            // Last among the servers, a blank line above it.
            _ => {
                new.set_position(None);
                new.decor_mut().set_prefix(if first { "" } else { "\n" });
            }
        }
        servers.insert(name, Item::Table(new));
    }

    pub(super) fn remove(&mut self, name: &str) -> bool {
        let servers = self.doc.get_mut(SERVERS).and_then(Item::as_table_mut);
        servers.is_some_and(|s| s.remove(name).is_some())
    }

    pub(super) fn to_bytes(&self) -> Vec<u8> {
        self.doc.to_string().into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::{Codex, check_codex_project_trust};
    use crate::vendor::Vendor;
    use crate::vendor::fixtures::*;
    use std::path::Path;

    fn trust_fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        std::fs::create_dir(&cwd).unwrap();
        let config = dir.path().join("config.toml");
        (dir, cwd, config)
    }

    fn trust_entry(path: &Path, level: &str) -> String {
        format!(
            "[projects.{}]\ntrust_level = \"{}\"\n",
            toml::Value::String(path.display().to_string()),
            level
        )
    }

    #[test]
    fn trust_uses_canonical_cwd_or_nearest_git_root_not_an_arbitrary_ancestor() {
        let (_dir, cwd, config) = trust_fixture();
        std::fs::write(&config, trust_entry(&cwd, "trusted")).unwrap();
        assert_eq!(check_codex_project_trust(&cwd, &config).unwrap().cwd, cwd);
        let sub = cwd.join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(check_codex_project_trust(&sub, &config).is_err());
        std::fs::create_dir(cwd.join(".git")).unwrap();
        let checked = check_codex_project_trust(&sub, &config).unwrap();
        assert_eq!(checked.cwd, sub);
        assert_eq!(checked.trust_key, cwd);
    }

    #[test]
    fn trust_worktree_fallback_never_overrides_an_existing_denial() {
        let (dir, cwd, config) = trust_fixture();
        let main = dir.path().join("main");
        let gitdir = main.join(".git/worktrees/branch");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(cwd.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        let fallback = trust_entry(&main, "trusted");
        std::fs::write(&config, &fallback).unwrap();
        assert_eq!(
            check_codex_project_trust(&cwd, &config).unwrap().trust_key,
            main
        );
        for own in [
            trust_entry(&cwd, "untrusted"),
            format!(
                "[projects.{}]\n",
                toml::Value::String(cwd.display().to_string())
            ),
        ] {
            std::fs::write(&config, format!("{own}\n{fallback}")).unwrap();
            assert!(check_codex_project_trust(&cwd, &config).is_err());
        }
    }

    #[test]
    fn trust_accepts_standalone_gitfiles_without_a_common_directory() {
        // Faithful layouts for `git init --separate-git-dir` and an absorbed
        // submodule: both own their objects/refs and have no commondir file.
        for relative_gitdir in ["../separate.git", "../super/.git/modules/project"] {
            let (_dir, cwd, config) = trust_fixture();
            let gitdir = cwd.join(relative_gitdir);
            std::fs::create_dir_all(gitdir.join("objects/info")).unwrap();
            std::fs::create_dir_all(gitdir.join("objects/pack")).unwrap();
            std::fs::create_dir_all(gitdir.join("refs/heads")).unwrap();
            std::fs::create_dir_all(gitdir.join("refs/tags")).unwrap();
            std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            std::fs::write(
                gitdir.join("config"),
                "[core]\nrepositoryformatversion = 0\nbare = false\n",
            )
            .unwrap();
            std::fs::write(cwd.join(".git"), format!("gitdir: {relative_gitdir}\n")).unwrap();
            let sub = cwd.join("src");
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(&config, trust_entry(&cwd, "trusted")).unwrap();
            let checked = check_codex_project_trust(&sub, &config).unwrap();
            assert_eq!(checked.cwd, sub);
            assert_eq!(checked.trust_key, cwd);

            // No inferred trust from the separate gitdir or superproject.
            std::fs::write(
                &config,
                trust_entry(&gitdir.canonicalize().unwrap(), "trusted"),
            )
            .unwrap();
            assert!(check_codex_project_trust(&sub, &config).is_err());
            std::fs::write(&config, trust_entry(&cwd, "untrusted")).unwrap();
            assert!(check_codex_project_trust(&sub, &config).is_err());
        }
    }

    #[test]
    fn trust_rejects_malformed_linked_worktree_common_directory() {
        let (dir, cwd, config) = trust_fixture();
        let gitdir = dir.path().join("main/.git/worktrees/project");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(cwd.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(gitdir.join("gitdir"), cwd.join(".git").to_str().unwrap()).unwrap();
        std::fs::write(&config, trust_entry(&cwd, "trusted")).unwrap();
        // Missing optional commondir suppresses fallback, not explicit trust.
        assert_eq!(
            check_codex_project_trust(&cwd, &config).unwrap().trust_key,
            cwd
        );
        let common = gitdir.join("commondir");
        for bytes in [
            b"\n".to_vec(),
            b"missing-directory\n".to_vec(),
            b"HEAD\n".to_vec(),
            vec![0xff],
            vec![b'x'; 8193],
        ] {
            std::fs::write(&common, bytes).unwrap();
            assert!(check_codex_project_trust(&cwd, &config).is_err());
        }
        std::fs::remove_file(&common).unwrap();
        std::fs::create_dir(&common).unwrap();
        assert!(check_codex_project_trust(&cwd, &config).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn trust_rejects_unreadable_or_dangling_common_directory_metadata() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (dir, cwd, config) = trust_fixture();
        let gitdir = dir.path().join("main/.git/worktrees/project");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(cwd.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(gitdir.join("gitdir"), cwd.join(".git").to_str().unwrap()).unwrap();
        std::fs::write(&config, trust_entry(&cwd, "trusted")).unwrap();
        let common = gitdir.join("commondir");
        std::fs::write(&common, "../..\n").unwrap();
        std::fs::set_permissions(&common, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&common).is_err() {
            assert!(check_codex_project_trust(&cwd, &config).is_err());
        }
        std::fs::set_permissions(&common, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_file(&common).unwrap();
        symlink("missing-target", &common).unwrap();
        assert!(check_codex_project_trust(&cwd, &config).is_err());
    }

    #[test]
    fn trust_refusals_are_bounded_actionable_and_redacted() {
        let (_dir, cwd, config) = trust_fixture();
        for bytes in [
            b"secret = \"PRIVATE_TOKEN\n".to_vec(),
            vec![b'x'; 1024 * 1024 + 1],
            vec![0xff],
            Vec::new(),
        ] {
            std::fs::write(&config, bytes).unwrap();
            let err = check_codex_project_trust(&cwd, &config)
                .unwrap_err()
                .to_string();
            assert!(!err.contains("PRIVATE_TOKEN"));
            assert!(err.contains(cwd.to_str().unwrap()), "{err}");
            assert!(err.contains(config.to_str().unwrap()), "{err}");
            assert!(
                err.contains(&trust_entry(&cwd, "trusted").trim().to_string()),
                "{err}"
            );
        }
        std::fs::remove_file(&config).unwrap();
        assert!(check_codex_project_trust(&cwd, &config).is_err());
        std::fs::create_dir(&config).unwrap();
        assert!(check_codex_project_trust(&cwd, &config).is_err());
    }

    #[test]
    fn trust_accepts_the_config_byte_limit_and_refuses_one_more_byte() {
        let (_dir, cwd, config) = trust_fixture();
        let mut text = trust_entry(&cwd, "trusted");
        text.push('#');
        text.push_str(&"x".repeat(1024 * 1024 - text.len()));
        std::fs::write(&config, &text).unwrap();
        assert!(check_codex_project_trust(&cwd, &config).is_ok());
        text.push('x');
        std::fs::write(&config, &text).unwrap();
        assert!(
            check_codex_project_trust(&cwd, &config)
                .unwrap_err()
                .to_string()
                .contains("1048576 byte limit")
        );
    }

    #[cfg(unix)]
    #[test]
    fn trust_refuses_an_unreadable_config_and_canonicalizes_symlink_cwd() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (dir, cwd, config) = trust_fixture();
        std::fs::write(&config, trust_entry(&cwd, "trusted")).unwrap();
        let link = dir.path().join("alias");
        symlink(&cwd, &link).unwrap();
        assert_eq!(check_codex_project_trust(&link, &config).unwrap().cwd, cwd);
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root can read mode-000 files; verify the actual read boundary where applicable.
        if std::fs::File::open(&config).is_err() {
            assert!(check_codex_project_trust(&cwd, &config).is_err());
        }
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_trust_read_detects_bytes_when_metadata_reports_zero() {
        // procfs reports zero bytes even though this process has a nonempty command line.
        let file = Path::new("/proc/self/cmdline");
        assert_eq!(std::fs::metadata(file).unwrap().len(), 0);
        assert!(
            super::read_trust_file(file, 1)
                .unwrap_err()
                .contains("1 byte limit")
        );
    }

    #[test]
    fn trust_rejects_bad_git_indirection_even_with_a_trusted_entry() {
        let (dir, cwd, config) = trust_fixture();
        std::fs::write(&config, trust_entry(&cwd, "trusted")).unwrap();
        for text in ["PRIVATE_TOKEN".to_string(), "x".repeat(8193)] {
            std::fs::write(cwd.join(".git"), text).unwrap();
            let err = check_codex_project_trust(&cwd, &config)
                .unwrap_err()
                .to_string();
            assert!(!err.contains("PRIVATE_TOKEN"));
        }
        let gitdir = dir.path().join("gitdir");
        std::fs::write(cwd.join(".git"), format!("gitdir: {}", gitdir.display())).unwrap();
        assert!(check_codex_project_trust(&cwd, &config).is_err());
        std::fs::write(&gitdir, "not a directory").unwrap();
        assert!(check_codex_project_trust(&cwd, &config).is_err());
        std::fs::remove_file(&gitdir).unwrap();
        std::fs::create_dir(&gitdir).unwrap();
        std::fs::write(gitdir.join("commondir"), "x".repeat(8193)).unwrap();
        assert!(check_codex_project_trust(&cwd, &config).is_err());
    }

    #[test]
    fn a_stdio_server_forwards_a_secret_by_name_and_writes_literals_only() {
        assert_eq!(
            file_with(&Codex, "files", STDIO),
            r#"[mcp_servers.files]
command = "npx"
args = ["-y", "@example/files-mcp@1.4.2"]
env = { LOG_LEVEL = "debug" }
env_vars = ["FILES_TOKEN"]
"#
        );
    }

    #[test]
    fn a_server_whose_env_is_all_secret_gets_no_env_table() {
        assert_eq!(
            file_with(&Codex, "files", ONLY_SECRET),
            "[mcp_servers.files]\ncommand = \"files-mcp\"\nenv_vars = [\"FILES_TOKEN\"]\n"
        );
    }

    #[test]
    fn a_secret_bearer_authorization_is_the_bearer_token_variable() {
        assert_eq!(
            file_with(&Codex, "docs", BEARER),
            r#"[mcp_servers.docs]
url = "https://mcp.example.com/mcp"
bearer_token_env_var = "DOCS_TOKEN"
http_headers = { X-Client = "fl" }
"#
        );
        // A header name and a scheme are matched without regard to case.
        let lower = "transport = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n\
                     headers.authorization = { secret = true, env = \"T\", scheme = \"bearer\" }\n";
        assert_eq!(
            file_with(&Codex, "docs", lower),
            "[mcp_servers.docs]\nurl = \"https://mcp.example.com/mcp\"\n\
             bearer_token_env_var = \"T\"\n"
        );
    }

    #[test]
    fn a_secret_header_with_no_scheme_is_read_whole_from_its_variable() {
        assert_eq!(
            file_with(&Codex, "docs", WHOLE),
            r#"[mcp_servers.docs]
url = "https://mcp.example.com/mcp"
env_http_headers = { X-Api-Key = "DOCS_KEY" }
"#
        );
    }

    #[test]
    fn an_sse_remote_is_refused_for_codex() {
        assert_eq!(
            refusal(&Codex, SSE),
            "Codex connects to streamable HTTP servers only, not SSE"
        );
    }

    #[test]
    fn a_secret_header_codex_cannot_prefix_is_refused() {
        let token = "transport = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n\
                     headers.Authorization = { secret = true, env = \"T\", scheme = \"Token\" }\n";
        let elsewhere = "transport = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n\
                         headers.X-Auth = { secret = true, env = \"T\", scheme = \"Bearer\" }\n";
        for (body, header, scheme) in [
            (token, "Authorization", "Token"),
            (elsewhere, "X-Auth", "Bearer"),
        ] {
            assert_eq!(
                refusal(&Codex, body),
                format!(
                    "the secret header `{header}` has the scheme `{scheme}`, and Codex sends a \
                     secret header either whole or as `Authorization: Bearer`"
                )
            );
        }
    }

    #[test]
    fn a_secret_under_another_name_is_refused_for_codex() {
        assert_eq!(
            refusal(&Codex, RENAMED),
            "`env.GITHUB_TOKEN` reads the variable `GH_PAT`, and Codex forwards a secret \
             variable under its own name"
        );
    }

    const COMMENTED: &str = r#"# Codex settings for this repository.
model = "o3"   # the default model

# A server someone added by hand.
[mcp_servers.theirs]
command = "their-tool"
args = [ "--fast" ]   # keep it quick

[mcp_servers.theirs.env]
MODE = "x"

# Profiles sit between the servers.
[profiles.fast]
model = "o4-mini"

# The docs server, as fl wrote it.
[mcp_servers.docs]
url = "https://old.example.com/mcp"
"#;

    #[test]
    fn codex_comments_and_layout_survive() {
        let mut file = Codex
            .open(Path::new("f"), Some(COMMENTED.as_bytes()))
            .unwrap();
        let theirs = file.entry("theirs").unwrap();
        file.set("docs", &Codex.render("docs", &server(BEARER)).unwrap());
        file.set(
            "files",
            &Codex.render("files", &server(ONLY_SECRET)).unwrap(),
        );
        let written = String::from_utf8(file.to_bytes()).unwrap();
        assert_eq!(
            written,
            r#"# Codex settings for this repository.
model = "o3"   # the default model

# A server someone added by hand.
[mcp_servers.theirs]
command = "their-tool"
args = [ "--fast" ]   # keep it quick

[mcp_servers.theirs.env]
MODE = "x"

# Profiles sit between the servers.
[profiles.fast]
model = "o4-mini"

# The docs server, as fl wrote it.
[mcp_servers.docs]
url = "https://mcp.example.com/mcp"
bearer_token_env_var = "DOCS_TOKEN"
http_headers = { X-Client = "fl" }

[mcp_servers.files]
command = "files-mcp"
env_vars = ["FILES_TOKEN"]
"#
        );
        let mut file = Codex
            .open(Path::new("f"), Some(written.as_bytes()))
            .unwrap();
        assert_eq!(file.entry("theirs").unwrap(), theirs);
        assert_eq!(file.names(), ["theirs", "docs", "files"]);
        assert!(file.remove("docs"));
        assert!(file.remove("files"));
        assert_eq!(
            String::from_utf8(file.to_bytes()).unwrap(),
            // The comment above an entry is the entry's, and goes with it.
            COMMENTED.replace(
                "\n# The docs server, as fl wrote it.\n[mcp_servers.docs]\n\
                 url = \"https://old.example.com/mcp\"\n",
                ""
            )
        );
    }

    #[test]
    fn a_first_entry_in_a_file_with_no_servers_goes_last() {
        let text = "model = \"o3\"\n\n[profiles.fast]\nmodel = \"o4-mini\"\n";
        let mut file = Codex.open(Path::new("f"), Some(text.as_bytes())).unwrap();
        file.set(
            "files",
            &Codex.render("files", &server(ONLY_SECRET)).unwrap(),
        );
        assert_eq!(
            String::from_utf8(file.to_bytes()).unwrap(),
            format!(
                "{text}\n[mcp_servers.files]\ncommand = \"files-mcp\"\n\
                 env_vars = [\"FILES_TOKEN\"]\n"
            )
        );
    }

    #[test]
    fn a_codex_file_fl_cannot_read_as_a_table_of_servers_is_refused() {
        // A line of the file can hold a value, so the refusal quotes none.
        let cases: [(&[u8], &str); 4] = [
            (
                b"model = \"o3\"\n\ntoken = \"sk-live-4f9a\" x\n",
                "it is not valid TOML (line 3: ",
            ),
            (b"model = \"\xff\"\n", "it is not valid TOML (invalid utf-8"),
            (b"mcp_servers = 3\n", "`mcp_servers` is not a table"),
            (
                b"mcp_servers = { x = { command = \"x\" } }\n",
                "`mcp_servers` is not a table",
            ),
        ];
        for (text, problem) in cases {
            let err = Codex.open(Path::new("p/.codex/config.toml"), Some(text));
            let err = err.unwrap_err().to_string();
            assert!(!err.contains("sk-live-4f9a"), "{err}");
            assert!(
                err.starts_with(&format!("p/.codex/config.toml: {problem}")),
                "{err}"
            );
            assert!(
                err.ends_with("Fix it by hand, then run `fl mcp sync` again"),
                "{err}"
            );
        }
    }
}
