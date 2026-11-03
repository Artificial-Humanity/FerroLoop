//! Design and planning layer for external reference-repository indexing
//! (`fl state ref-index`, Record #91).
//!
//! Before writing code, a caller may want to consult external open-source
//! reference code or documentation. This module defines the validated plan
//! for such on-demand indexing: which named sources would be indexed, where
//! each source lives, and optionally which revision of it.
//!
//! It deliberately does **not** do any of the following yet:
//!
//! * clone, fetch, or otherwise retrieve a source (no network or filesystem
//!   access of any kind);
//! * wire the plan into a CLI command;
//! * persist a plan or an index in the store.
//!
//! Those land after Record #92. This module is pure validation and planning
//! only. Construction through [`RefSource::new`], [`RefIndexPlan::build`],
//! or `Deserialize` validates (and slug-normalizes) the value, so a value
//! obtained through those paths satisfies the documented invariants.
//! `Deserialize` is implemented manually (not derived) for both types and
//! routes through that same validation; see the impls below.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::state::validate_slug;

/// Maximum number of sources in one [`RefIndexPlan`].
pub const MAX_REF_SOURCES: usize = 32;

/// What kind of material a reference source holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    /// Source code to search and read.
    Code,
    /// Documentation to search and read.
    Docs,
}

/// One named external reference source in a [`RefIndexPlan`].
///
/// Fields are private. Construct with [`RefSource::new`] (or
/// `Deserialize`, which routes through `new`) and read with the
/// `name` / `locator` / `revision` / `kind` accessors, so an invalid
/// or un-normalized source cannot be assembled field by field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefSource {
    /// Slug identifying the source; validated by
    /// [`validate_slug`](crate::state::validate_slug), which also caps its
    /// length, so no separate name-length limit lives here.
    name: String,
    /// Where the source lives: an `https://`, `http://`, or `ssh://` URL, a
    /// scp-style `git@host:path`, or an absolute local path.
    locator: String,
    /// Optional revision (branch, tag, or commit) to index. `None` means the
    /// source's default revision.
    revision: Option<String>,
    /// Whether this source is code or documentation.
    kind: RefKind,
}

impl RefSource {
    /// Validate and normalize (slug-lowercase the name), or refuse.
    pub fn new(
        name: impl Into<String>,
        locator: impl Into<String>,
        revision: Option<String>,
        kind: RefKind,
    ) -> Result<Self, RefIndexError> {
        let mut source = Self {
            name: name.into(),
            locator: locator.into(),
            revision,
            kind,
        };
        source.validate()?;
        source.name = source.validated_name()?;
        Ok(source)
    }

    /// The validated (normalized) slug for this source.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where the source lives, as validated.
    pub fn locator(&self) -> &str {
        &self.locator
    }

    /// The revision to index, if one was given.
    pub fn revision(&self) -> Option<&str> {
        self.revision.as_deref()
    }

    /// Whether this source is code or documentation.
    pub fn kind(&self) -> RefKind {
        self.kind
    }

    /// Validate the name, locator, and revision.
    ///
    /// The name must satisfy [`validate_slug`](crate::state::validate_slug).
    /// The locator must be an `https://`, `http://`, or `ssh://` URL, a
    /// scp-style `git@host:path`, or an absolute local path, with no embedded
    /// credentials, no whitespace, no backslash, no leading `//`, and no
    /// `..` traversal (literal or percent-encoded) in any form. A present
    /// revision must be non-empty, free of whitespace and control
    /// characters, not start with `-`, and not contain `..`.
    pub fn validate(&self) -> Result<(), RefIndexError> {
        validate_slug(&self.name).map_err(|_| RefIndexError::InvalidName(self.name.clone()))?;
        validate_locator(&self.name, &self.locator)?;
        if let Some(revision) = &self.revision {
            validate_revision(&self.name, revision)?;
        }
        Ok(())
    }

    /// The validated (normalized) slug for this source's name.
    fn validated_name(&self) -> Result<String, RefIndexError> {
        validate_slug(&self.name).map_err(|_| RefIndexError::InvalidName(self.name.clone()))
    }
}

