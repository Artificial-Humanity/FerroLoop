//! What a binary says about how it was built: the commit, the dirty flag, the build time.
//!
//! The stamps come from a `build.rs` (`crates/core/build/stamp.rs`) and reach a crate as the
//! `FL_BUILD_*` environment values [`build_info!`] reads. `fl`, `ferrostore` and the hub
//! report the same shape.
//!
//! A source build never poses as a release: [`BuildInfo::source`] is `source-build`, and a
//! dirty tree says so. No release channel exists yet, so [`SOURCE_BUILD`] is the only value.

use serde::{Deserialize, Serialize};

/// [`BuildInfo::source`] of a build made from a source tree.
pub const SOURCE_BUILD: &str = "source-build";
/// What a stamp holds when it could not be read.
pub const UNKNOWN: &str = "unknown";
/// Hex digits of the commit shown in the short form.
pub const SHORT_COMMIT_LEN: usize = 12;

/// How a binary was built. All fields serialise, `null` when unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    /// The crate version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// The full commit the build was made from, or `unknown`.
    pub commit: String,
    /// The first [`SHORT_COMMIT_LEN`] digits of `commit`, or `unknown`.
    pub commit_short: String,
    /// Whether the tree had changes the commit does not hold; `None` when git could not say.
    pub dirty: Option<bool>,
    /// Build time, unix seconds (`SOURCE_DATE_EPOCH` when it was set).
    pub build_time_unix: Option<u64>,
    /// Always [`SOURCE_BUILD`] for now.
    pub source: String,
    /// The release id, set at build time through `$FL_RELEASE_ID`; `None` for a plain build.
    pub release_id: Option<String>,
}

impl BuildInfo {
    /// Read the raw stamp strings: a malformed value becomes unknown, never an error.
    pub fn from_stamp(
        version: &str,
        commit: &str,
        dirty: &str,
        build_time: &str,
        release_id: Option<&str>,
    ) -> Self {
        let commit = commit.trim();
        let valid = matches!(commit.len(), 40 | 64)
            && commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let (commit, commit_short) = if valid {
            (commit.to_string(), commit[..SHORT_COMMIT_LEN].to_string())
        } else {
            (UNKNOWN.to_string(), UNKNOWN.to_string())
        };
        Self {
            version: version.to_string(),
            commit,
            commit_short,
            dirty: match dirty.trim() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            build_time_unix: build_time.trim().parse().ok(),
            source: SOURCE_BUILD.to_string(),
            release_id: release_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        }
    }

    /// Whether the commit is known.
    pub fn commit_known(&self) -> bool {
        self.commit != UNKNOWN
    }

    /// The parenthesised build note of a version line: `abc123456789`, `abc123456789, dirty`,
    /// `unknown commit`; a dirty state git could not read is `dirty unknown`.
    pub fn note(&self) -> String {
        let mut parts = Vec::new();
        if self.commit_known() {
            parts.push(self.commit_short.clone());
            match self.dirty {
                Some(true) => parts.push("dirty".to_string()),
                Some(false) => {}
                None => parts.push("dirty unknown".to_string()),
            }
        } else {
            parts.push("unknown commit".to_string());
        }
        if let Some(id) = &self.release_id {
            parts.push(format!("release {id}"));
        }
        parts.join(", ")
    }

    /// The one human line: `fl 0.1.0 (abc123456789, dirty)`.
    pub fn human_line(&self, name: &str) -> String {
        format!("{name} {} ({})", self.version, self.note())
    }

    /// `name` plus every field, as the JSON object `--version --json` prints.
    pub fn to_json(&self, name: &str) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let serde_json::Value::Object(o) = &mut v {
            o.insert("name".into(), name.into());
        }
        v
    }
}

