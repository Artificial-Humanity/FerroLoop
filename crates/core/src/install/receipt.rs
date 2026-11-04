//! `install-receipt.json`: what an install OWNS.
//!
//! The receipt records identities, hashes and paths, and never a credential.
//! Its types have no free-form value slot: every string field is a path, a
//! name, a hash or a release name, and `deny_unknown_fields` means a JSON
//! document that carries any other field (a `token`, a `vault_key`) does not
//! parse.

use super::manifest::{BuildProvenance, ReleaseKind};
use super::{
    Product, Profile, RECEIPT_SCHEMA_SOURCE_BUILD, RECEIPT_SCHEMA_VERSION, file_name_ok,
    is_sha256_hex,
};
use serde::{Deserialize, Serialize};

/// A receipt larger than this is refused unread.
pub const MAX_RECEIPT_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const MAX_PATH_LEN: usize = 1024;

/// The release an install activated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptRelease {
    pub version: String,
    /// `<version>+<sha12>` — the directory name under `releases/`.
    pub name: String,
    pub kind: ReleaseKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// How a source build was made (reviewed or not, the toolchain). Present exactly when the
    /// receipt's `schema_version` is [`RECEIPT_SCHEMA_SOURCE_BUILD`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildProvenance>,
}

impl ReceiptRelease {
    /// The `schema_version` a receipt of this release is written with.
    pub fn schema_version(&self) -> u32 {
        if self.build.is_some() {
            RECEIPT_SCHEMA_SOURCE_BUILD
        } else {
            RECEIPT_SCHEMA_VERSION
        }
    }
}

/// The directories the install used, as the plan displayed them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptLayout {
    pub prefix: String,
    pub bin_dir: String,
    pub unit_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedArtifact {
    pub id: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnedPathKind {
    Dir,
    File,
    Symlink,
}

/// A path the install created. A file carries its hash, a symlink its target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedPath {
    pub path: String,
    pub kind: OwnedPathKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
}

/// A launcher alias (`agent-msg`): a symlink the install owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedAlias {
    pub name: String,
    pub path: String,
    pub target: String,
}

/// A service unit the install wrote. It carries the generated-by marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedUnit {
    pub name: String,
    pub path: String,
    pub sha256: String,
    /// The release name the marker line names.
    pub release: String,
}

/// An MCP catalog entry the install wrote into a config file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedCatalogEntry {
    pub file: String,
    pub name: String,
    pub sha256: String,
}

/// A block the install owns inside a config file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedConfigBlock {
    pub file: String,
    pub block_id: String,
    pub sha256: String,
}

/// Where a file transaction stood (types only; a later phase writes them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalPhase {
    Begin,
    Staged,
    Verified,
    Activated,
    Committed,
    RolledBack,
    Gc,
}

/// One journal line of the install transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEntry {
    pub seq: u64,
    pub phase: JournalPhase,
    pub release: String,
    pub at_ms: u64,
}

/// The install receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallReceipt {
    pub schema_version: u32,
    pub release: ReceiptRelease,
    pub profile: Profile,
    /// The explicit selection (profile products, plus adds, minus removes).
    #[serde(deserialize_with = "super::deserialize_installed_products")]
    pub products: Vec<Product>,
    pub layout: ReceiptLayout,
    #[serde(default)]
    pub artifacts: Vec<OwnedArtifact>,
    #[serde(default)]
    pub paths: Vec<OwnedPath>,
    #[serde(default)]
    pub aliases: Vec<OwnedAlias>,
    #[serde(default)]
    pub units: Vec<OwnedUnit>,
    #[serde(default)]
    pub catalog_entries: Vec<OwnedCatalogEntry>,
    #[serde(default)]
    pub config_blocks: Vec<OwnedConfigBlock>,
    #[serde(default)]
    pub journal: Vec<JournalEntry>,
}