// Manual, not derived: derived deserialization would construct the struct
// field by field and bypass `new`, yielding unvalidated sources. The wire
// shape is unchanged (same field names via the private `Wire` mirror).
impl<'de> Deserialize<'de> for RefSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            name: String,
            locator: String,
            revision: Option<String>,
            kind: RefKind,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.name, wire.locator, wire.revision, wire.kind)
            .map_err(serde::de::Error::custom)
    }
}

/// Why a reference source or plan was refused.
///
/// Locators stored in these errors are redacted (see `redact_locator`):
/// any `user:pass@` userinfo is replaced with `***`, so `Display` (and
/// `Debug`) never echo embedded credentials.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefIndexError {
    #[error(
        "invalid reference name `{0}`: use 1 to 64 lowercase letters, digits, `-`, or `_`, \
         starting with a letter (it is normalized to lowercase first)"
    )]
    InvalidName(String),
    #[error(
        "reference `{name}` has an empty locator: give an https://, http://, or ssh:// URL, a git@host:path, or an absolute local path"
    )]
    EmptyLocator { name: String },
    #[error(
        "reference `{name}` has invalid locator `{locator}`: {reason}; use an https://, http://, \
         or ssh:// URL, a git@host:path, or an absolute local path"
    )]
    InvalidLocator {
        name: String,
        locator: String,
        reason: String,
    },
    #[error(
        "reference `{name}` locator `{locator}` embeds credentials: remove the `user:pass@` \
         part and supply credentials through the vault instead"
    )]
    EmbeddedCredentials { name: String, locator: String },
    #[error(
        "reference `{name}` locator `{locator}` contains a `..` path segment: give a direct \
         absolute path without traversal"
    )]
    PathTraversal { name: String, locator: String },
    #[error(
        "reference `{name}` has invalid revision `{revision}`: give a non-empty branch, tag, or \
         commit without whitespace or control characters, or omit it for the default revision"
    )]
    InvalidRevision { name: String, revision: String },
    #[error(
        "duplicate reference name `{0}`: each source name must be unique in a plan; rename or \
         remove one of them"
    )]
    DuplicateName(String),
    #[error(
        "too many reference sources: got {count}, at most {MAX_REF_SOURCES} are allowed in one \
         plan; split the plan or remove sources"
    )]
    TooManySources { count: usize },
}

/// Replace any userinfo in `locator` with `***` so errors never leak it.
///
/// Handles `scheme://user:pass@host/...` and scheme-less
/// `user:pass@host/...` (including a leading `//`). A plain `git@host`
/// scp user carries no password and is left alone unless it has a `:`.
fn redact_locator(locator: &str) -> String {
    if let Some((scheme, rest)) = locator.split_once("://") {
        let authority = authority_of(rest);
        if let Some((_, host_part)) = authority.rsplit_once('@') {
            return format!("{scheme}://***@{host_part}{}", &rest[authority.len()..]);
        }
        return locator.to_string();
    }
    if let Some((userinfo, host_part)) = locator.rsplit_once('@')
        && userinfo.contains(':')
        && !userinfo.contains('/')
    {
        return format!("***@{host_part}");
    }
    locator.to_string()
}

fn validate_locator(name: &str, locator: &str) -> Result<(), RefIndexError> {
    if locator.is_empty() {
        return Err(RefIndexError::EmptyLocator {
            name: name.to_string(),
        });
    }
    if locator.chars().any(char::is_control) {
        return Err(invalid_locator(
            name,
            locator,
            "it contains control characters",
        ));
    }
    if locator.chars().any(char::is_whitespace) {
        return Err(invalid_locator(name, locator, "it contains whitespace"));
    }
    if locator.contains('\\') {
        return Err(invalid_locator(
            name,
            locator,
            "it contains a backslash; use forward slashes",
        ));
    }
    if locator.starts_with("//") {
        return Err(invalid_locator(
            name,
            locator,
            "it starts with `//`; give an explicit URL or absolute local path",
        ));
    }
    if let Some(rest) = locator
        .strip_prefix("https://")
        .or_else(|| locator.strip_prefix("http://"))
    {
        return validate_http_locator(name, locator, rest);
    }
    if let Some(rest) = locator.strip_prefix("ssh://") {
        return validate_ssh_locator(name, locator, rest);
    }
    if locator.starts_with("git@") {
        return validate_scp_locator(name, locator);
    }
    if Path::new(locator).is_absolute() {
        return validate_local_path(name, locator);
    }
    Err(invalid_locator(
        name,
        locator,
        "it is not a recognized URL, git@host:path, or absolute local path",
    ))
}