/// The [`BuildInfo`] of the crate that expands it, from the `FL_BUILD_*` values its
/// `build.rs` (`stamp::emit`) set. A crate with no such build script does not
/// compile.
#[macro_export]
macro_rules! build_info {
    () => {
        $crate::build_info::BuildInfo::from_stamp(
            env!("CARGO_PKG_VERSION"),
            env!("FL_BUILD_COMMIT"),
            env!("FL_BUILD_DIRTY"),
            env!("FL_BUILD_TIME"),
            option_env!("FL_RELEASE_ID"),
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const C: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_clean_stamp_parses() {
        let b = BuildInfo::from_stamp("0.1.0", C, "false", "1700000000", None);
        assert_eq!(b.commit, C);
        assert_eq!(b.commit_short, "0123456789ab");
        assert_eq!(
            (b.dirty, b.build_time_unix, b.release_id.as_deref()),
            (Some(false), Some(1_700_000_000), None)
        );
        assert_eq!(b.source, "source-build");
        assert_eq!(b.human_line("fl"), "fl 0.1.0 (0123456789ab)");
    }

    #[test]
    fn a_dirty_tree_says_dirty_and_is_still_a_source_build() {
        let b = BuildInfo::from_stamp("0.1.0", C, "true", "1", None);
        assert_eq!(b.dirty, Some(true));
        assert_eq!(b.source, SOURCE_BUILD);
        assert_eq!(b.human_line("fl"), "fl 0.1.0 (0123456789ab, dirty)");
    }

    #[test]
    fn unknown_and_malformed_stamps_degrade_without_failing() {
        let b = BuildInfo::from_stamp("0.1.0", "unknown", "unknown", "unknown", None);
        assert_eq!(
            (
                b.commit.as_str(),
                b.commit_short.as_str(),
                b.dirty,
                b.build_time_unix
            ),
            ("unknown", "unknown", None, None)
        );
        assert_eq!(b.human_line("fl"), "fl 0.1.0 (unknown commit)");
        // A short or upper-case "commit" is not one.
        for bad in ["", "abc", &C.to_uppercase(), &format!("{C}ff")] {
            let b = BuildInfo::from_stamp("0.1.0", bad, "maybe", "-5", None);
            assert_eq!(
                (b.commit.as_str(), b.dirty, b.build_time_unix),
                ("unknown", None, None)
            );
        }
        // A commit with no readable dirty flag is shown as such, not as clean.
        let b = BuildInfo::from_stamp("0.1.0", C, "unknown", "1", None);
        assert_eq!(b.note(), "0123456789ab, dirty unknown");
    }

    #[test]
    fn a_release_id_is_shown_but_the_source_stays_a_source_build() {
        let b = BuildInfo::from_stamp("0.1.0", C, "false", "1", Some(" 2026.11.1 "));
        assert_eq!(b.release_id.as_deref(), Some("2026.11.1"));
        assert_eq!(b.source, SOURCE_BUILD);
        assert_eq!(b.note(), "0123456789ab, release 2026.11.1");
        assert_eq!(
            BuildInfo::from_stamp("0.1.0", C, "false", "1", Some(" ")).release_id,
            None
        );
    }

    #[test]
    fn json_has_every_key_and_round_trips() {
        let b = BuildInfo::from_stamp("0.1.0", C, "true", "42", None);
        let v = b.to_json("fl");
        assert_eq!(
            v,
            json!({
                "name": "fl",
                "version": "0.1.0",
                "commit": C,
                "commit_short": "0123456789ab",
                "dirty": true,
                "build_time_unix": 42,
                "source": "source-build",
                "release_id": null,
            })
        );
        let back: BuildInfo = serde_json::from_value(v).unwrap();
        assert_eq!(back, b);
        // Unknowns serialise as null, not as missing keys.
        let u = BuildInfo::from_stamp("0.1.0", "x", "x", "x", None).to_json("fl");
        assert_eq!(u["dirty"], json!(null));
        assert_eq!(u["build_time_unix"], json!(null));
    }
}