/// Why a receipt was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReceiptError {
    #[error("receipt is {bytes} bytes; the limit is {MAX_RECEIPT_BYTES}")]
    TooLarge { bytes: usize },
    #[error("receipt is not valid: {0}")]
    Malformed(String),
    #[error(
        "unsupported receipt schema_version {found}; this release reads {RECEIPT_SCHEMA_VERSION} and {RECEIPT_SCHEMA_SOURCE_BUILD}"
    )]
    UnsupportedSchema { found: u64 },
    #[error("receipt has more than {MAX_ENTRIES} entries in `{0}`")]
    TooMany(&'static str),
    #[error("receipt `{field}` is not accepted: {reason}")]
    Bad {
        field: &'static str,
        reason: &'static str,
    },
}

fn path_problem(p: &str) -> Option<&'static str> {
    if p.is_empty() {
        Some("is empty")
    } else if p.len() > MAX_PATH_LEN {
        Some("is too long")
    } else if p.contains('\0') {
        Some("contains a NUL byte")
    } else if p.split('/').any(|s| s == "..") {
        Some("has a `..` segment")
    } else if !(p.starts_with('/') || p.starts_with("~/")) {
        Some("is neither absolute nor `~/`-relative")
    } else {
        None
    }
}

/// A symlink target: bounded, no NUL or control character.
fn link_target_check(field: &'static str, t: &str) -> Result<(), ReceiptError> {
    if t.is_empty() || t.len() > MAX_PATH_LEN || t.chars().any(char::is_control) {
        Err(ReceiptError::Bad {
            field,
            reason: "is empty, too long, or has a control character",
        })
    } else {
        Ok(())
    }
}