fn invalid_locator(name: &str, locator: &str, reason: &str) -> RefIndexError {
    RefIndexError::InvalidLocator {
        name: name.to_string(),
        locator: redact_locator(locator),
        reason: reason.to_string(),
    }
}

fn embedded_credentials(name: &str, locator: &str) -> RefIndexError {
    RefIndexError::EmbeddedCredentials {
        name: name.to_string(),
        locator: redact_locator(locator),
    }
}

fn path_traversal(name: &str, locator: &str) -> RefIndexError {
    RefIndexError::PathTraversal {
        name: name.to_string(),
        locator: redact_locator(locator),
    }
}

/// The `host[:port]` (or `user@host[:port]`) part of a URL: everything before
/// the first `/`, `?`, or `#`.
fn authority_of(rest: &str) -> &str {
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    &rest[..end]
}

/// The path part of a URL remainder: after the authority, before `?`/`#`.
fn path_of(rest: &str) -> &str {
    let after_authority = &rest[authority_of(rest).len()..];
    let end = after_authority
        .find(['?', '#'])
        .unwrap_or(after_authority.len());
    &after_authority[..end]
}

/// Percent-decode `%XX` sequences (hex is case-insensitive); invalid
/// sequences are left as-is.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(value) = u8::from_str_radix(hex, 16)
        {
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// True if `path` contains a `..` segment, literally or percent-encoded
/// (`%2e%2e`, `..%2f`, mixed case, or multiply-encoded). Repeated decoding
/// is bounded; backslashes produced by decoding count as separators.
fn contains_traversal(path: &str) -> bool {
    let mut decoded = path.to_string();
    for _ in 0..4 {
        let next = percent_decode(&decoded);
        if next == decoded {
            break;
        }
        decoded = next;
    }
    decoded.split(['/', '\\']).any(|segment| segment == "..")
}

fn check_traversal(name: &str, locator: &str, path: &str) -> Result<(), RefIndexError> {
    if contains_traversal(path) {
        return Err(path_traversal(name, locator));
    }
    Ok(())
}

/// The host part of a URL remainder, with any `user@` stripped.
fn host_of(rest: &str) -> &str {
    let host_port = authority_of(rest).rsplit('@').next().unwrap_or_default();
    host_port
        .strip_prefix('[')
        .and_then(|h| h.split(']').next())
        .unwrap_or_else(|| host_port.split(':').next().unwrap_or_default())
}

fn validate_http_locator(name: &str, locator: &str, rest: &str) -> Result<(), RefIndexError> {
    let authority = authority_of(rest);
    if authority.contains('@') {
        return Err(embedded_credentials(name, locator));
    }
    validate_authority_locator(name, locator, rest)
}

fn validate_ssh_locator(name: &str, locator: &str, rest: &str) -> Result<(), RefIndexError> {
    // Reject `user:pass@` (or percent-encoded `user%3apass@`) like http.
    // A bare `user@` (e.g. `git@`) remains allowed.
    let authority = authority_of(rest);
    if let Some((userinfo, _)) = authority.rsplit_once('@')
        && percent_decode(userinfo).contains(':')
    {
        return Err(embedded_credentials(name, locator));
    }
    validate_authority_locator(name, locator, rest)
}

fn validate_authority_locator(name: &str, locator: &str, rest: &str) -> Result<(), RefIndexError> {
    if host_of(rest).is_empty() {
        return Err(invalid_locator(name, locator, "it has no host"));
    }
    check_traversal(name, locator, path_of(rest))
}

fn validate_scp_locator(name: &str, locator: &str) -> Result<(), RefIndexError> {
    let Some((host, path)) = locator["git@".len()..].split_once(':') else {
        return Err(invalid_locator(
            name,
            locator,
            "a scp-style locator must look like git@host:path",
        ));
    };
    if host.is_empty() || path.is_empty() || host.contains('/') {
        return Err(invalid_locator(
            name,
            locator,
            "a scp-style locator must name both a host and a path, like git@host:path",
        ));
    }
    check_traversal(name, locator, path)
}

fn validate_local_path(name: &str, locator: &str) -> Result<(), RefIndexError> {
    if Path::new(locator)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
        || contains_traversal(locator)
    {
        return Err(path_traversal(name, locator));
    }
    Ok(())
}

fn validate_revision(name: &str, revision: &str) -> Result<(), RefIndexError> {
    if revision.is_empty()
        || revision.starts_with('-')
        || revision.contains("..")
        || revision
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(RefIndexError::InvalidRevision {
            name: name.to_string(),
            revision: revision.to_string(),
        });
    }
    Ok(())
}

/// A validated set of reference sources to index, in caller-given order.
///
/// Fields are private. Construct with [`RefIndexPlan::build`] (or
/// `Deserialize`, which routes through `build`) and read with
/// [`RefIndexPlan::sources`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RefIndexPlan {
    sources: Vec<RefSource>,
}

// Manual, not derived, for the same reason as `RefSource`: derived
// deserialization would bypass `build` (cap, duplicates, normalization).
impl<'de> Deserialize<'de> for RefIndexPlan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            sources: Vec<RefSource>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::build(wire.sources).map_err(serde::de::Error::custom)
    }
}

impl RefIndexPlan {
    /// Validate every source and assemble a plan.
    ///
    /// Rejects more than [`MAX_REF_SOURCES`] sources and duplicate validated
    /// names. Source names are stored in their validated (normalized) form;
    /// input order is preserved.
    pub fn build(sources: Vec<RefSource>) -> Result<Self, RefIndexError> {
        if sources.len() > MAX_REF_SOURCES {
            return Err(RefIndexError::TooManySources {
                count: sources.len(),
            });
        }
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut validated = Vec::with_capacity(sources.len());
        for mut source in sources {
            source.validate()?;
            let name = source.validated_name()?;
            if !seen.insert(name.clone()) {
                return Err(RefIndexError::DuplicateName(name));
            }
            source.name = name;
            validated.push(source);
        }
        Ok(Self { sources: validated })
    }

    /// The validated sources, in plan order.
    pub fn sources(&self) -> &[RefSource] {
        &self.sources
    }