impl InstallReceipt {
    /// Parse and validate a receipt.
    pub fn parse(json: &str) -> Result<Self, ReceiptError> {
        if json.len() > MAX_RECEIPT_BYTES {
            return Err(ReceiptError::TooLarge { bytes: json.len() });
        }
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| ReceiptError::Malformed(e.to_string()))?;
        match value.get("schema_version").and_then(|v| v.as_u64()) {
            Some(v)
                if v == u64::from(RECEIPT_SCHEMA_VERSION)
                    || v == u64::from(RECEIPT_SCHEMA_SOURCE_BUILD) => {}
            Some(found) => return Err(ReceiptError::UnsupportedSchema { found }),
            None => {
                return Err(ReceiptError::Malformed(
                    "schema_version is missing or not a number".into(),
                ));
            }
        }
        let r: Self =
            serde_json::from_value(value).map_err(|e| ReceiptError::Malformed(e.to_string()))?;
        r.validate()?;
        Ok(r)
    }

    /// Pretty JSON with a trailing newline.
    pub fn to_json(&self) -> String {
        let mut out = serde_json::to_string_pretty(self).expect("receipt serializes");
        out.push('\n');
        out
    }

    pub fn validate(&self) -> Result<(), ReceiptError> {
        if self.schema_version != RECEIPT_SCHEMA_VERSION
            && self.schema_version != RECEIPT_SCHEMA_SOURCE_BUILD
        {
            return Err(ReceiptError::UnsupportedSchema {
                found: u64::from(self.schema_version),
            });
        }
        // Version 2 carries the source-build block and nothing else does.
        if self.schema_version != self.release.schema_version() {
            return Err(ReceiptError::Bad {
                field: "schema_version",
                reason: "does not match whether the release carries a build block (2) or not (1)",
            });
        }
        if let Some(b) = &self.release.build
            && let Some(why) = b.problem()
        {
            return Err(ReceiptError::Bad {
                field: "release.build",
                reason: why,
            });
        }
        if self.release.build.is_some()
            && (self.release.kind != ReleaseKind::SourceBuild || self.release.commit.is_none())
        {
            return Err(ReceiptError::Bad {
                field: "release.build",
                reason: "a build block belongs to a source build with a commit",
            });
        }
        for (name, n) in [
            ("products", self.products.len()),
            ("artifacts", self.artifacts.len()),
            ("paths", self.paths.len()),
            ("aliases", self.aliases.len()),
            ("units", self.units.len()),
            ("catalog_entries", self.catalog_entries.len()),
            ("config_blocks", self.config_blocks.len()),
            ("journal", self.journal.len()),
        ] {
            if n > MAX_ENTRIES {
                return Err(ReceiptError::TooMany(name));
            }
        }
        let bad = |field, reason| ReceiptError::Bad { field, reason };
        for p in [
            &self.layout.prefix,
            &self.layout.bin_dir,
            &self.layout.unit_dir,
        ] {
            if let Some(r) = path_problem(p) {
                return Err(bad("layout", r));
            }
        }
        for a in &self.artifacts {
            if !is_sha256_hex(&a.sha256) {
                return Err(bad("artifacts.sha256", "is not 64 lowercase hex digits"));
            }
        }
        for p in &self.paths {
            if let Some(r) = path_problem(&p.path) {
                return Err(bad("paths.path", r));
            }
            if let Some(h) = &p.sha256
                && !is_sha256_hex(h)
            {
                return Err(bad("paths.sha256", "is not 64 lowercase hex digits"));
            }
            match p.kind {
                OwnedPathKind::File if p.sha256.is_none() => {
                    return Err(bad("paths.sha256", "a file entry needs its hash"));
                }
                OwnedPathKind::Symlink if p.link_target.is_none() => {
                    return Err(bad("paths.link_target", "a symlink entry needs its target"));
                }
                _ => {}
            }
        }
        for a in &self.aliases {
            if let Some(r) = path_problem(&a.path) {
                return Err(bad("aliases.path", r));
            }
        }
        for u in &self.units {
            if let Some(r) = path_problem(&u.path) {
                return Err(bad("units.path", r));
            }
            if !is_sha256_hex(&u.sha256) {
                return Err(bad("units.sha256", "is not 64 lowercase hex digits"));
            }
        }
        for c in &self.catalog_entries {
            if !is_sha256_hex(&c.sha256) {
                return Err(bad(
                    "catalog_entries.sha256",
                    "is not 64 lowercase hex digits",
                ));
            }
        }
        for c in &self.config_blocks {
            if !is_sha256_hex(&c.sha256) {
                return Err(bad(
                    "config_blocks.sha256",
                    "is not 64 lowercase hex digits",
                ));
            }
        }
        let name = |field: &'static str, v: &str| {
            if file_name_ok(v) {
                Ok(())
            } else {
                Err(bad(
                    field,
                    "is not an accepted name (letters, digits and . _ - + @, up to 80, no `.`/`..`)",
                ))
            }
        };
        name("release.version", &self.release.version)?;
        name("release.name", &self.release.name)?;
        if let Some(c) = &self.release.commit
            && !(c.len() == 40 && c.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        {
            return Err(bad("release.commit", "is not 40 lowercase hex digits"));
        }
        for a in &self.artifacts {
            name("artifacts.id", &a.id)?;
        }
        for a in &self.aliases {
            name("aliases.name", &a.name)?;
            link_target_check("aliases.target", &a.target)?;
        }
        for p in &self.paths {
            if let Some(t) = &p.link_target {
                link_target_check("paths.link_target", t)?;
            }
        }
        for u in &self.units {
            name("units.name", &u.name)?;
            name("units.release", &u.release)?;
        }
        for c in &self.catalog_entries {
            name("catalog_entries.name", &c.name)?;
            if path_problem(&c.file).is_some() {
                return Err(bad(
                    "catalog_entries.file",
                    "is not a bounded absolute path",
                ));
            }
        }
        for c in &self.config_blocks {
            name("config_blocks.block_id", &c.block_id)?;
            if path_problem(&c.file).is_some() {
                return Err(bad("config_blocks.file", "is not a bounded absolute path"));
            }
        }
        for j in &self.journal {
            name("journal.release", &j.release)?;
        }
        Ok(())
    }

    pub fn owned_path(&self, path: &str) -> Option<&OwnedPath> {
        self.paths.iter().find(|p| p.path == path)
    }

    pub fn owned_alias(&self, path: &str) -> Option<&OwnedAlias> {
        self.aliases.iter().find(|a| a.path == path)
    }

    pub fn owned_unit(&self, path: &str) -> Option<&OwnedUnit> {
        self.units.iter().find(|u| u.path == path)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn h(c: char) -> String {
        c.to_string().repeat(64)
    }

    /// A receipt with EVERY optional field present, so a key scan sees them all.
    pub(crate) fn full_receipt() -> InstallReceipt {
        InstallReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            release: ReceiptRelease {
                version: "0.1.0".into(),
                name: "0.1.0+0123456789ab".into(),
                kind: ReleaseKind::Release,
                commit: Some("0123456789abcdef0123456789abcdef01234567".into()),
                build: None,
            },
            profile: Profile::Full,
            products: vec![Product::Ferroloop, Product::Ferrowire],
            layout: ReceiptLayout {
                prefix: "/home/u/.local/share/fl".into(),
                bin_dir: "/home/u/.local/bin".into(),
                unit_dir: "/home/u/.config/systemd/user".into(),
            },
            artifacts: vec![OwnedArtifact {
                id: "fl-linux".into(),
                sha256: h('a'),
            }],
            paths: vec![
                OwnedPath {
                    path: "/home/u/.local/share/fl/releases/0.1.0+0123456789ab/bin/fl".into(),
                    kind: OwnedPathKind::File,
                    sha256: Some(h('b')),
                    link_target: None,
                },
                OwnedPath {
                    path: "/home/u/.local/share/fl/current".into(),
                    kind: OwnedPathKind::Symlink,
                    sha256: None,
                    link_target: Some("releases/0.1.0+0123456789ab".into()),
                },
            ],
            aliases: vec![OwnedAlias {
                name: "agent-msg".into(),
                path: "/home/u/.local/bin/agent-msg".into(),
                target: "fl".into(),
            }],
            units: vec![OwnedUnit {
                name: "ferrowire.service".into(),
                path: "/home/u/.config/systemd/user/ferrowire.service".into(),
                sha256: h('c'),
                release: "0.1.0+0123456789ab".into(),
            }],
            catalog_entries: vec![OwnedCatalogEntry {
                file: "/home/u/proj/.fl/mcp.toml".into(),
                name: "ferrostate".into(),
                sha256: h('d'),
            }],
            config_blocks: vec![OwnedConfigBlock {
                file: "/home/u/.config/fl/config.toml".into(),
                block_id: "install".into(),
                sha256: h('e'),
            }],
            journal: vec![JournalEntry {
                seq: 1,
                phase: JournalPhase::Committed,
                release: "0.1.0+0123456789ab".into(),
                at_ms: 1,
            }],
        }
    }

    #[test]
    fn legacy_ferrostep_receipt_is_read_without_retaining_a_retired_product() {
        let mut value = serde_json::to_value(full_receipt()).unwrap();
        value["products"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("ferrostep"));
        let parsed = InstallReceipt::parse(&value.to_string()).unwrap();
        assert!(!parsed.to_json().contains("ferrostep"));
        assert!(parsed.products.contains(&Product::Ferroloop));
    }

    #[test]
    fn a_receipt_round_trips_byte_for_byte() {
        let r = full_receipt();
        let json = r.to_json();
        let back = InstallReceipt::parse(&json).expect("parses");
        assert_eq!(back, r);
        assert_eq!(back.to_json(), json);
    }

    fn collect_keys(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, v) in m {
                    out.push(k.clone());
                    collect_keys(v, out);
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|v| collect_keys(v, out)),
            _ => {}
        }
    }

    #[test]
    fn no_receipt_field_can_hold_a_credential() {
        let value = serde_json::to_value(full_receipt()).unwrap();
        let mut keys = Vec::new();
        collect_keys(&value, &mut keys);
        assert!(
            keys.len() > 30,
            "the scan must see the whole schema: {keys:?}"
        );
        for k in &keys {
            for word in [
                "key",
                "secret",
                "token",
                "password",
                "passwd",
                "credential",
                "private",
                "auth",
                "bearer",
                "cookie",
            ] {
                assert!(
                    !k.to_ascii_lowercase().contains(word),
                    "receipt field `{k}` looks like a credential slot (`{word}`)"
                );
            }
        }
    }

    #[test]
    fn a_secret_cannot_be_smuggled_in_through_an_unknown_field() {
        let base = serde_json::to_value(full_receipt()).unwrap();
        // At the top level and inside every nested record type.
        let targets: [&[&str]; 8] = [
            &[],
            &["release"],
            &["layout"],
            &["artifacts", "0"],
            &["paths", "0"],
            &["aliases", "0"],
            &["units", "0"],
            &["journal", "0"],
        ];
        for path in targets {
            for field in ["api_key", "token", "vault_key", "password"] {
                let mut v = base.clone();
                let mut at = &mut v;
                for seg in path {
                    at = match seg.parse::<usize>() {
                        Ok(i) => &mut at[i],
                        Err(_) => &mut at[*seg],
                    };
                }
                at.as_object_mut()
                    .unwrap()
                    .insert(field.into(), "hunter2".into());
                let err = InstallReceipt::parse(&v.to_string()).unwrap_err();
                assert!(
                    matches!(err, ReceiptError::Malformed(ref m) if m.contains("unknown field")),
                    "{path:?} accepted `{field}`: {err}"
                );
            }
        }
    }

    #[test]
    fn receipt_validation_table() {
        type Mutate = fn(&mut InstallReceipt);
        // (the field the refusal must name, the corruption)
        let cases: Vec<(&str, Mutate)> = vec![
            ("artifacts.sha256", |r| r.artifacts[0].sha256 = "xyz".into()),
            ("paths.path", |r| r.paths[0].path = "/a/../b".into()),
            ("paths.path", |r| r.paths[0].path = "a/b".into()),
            ("units.path", |r| r.units[0].path = "/a\0b".into()),
            ("paths.sha256", |r| r.paths[0].sha256 = None),
            ("paths.link_target", |r| r.paths[1].link_target = None),
            ("release.name", |r| r.release.name = "a/b".into()),
            ("release.name", |r| r.release.name = "..".into()),
            ("release.version", |r| r.release.version = "1 0".into()),
            ("release.commit", |r| r.release.commit = Some("abc".into())),
            ("artifacts.id", |r| r.artifacts[0].id = "a/b".into()),
            ("artifacts.id", |r| r.artifacts[0].id = "-rf".into()),
            ("aliases.name", |r| r.aliases[0].name = "..".into()),
            ("aliases.name", |r| r.aliases[0].name = "x".repeat(81)),
            ("aliases.target", |r| r.aliases[0].target = "a\0b".into()),
            ("aliases.target", |r| r.aliases[0].target = "x".repeat(2000)),
            ("paths.link_target", |r| {
                r.paths[1].link_target = Some("a\nb".into())
            }),
            ("units.name", |r| r.units[0].name = "a b.service".into()),
            ("units.release", |r| r.units[0].release = "a/b".into()),
            ("catalog_entries.name", |r| {
                r.catalog_entries[0].name = "a\0".into()
            }),
            ("catalog_entries.file", |r| {
                r.catalog_entries[0].file = "rel/x".into()
            }),
            ("config_blocks.block_id", |r| {
                r.config_blocks[0].block_id = "a/b".into()
            }),
            ("config_blocks.file", |r| {
                r.config_blocks[0].file = "/a/../b".into()
            }),
            ("journal.release", |r| r.journal[0].release = "x".repeat(81)),
        ];
        for (field, mutate) in cases {
            let mut r = full_receipt();
            mutate(&mut r);
            let err = r.validate().expect_err(field);
            assert!(
                err.to_string().contains(&format!("`{field}`")),
                "refused for the wrong reason (wanted `{field}`): {err}"
            );
        }
        let mut r = full_receipt();
        r.schema_version = 3;
        assert!(matches!(
            InstallReceipt::parse(&r.to_json()),
            Err(ReceiptError::UnsupportedSchema { found: 3 })
        ));
        assert!(matches!(
            InstallReceipt::parse(&" ".repeat(MAX_RECEIPT_BYTES + 1)),
            Err(ReceiptError::TooLarge { .. })
        ));
    }

    // ---- schema 1 and 2 (#205 A4b)

    fn build_block() -> BuildProvenance {
        BuildProvenance {
            reviewed: false,
            rustc: "rustc 1.98.0 (abc 2026-01-01)".into(),
            cargo: "cargo 1.98.0 (def 2026-01-01)".into(),
            build_id: "0123456789ab-fedcba987654".into(),
            build_time_unix: 1_700_000_000,
        }
    }

    fn source_receipt() -> InstallReceipt {
        let mut r = full_receipt();
        r.release.kind = ReleaseKind::SourceBuild;
        r.release.build = Some(build_block());
        r.schema_version = RECEIPT_SCHEMA_SOURCE_BUILD;
        r
    }

    #[test]
    fn a_receipt_with_a_build_block_is_schema_2_and_round_trips() {
        let r = source_receipt();
        let json = r.to_json();
        assert!(json.contains("\"schema_version\": 2"));
        let back = InstallReceipt::parse(&json).expect("parses");
        assert_eq!(back, r);
        assert_eq!(back.to_json(), json);
        assert!(!back.release.build.unwrap().reviewed);
    }

    #[test]
    fn a_version_1_receipt_reads_and_writes_as_it_always_did() {
        // An A4a receipt: no `build` key anywhere.
        let old = full_receipt();
        assert_eq!(old.release.schema_version(), RECEIPT_SCHEMA_VERSION);
        let json = old.to_json();
        assert!(!json.contains("\"build\""));
        assert_eq!(InstallReceipt::parse(&json).unwrap(), old);
    }

    #[test]
    fn schema_and_build_block_must_agree() {
        // version 1 carrying the block
        let mut r = source_receipt();
        r.schema_version = RECEIPT_SCHEMA_VERSION;
        assert!(matches!(
            InstallReceipt::parse(&r.to_json()),
            Err(ReceiptError::Bad {
                field: "schema_version",
                ..
            })
        ));
        // version 2 without it
        let mut r = full_receipt();
        r.schema_version = RECEIPT_SCHEMA_SOURCE_BUILD;
        assert!(matches!(
            InstallReceipt::parse(&r.to_json()),
            Err(ReceiptError::Bad {
                field: "schema_version",
                ..
            })
        ));
        // a block on a signed release
        let mut r = source_receipt();
        r.release.kind = ReleaseKind::Release;
        assert!(matches!(
            InstallReceipt::parse(&r.to_json()),
            Err(ReceiptError::Bad {
                field: "release.build",
                ..
            })
        ));
        // a block without a commit
        let mut r = source_receipt();
        r.release.commit = None;
        assert!(matches!(
            InstallReceipt::parse(&r.to_json()),
            Err(ReceiptError::Bad {
                field: "release.build",
                ..
            })
        ));
        // unbounded text in the block
        let mut r = source_receipt();
        r.release.build.as_mut().unwrap().rustc = "x\ny".into();
        assert!(matches!(
            InstallReceipt::parse(&r.to_json()),
            Err(ReceiptError::Bad {
                field: "release.build",
                ..
            })
        ));
        // a key the block does not have
        let mut v = serde_json::to_value(source_receipt()).unwrap();
        v["release"]["build"]["token"] = "hunter2".into();
        assert!(InstallReceipt::parse(&v.to_string()).is_err());
    }
}