    /// The validated source names, in plan order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.sources.iter().map(|s| s.name.as_str())
    }

    /// Whether the plan has no sources.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(name: &str, locator: &str) -> RefSource {
        RefSource::new(name, locator, None, RefKind::Code).unwrap()
    }

    fn bad_source(name: &str, locator: &str) -> RefIndexError {
        RefSource::new(name, locator, None, RefKind::Code).unwrap_err()
    }

    #[test]
    fn valid_https_ssh_scp_and_local_sources_pass() {
        for locator in [
            "https://github.com/rust-lang/rust.git",
            "http://example.com/docs",
            "ssh://git@github.com/rust-lang/rust.git",
            "git@github.com:rust-lang/rust.git",
            "/home/user/reference/rust",
        ] {
            source("rust-ref", locator).validate().unwrap();
        }
        let docs = RefSource::new(
            "rust-docs",
            "https://doc.rust-lang.org/book/",
            None,
            RefKind::Docs,
        )
        .unwrap();
        docs.validate().unwrap();
        assert_eq!(docs.kind(), RefKind::Docs);
        assert_eq!(docs.locator(), "https://doc.rust-lang.org/book/");
        assert_eq!(docs.revision(), None);
    }

    #[test]
    fn embedded_credentials_are_rejected() {
        let err = bad_source(
            "rust-ref",
            "https://user:secret@github.com/rust-lang/rust.git",
        );
        assert!(matches!(err, RefIndexError::EmbeddedCredentials { .. }));
        assert!(
            bad_source("rust-ref", "http://user:secret@example.com/repo")
                .to_string()
                .contains("***")
        );
    }

    #[test]
    fn ssh_password_userinfo_is_rejected() {
        for locator in [
            "ssh://git:secretpw@github.com/rust-lang/rust.git",
            "ssh://user:p4ssw0rd@example.com/repo.git",
            "ssh://user%3ap4ssw0rd@example.com/repo.git",
            "ssh://user%3Ap4ssw0rd@example.com/repo.git",
        ] {
            let err = bad_source("rust-ref", locator);
            assert!(
                matches!(err, RefIndexError::EmbeddedCredentials { .. }),
                "{locator} must be EmbeddedCredentials, got {err:?}"
            );
            assert!(!err.to_string().contains("secretpw"), "{err}");
            assert!(!err.to_string().contains("p4ssw0rd"), "{err}");
            assert!(!format!("{err:?}").contains("secretpw"), "{err:?}");
            assert!(!format!("{err:?}").contains("p4ssw0rd"), "{err:?}");
        }
        // Bare ssh user (no password) is still allowed.
        source("rust-ref", "ssh://git@github.com/rust-lang/rust.git");
    }

    #[test]
    fn traversal_and_relative_local_paths_are_rejected() {
        let err = bad_source("rust-ref", "/home/user/../secret");
        assert!(matches!(err, RefIndexError::PathTraversal { .. }));
        assert!(RefSource::new("rust-ref", "relative/path", None, RefKind::Code).is_err());
        assert!(RefSource::new("rust-ref", "", None, RefKind::Code).is_err());
        assert!(
            RefSource::new(
                "rust-ref",
                "https://example.com/repo\u{0007}",
                None,
                RefKind::Code
            )
            .is_err()
        );
    }

    #[test]
    fn traversal_is_rejected_in_all_locator_forms() {
        for locator in [
            // http(s): literal, encoded, encoded-separator, mixed case
            "https://example.com/a/../b",
            "https://example.com/../secret",
            "https://example.com/%2e%2e%2fsecret",
            "https://example.com/%2E%2E%2Fsecret",
            "https://example.com/..%2fsecret",
            "https://example.com/..%2Fsecret",
            "https://example.com/a/%2e%2e/b",
            "https://example.com/%2e./secret",
            "http://example.com/%252e%252e%252fsecret",
            // ssh://
            "ssh://git@example.com/a/../b",
            "ssh://git@example.com/%2e%2e%2fsecret",
            "ssh://git@example.com/..%2fsecret",
            // scp-style
            "git@example.com:a/../b",
            "git@example.com:../secret",
            "git@example.com:%2e%2e%2fsecret",
            "git@example.com:..%2fsecret",
            "git@example.com:a/%2E%2E/b",
            // local, percent-encoded
            "/home/user/%2e%2e%2fsecret",
        ] {
            let err = bad_source("rust-ref", locator);
            assert!(
                matches!(err, RefIndexError::PathTraversal { .. }),
                "{locator} must be PathTraversal, got {err:?}"
            );
        }
        // Lookalikes that are not traversal still pass.
        for locator in [
            "https://example.com/a..b/repo.git",
            "https://example.com/.../repo",
            "git@example.com:a..b/repo.git",
        ] {
            source("rust-ref", locator);
        }
    }

    #[test]
    fn leading_double_slash_and_backslash_locators_are_rejected() {
        for locator in [
            "//example.com/repo",
            "//home/user/repo",
            "\\\\server\\share\\repo",
            "\\home\\user\\repo",
            "C:\\Users\\ref\\repo",
            "/home/user\\repo",
            "https://example.com/repo\\..\\secret",
            "git@example.com:repo\\..\\secret",
        ] {
            assert!(
                RefSource::new("rust-ref", locator, None, RefKind::Code).is_err(),
                "{locator} must be rejected"
            );
        }
    }

    #[test]
    fn credentials_are_redacted_in_error_display() {
        for locator in [
            "https://user:secret@github.com/rust-lang/rust.git",
            "ssh://git:s3cr3t@github.com/rust-lang/rust.git",
            "https://user:secret@example.com/a/../b",
            "https://user:secret@example.com/re po",
        ] {
            let err = bad_source("rust-ref", locator);
            let shown = err.to_string();
            assert!(!shown.contains("secret"), "{shown}");
            assert!(!shown.contains("s3cr3t"), "{shown}");
            assert!(shown.contains("***"), "{shown}");
            assert!(!format!("{err:?}").contains("secret"));
            assert!(!format!("{err:?}").contains("s3cr3t"));
        }
    }

    #[test]
    fn whitespace_locators_and_empty_hosts_are_rejected() {
        for locator in [
            " https://example.com/r.git",
            "https://example.com/r.git ",
            "https://exam ple.com/r.git",
            "https://example.com/re po",
            "ssh://git@github.com/rust lang/rust.git",
            "git@github.com:rust lang/rust.git",
            "/home/user/my dir",
            "https://example.com/r\t.git",
            // empty host
            "https:///repo",
            "https://:8080/repo",
            "http://?q=1",
            "ssh:///repo",
            "ssh://git@/repo",
            "ssh://:22/repo",
            "git@:rust-lang/rust.git",
            "git@github.com:",
            "git@/bad:repo",
        ] {
            assert!(
                RefSource::new("rust-ref", locator, None, RefKind::Code).is_err(),
                "{locator:?} must be rejected"
            );
        }
    }

    #[test]
    fn accessors_and_normalized_name() {
        let s = RefSource::new(
            "Rust-Ref",
            "https://example.com/r.git",
            Some("v1.0".to_string()),
            RefKind::Docs,
        )
        .unwrap();
        assert_eq!(s.name(), "rust-ref");
        assert_eq!(s.locator(), "https://example.com/r.git");
        assert_eq!(s.revision(), Some("v1.0"));
        assert_eq!(s.kind(), RefKind::Docs);
    }

    #[test]
    fn deserialize_routes_through_validation() {
        // Valid JSON deserializes, normalizes the name, exposes accessors.
        let s: RefSource = serde_json::from_str(
            r#"{"name":"Rust-Ref","locator":"https://example.com/r.git","revision":"v1","kind":"docs"}"#,
        )
        .unwrap();
        assert_eq!(s.name(), "rust-ref");
        assert_eq!(s.kind(), RefKind::Docs);
        assert_eq!(s.revision(), Some("v1"));
        // Omitted revision is allowed.
        let s: RefSource =
            serde_json::from_str(r#"{"name":"a-ref","locator":"/srv/refs/a","kind":"code"}"#)
                .unwrap();
        assert_eq!(s.revision(), None);
        // Invalid JSON values are refused, without leaking credentials.
        for json in [
            r#"{"name":"a-ref","locator":"ssh://git:secret@example.com/r.git","kind":"code"}"#,
            r#"{"name":"a-ref","locator":"https://example.com/%2e%2e%2fsecret","kind":"code"}"#,
            r#"{"name":"a-ref","locator":"//example.com/r.git","kind":"code"}"#,
            r#"{"name":"a-ref","locator":"https://example.com/r.git","revision":"-x","kind":"code"}"#,
        ] {
            let err = serde_json::from_str::<RefSource>(json).unwrap_err();
            assert!(!err.to_string().contains("git:secret"), "{err}");
        }
        // Serialize round-trips through the validating Deserialize.
        let original = source("rust-ref", "https://example.com/r.git");
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(serde_json::from_str::<RefSource>(&json).unwrap(), original);
    }

    #[test]
    fn deserialize_plan_routes_through_build() {
        let plan: RefIndexPlan = serde_json::from_str(
            r#"{"sources":[{"name":"Zeta","locator":"https://example.com/z.git","kind":"code"}]}"#,
        )
        .unwrap();
        assert_eq!(plan.sources().len(), 1);
        assert_eq!(plan.sources()[0].name(), "zeta");
        // Duplicate (post-normalization) names are refused on deserialize.
        assert!(
            serde_json::from_str::<RefIndexPlan>(
                r#"{"sources":[
                    {"name":"Dup","locator":"https://example.com/a.git","kind":"code"},
                    {"name":"dup","locator":"https://example.com/b.git","kind":"code"}
                ]}"#
            )
            .is_err()
        );
        // Invalid member is refused on deserialize.
        assert!(
            serde_json::from_str::<RefIndexPlan>(
                r#"{"sources":[{"name":"a-ref","locator":"relative/path","kind":"code"}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn duplicate_validated_slug_is_rejected_and_named() {
        let err = RefIndexPlan::build(vec![
            source("Rust-Ref", "https://example.com/a.git"),
            source("rust-ref", "https://example.com/b.git"),
        ])
        .unwrap_err();
        assert_eq!(err, RefIndexError::DuplicateName("rust-ref".to_string()));
        assert!(err.to_string().contains("rust-ref"));
    }

    #[test]
    fn source_cap_is_enforced() {
        let sources: Vec<RefSource> = (0..=MAX_REF_SOURCES)
            .map(|i| source(&format!("ref-{i}"), "https://example.com/r.git"))
            .collect();
        let err = RefIndexPlan::build(sources).unwrap_err();
        assert_eq!(
            err,
            RefIndexError::TooManySources {
                count: MAX_REF_SOURCES + 1
            }
        );
        let at_cap: Vec<RefSource> = (0..MAX_REF_SOURCES)
            .map(|i| source(&format!("ref-{i}"), "https://example.com/r.git"))
            .collect();
        assert_eq!(
            RefIndexPlan::build(at_cap).unwrap().sources().len(),
            MAX_REF_SOURCES
        );
    }

    #[test]
    fn revision_validation() {
        RefSource::new(
            "rust-ref",
            "https://example.com/r.git",
            Some("v1.2.3".to_string()),
            RefKind::Code,
        )
        .unwrap();

        for bad in ["", "  ", "my branch", "bad\u{0001}rev"] {
            let err = RefSource::new(
                "rust-ref",
                "https://example.com/r.git",
                Some(bad.to_string()),
                RefKind::Code,
            )
            .unwrap_err();
            assert!(
                matches!(err, RefIndexError::InvalidRevision { .. }),
                "revision {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn dash_and_dotdot_revisions_are_rejected() {
        for bad in [
            "-main",
            "--upload-pack=touch",
            "-",
            "..",
            "a..b",
            "v1..2",
            "../main",
        ] {
            let err = RefSource::new(
                "rust-ref",
                "https://example.com/r.git",
                Some(bad.to_string()),
                RefKind::Code,
            )
            .unwrap_err();
            assert!(
                matches!(err, RefIndexError::InvalidRevision { .. }),
                "revision {bad:?} must be rejected, got {err:?}"
            );
        }
        // Single dots and interior dashes remain fine.
        for ok in ["v1.2.3", "release-1.0", "a.b/c"] {
            RefSource::new(
                "rust-ref",
                "https://example.com/r.git",
                Some(ok.to_string()),
                RefKind::Code,
            )
            .unwrap();
        }
    }

    #[test]
    fn build_preserves_input_order_and_names() {
        let plan = RefIndexPlan::build(vec![
            source("zeta", "https://example.com/z.git"),
            source("Alpha", "/srv/refs/alpha"),
            source("mid-docs", "git@example.com:docs/mid.git"),
        ])
        .unwrap();
        assert_eq!(
            plan.names().collect::<Vec<_>>(),
            ["zeta", "alpha", "mid-docs"]
        );
        assert_eq!(plan.sources().len(), 3);
        assert!(!plan.is_empty());
        assert!(RefIndexPlan::build(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn invalid_names_are_rejected() {
        assert!(matches!(
            RefSource::new("1bad", "https://example.com/r.git", None, RefKind::Code),
            Err(RefIndexError::InvalidName(_))
        ));
        assert!(RefSource::new("", "https://example.com/r.git", None, RefKind::Code).is_err());
    }
}
