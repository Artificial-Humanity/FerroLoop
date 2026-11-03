//! The install transaction's pure model (#205 phase A4a): the steps, the journal lines, the
//! intent a transaction records before it changes anything, the receipt it ends with, and the
//! retention report. Nothing here reads or writes the file system; `fl-install` does that.
//!
//! A transaction is an ordered list of [`Step`]s. Before each step runs, one [`JournalLine`]
//! naming it is appended to `journal.jsonl` and flushed. The first line (`begin`) carries the
//! whole [`TxnIntent`]: everything a later run needs to finish the transaction or to undo it,
//! with no input but the journal and the disk.

use super::plan::join;
use super::receipt::{
    InstallReceipt, JournalEntry, JournalPhase, OwnedAlias, OwnedArtifact, OwnedPath,
    OwnedPathKind, ReceiptError, ReceiptLayout, ReceiptRelease,
};
use super::{Product, Profile};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// `schema_version` of `journal.jsonl` lines (`begin` carries it implicitly: a line this
/// release does not parse is refused, never guessed at).
pub const JOURNAL_SCHEMA_VERSION: u32 = 1;
/// The most committed transactions the receipt's journal keeps.
pub const MAX_COMMITTED_ENTRIES: usize = 256;
/// Releases kept besides the current one (spec §4).
pub const RETAINED_PREVIOUS_RELEASES: usize = 2;
/// The file the installer writes into every release directory.
pub const RELEASE_JSON: &str = "release.json";

/// One step of the install transaction, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// The journal exists and holds the intent.
    Begin,
    /// A path the user chose to replace is moved to `backups/<n>/`.
    Backup,
    /// The release is copied into `releases/.stage-<txn>/`.
    Stage,
    /// The staged tree is hashed again and compared with the manifest's hashes; then sealed.
    Verify,
    /// The staged directory becomes `releases/<name>/` (one `rename(2)`).
    Publish,
    /// `current` is switched (`rename(2)` of a new symlink).
    LinkCurrent,
    /// The stable launcher is written.
    WriteLauncher,
    /// The owned aliases are written.
    WriteAliases,
    /// `receipt.json` is replaced atomically (temp file, fsync, rename).
    WriteReceipt,
    /// The transaction is recorded as committed and the journal is closed.
    Commit,
}

crate::wire::wire_names!(Step as step_wire {
    Begin => "begin",
    Backup => "backup",
    Stage => "stage",
    Verify => "verify",
    Publish => "publish",
    LinkCurrent => "link_current",
    WriteLauncher => "write_launcher",
    WriteAliases => "write_aliases",
    WriteReceipt => "write_receipt",
    Commit => "commit",
});
crate::wire::wire_parse!(Step as step_parse);

/// A file of the staged release, as the manifest's hashes promise it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentFile {
    /// Relative to the release directory.
    pub path: String,
    pub sha256: String,
    pub size: u64,
    /// A binary: sealed `0555` instead of `0444`.
    pub exec: bool,
}

/// A symlink the transaction places (the launcher, an alias).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkSpec {
    pub name: String,
    /// The path as the plan shows it.
    pub path: String,
    pub target: String,
    /// What the symlink pointed to before (an owned link being moved), if it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    /// `false`: already right and owned; recorded in the receipt, not written.
    pub write: bool,
}

/// A path the user chose to replace: it is moved to `backups/<n>/` first, and the receipt
/// then owns the backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupSpec {
    /// Where it was, as the plan shows it.
    pub path: String,
    /// Where it goes, as the plan shows it.
    pub to: String,
    pub kind: OwnedPathKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
}

/// Everything a transaction decides before it changes anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TxnIntent {
    pub release: ReceiptRelease,
    pub profile: Profile,
    pub products: Vec<Product>,
    pub layout: ReceiptLayout,
    pub artifacts: Vec<OwnedArtifact>,
    /// Every file of the release (including `release.json`), sorted by path.
    pub files: Vec<IntentFile>,
    /// The steps this transaction runs, in order.
    pub steps: Vec<Step>,
    /// The release directory was already there (and owned): nothing is staged.
    pub release_existed: bool,
    /// Where `current` pointed before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_current: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher: Option<LinkSpec>,
    #[serde(default)]
    pub aliases: Vec<LinkSpec>,
    /// Paths moved aside by `--replace`, in the order they are moved.
    #[serde(default)]
    pub backups: Vec<BackupSpec>,
    /// Directories that did not exist when the transaction began and that it created
    /// (canonical paths).
    #[serde(default)]
    pub created_dirs: Vec<String>,
    /// `install.lock` did not exist and this transaction created it.
    #[serde(default)]
    pub lock_created: bool,
    /// The receipt this transaction replaces, byte for byte (`None`: there was none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_receipt: Option<String>,
}

/// The operation a journal line announces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum JournalOp {
    Begin {
        intent: Box<TxnIntent>,
    },
    /// `intent.backups[index]` is moved aside.
    Backup {
        index: u32,
    },
    Stage,
    Verify,
    Publish,
    /// The rename of the staging directory to `releases/<name>` happened in THIS transaction.
    Published,
    LinkCurrent,
    WriteLauncher,
    WriteAliases,
    WriteReceipt,
    Commit,
    /// An undo began (after a failure, or by `--resume` / `--rollback`).
    RollbackBegin,
    /// Every undo finished.
    RollbackDone,
}

impl JournalOp {
    /// The step this line announces, if it announces one.
    pub fn step(&self) -> Option<Step> {
        Some(match self {
            JournalOp::Begin { .. } => Step::Begin,
            JournalOp::Backup { .. } => Step::Backup,
            JournalOp::Stage => Step::Stage,
            JournalOp::Verify => Step::Verify,
            JournalOp::Publish => Step::Publish,
            JournalOp::LinkCurrent => Step::LinkCurrent,
            JournalOp::WriteLauncher => Step::WriteLauncher,
            JournalOp::WriteAliases => Step::WriteAliases,
            JournalOp::WriteReceipt => Step::WriteReceipt,
            JournalOp::Commit => Step::Commit,
            JournalOp::Published | JournalOp::RollbackBegin | JournalOp::RollbackDone => {
                return None;
            }
        })
    }
}

/// One line of `journal.jsonl`. There is no clock: `seq` orders the lines and `txn` is the
/// transaction counter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalLine {
    pub v: u32,
    pub txn: u64,
    pub seq: u64,
    #[serde(flatten)]
    pub op: JournalOp,
}

impl JournalLine {
    /// The line as one JSON object and a newline.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("journal line serializes");
        s.push('\n');
        s
    }
}

// ------------------------------------------------------------ tree digest

/// The digest a manifest pins for a directory artifact: SHA-256 over one line per regular
/// file, sorted by path: `<sha256> <size> <path>\n`. Directories carry no data and symlinks
/// are refused before this is computed, so the digest covers exactly the content.
pub fn tree_digest(files: &[(String, String, u64)]) -> String {
    let mut sorted: Vec<&(String, String, u64)> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = Sha256::new();
    for (path, sha, size) in sorted {
        h.update(format!("{sha} {size} {path}\n").as_bytes());
    }
    hex(&h.finalize())
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of `bytes`, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

// ----------------------------------------------------------- release.json

/// The bytes of `release.json`: deterministic, no clock, no transaction id.
pub fn release_json(release: &ReceiptRelease, artifacts: &[OwnedArtifact]) -> String {
    let mut artifacts: Vec<serde_json::Value> = artifacts
        .iter()
        .map(|a| serde_json::json!({ "id": a.id, "sha256": a.sha256 }))
        .collect();
    artifacts.sort_by_key(|v| v["id"].as_str().unwrap_or_default().to_string());
    let value = serde_json::json!({
        "schema_version": 1,
        "name": release.name,
        "version": release.version,
        "kind": release.kind,
        "commit": release.commit,
        "artifacts": artifacts,
    });
    let mut value = value;
    if let Some(b) = &release.build {
        // A source build says how it was made; a signed release's bytes stay as they were.
        value["build"] = serde_json::json!({
            "reviewed": b.reviewed,
            "rustc": b.rustc,
            "cargo": b.cargo,
            "build_id": b.build_id,
            "build_time_unix": b.build_time_unix,
        });
    }
    let mut out = serde_json::to_string_pretty(&value).expect("release.json serializes");
    out.push('\n');
    out
}

// ---------------------------------------------------------------- receipt

fn parent_dirs(rel: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut acc = String::new();
    let mut segs: Vec<&str> = rel.split('/').collect();
    segs.pop();
    for s in segs {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(s);
        out.push(acc.clone());
    }
    out
}

/// The receipt a committed transaction leaves: the previous receipt's ownership (earlier
/// releases, backups, aliases, units) kept, `current` and the launcher moved to this release,
/// this release's files added with their hashes, and one `committed` journal entry. Pure and
/// deterministic: the same inputs give the same bytes.
pub fn build_receipt(
    prev: Option<&InstallReceipt>,
    intent: &TxnIntent,
    txn: u64,
) -> Result<InstallReceipt, ReceiptError> {
    let layout = &intent.layout;
    let releases = join(&layout.prefix, "releases");
    let base = join(&releases, &intent.release.name);
    let current = join(&layout.prefix, "current");
    let launcher_path = join(&layout.bin_dir, "fl");

    // Ownership carried over from the receipt this one replaces.
    // A release written by this transaction replaces whatever the old receipt said about the
    // same directory; one that already existed keeps its recorded files.
    let base_prefix = format!("{base}/");
    let mut paths: Vec<OwnedPath> = prev
        .map(|r| {
            r.paths
                .iter()
                .filter(|p| p.path != current && p.path != launcher_path)
                .filter(|p| {
                    intent.release_existed || (p.path != base && !p.path.starts_with(&base_prefix))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let new_paths = {
        let mut v: Vec<OwnedPath> = Vec::new();
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for f in &intent.files {
            for d in parent_dirs(&f.path) {
                dirs.insert(d);
            }
        }
        v.push(OwnedPath {
            path: base.clone(),
            kind: OwnedPathKind::Dir,
            sha256: None,
            link_target: None,
        });
        for d in dirs {
            v.push(OwnedPath {
                path: join(&base, &d),
                kind: OwnedPathKind::Dir,
                sha256: None,
                link_target: None,
            });
        }
        for f in &intent.files {
            v.push(OwnedPath {
                path: join(&base, &f.path),
                kind: OwnedPathKind::File,
                sha256: Some(f.sha256.clone()),
                link_target: None,
            });
        }
        v.push(OwnedPath {
            path: current,
            kind: OwnedPathKind::Symlink,
            sha256: None,
            link_target: Some(format!("releases/{}", intent.release.name)),
        });
        if let Some(l) = &intent.launcher {
            v.push(OwnedPath {
                path: l.path.clone(),
                kind: OwnedPathKind::Symlink,
                sha256: None,
                link_target: Some(l.target.clone()),
            });
        }
        for b in &intent.backups {
            v.push(OwnedPath {
                path: b.to.clone(),
                kind: b.kind,
                sha256: b.sha256.clone(),
                link_target: b.link_target.clone(),
            });
        }
        v
    };
    let replaced: BTreeSet<&str> = new_paths.iter().map(|p| p.path.as_str()).collect();
    paths.retain(|p| !replaced.contains(p.path.as_str()));
    // A launcher the user chose to keep stays unowned, but one a previous install owned stays.
    paths.extend(new_paths);
    paths.sort_by(|a, b| a.path.cmp(&b.path));

    let mut aliases: Vec<OwnedAlias> = intent
        .aliases
        .iter()
        .map(|a| OwnedAlias {
            name: a.name.clone(),
            path: a.path.clone(),
            target: a.target.clone(),
        })
        .collect();
    if let Some(r) = prev {
        for a in &r.aliases {
            if !aliases.iter().any(|n| n.path == a.path) {
                aliases.push(a.clone());
            }
        }
    }
    aliases.sort_by(|a, b| a.path.cmp(&b.path));

    let mut journal: Vec<JournalEntry> = prev.map(|r| r.journal.clone()).unwrap_or_default();
    journal.push(JournalEntry {
        seq: txn,
        phase: JournalPhase::Committed,
        release: intent.release.name.clone(),
        at_ms: 0,
    });
    if journal.len() > MAX_COMMITTED_ENTRIES {
        let cut = journal.len() - MAX_COMMITTED_ENTRIES;
        journal.drain(..cut);
    }

    let mut products = intent.products.clone();
    products.sort();
    products.dedup();
    let mut artifacts = intent.artifacts.clone();
    artifacts.sort_by(|a, b| a.id.cmp(&b.id));

    let receipt = InstallReceipt {
        schema_version: intent.release.schema_version(),
        release: intent.release.clone(),
        profile: intent.profile,
        products,
        layout: intent.layout.clone(),
        artifacts,
        paths,
        aliases,
        units: prev.map(|r| r.units.clone()).unwrap_or_default(),
        catalog_entries: prev.map(|r| r.catalog_entries.clone()).unwrap_or_default(),
        config_blocks: prev.map(|r| r.config_blocks.clone()).unwrap_or_default(),
        journal,
    };
    receipt.validate()?;
    Ok(receipt)
}

/// The next transaction id: one above the newest committed entry (1 for a first install).
pub fn next_txn(prev: Option<&InstallReceipt>) -> u64 {
    prev.and_then(|r| {
        r.journal
            .iter()
            .filter(|j| j.phase == JournalPhase::Committed)
            .map(|j| j.seq)
            .max()
    })
    .map_or(1, |m| m + 1)
}

/// Whether `receipt` already records transaction `txn` as committed.
pub fn has_committed(receipt: &InstallReceipt, txn: u64) -> bool {
    receipt
        .journal
        .iter()
        .any(|j| j.phase == JournalPhase::Committed && j.seq == txn)
}

// -------------------------------------------------------------- retention

/// Which releases the retention rule keeps and which a later `--gc` could clear.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Retention {
    /// `current` first, then up to two previous releases, newest first.
    pub keep: Vec<String>,
    /// Older releases. Nothing clears them in this release; they are only reported.
    pub gc_eligible: Vec<String>,
}

/// Apply the retention rule (`current` plus the two previous releases) to the releases a
/// receipt owns. "Previous" is the commit order recorded in the receipt's journal.
pub fn retention(receipt: &InstallReceipt) -> Retention {
    let releases_dir = format!("{}/releases/", receipt.layout.prefix.trim_end_matches('/'));
    let owned: BTreeSet<String> = receipt
        .paths
        .iter()
        .filter(|p| p.kind == OwnedPathKind::Dir)
        .filter_map(|p| p.path.strip_prefix(&releases_dir))
        .filter(|rest| !rest.contains('/'))
        .map(str::to_string)
        .collect();
    let mut ordered: Vec<String> = vec![receipt.release.name.clone()];
    for j in receipt.journal.iter().rev() {
        if j.phase == JournalPhase::Committed
            && owned.contains(&j.release)
            && !ordered.contains(&j.release)
        {
            ordered.push(j.release.clone());
        }
    }
    for name in &owned {
        if !ordered.contains(name) {
            ordered.push(name.clone());
        }
    }
    let n = (1 + RETAINED_PREVIOUS_RELEASES).min(ordered.len());
    let gc_eligible = ordered.split_off(n);
    Retention {
        keep: ordered,
        gc_eligible,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::ReleaseKind;

    fn sha(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn intent(name: &str) -> TxnIntent {
        TxnIntent {
            release: ReceiptRelease {
                version: name.split('+').next().unwrap().into(),
                name: name.into(),
                kind: ReleaseKind::Release,
                commit: Some("0123456789abcdef0123456789abcdef01234567".into()),
                build: None,
            },
            profile: Profile::Minimal,
            products: vec![Product::Ferroloop, Product::Ferrostore],
            layout: ReceiptLayout {
                prefix: "/r/fl".into(),
                bin_dir: "/r/bin".into(),
                unit_dir: "/r/units".into(),
            },
            artifacts: vec![OwnedArtifact {
                id: "a".into(),
                sha256: sha('a'),
            }],
            files: vec![
                IntentFile {
                    path: "bin/fl".into(),
                    sha256: sha('b'),
                    size: 3,
                    exec: true,
                },
                IntentFile {
                    path: "ui/dist/assets/x.js".into(),
                    sha256: sha('c'),
                    size: 4,
                    exec: false,
                },
            ],
            steps: vec![Step::Begin, Step::Commit],
            release_existed: false,
            prev_current: None,
            launcher: Some(LinkSpec {
                name: "fl".into(),
                path: "/r/bin/fl".into(),
                target: "/r/fl/current/bin/fl".into(),
                prev: None,
                write: true,
            }),
            aliases: vec![LinkSpec {
                name: "agent-msg".into(),
                path: "/r/bin/agent-msg".into(),
                target: "fl".into(),
                prev: None,
                write: true,
            }],
            backups: vec![],
            created_dirs: vec![],
            lock_created: false,
            prev_receipt: None,
        }
    }

    #[test]
    fn the_receipt_owns_every_file_dir_link_and_alias_with_its_hash() {
        let r = build_receipt(None, &intent("0.1.0+aaaaaaaaaaaa"), 1).unwrap();
        let by = |p: &str| r.owned_path(p).unwrap_or_else(|| panic!("no {p}"));
        let b = "/r/fl/releases/0.1.0+aaaaaaaaaaaa";
        assert_eq!(by(b).kind, OwnedPathKind::Dir);
        assert_eq!(by(&format!("{b}/ui/dist")).kind, OwnedPathKind::Dir);
        assert_eq!(by(&format!("{b}/ui/dist/assets")).kind, OwnedPathKind::Dir);
        assert_eq!(by(&format!("{b}/bin/fl")).sha256, Some(sha('b')));
        assert_eq!(
            by("/r/fl/current").link_target.as_deref(),
            Some("releases/0.1.0+aaaaaaaaaaaa")
        );
        assert_eq!(
            by("/r/bin/fl").link_target.as_deref(),
            Some("/r/fl/current/bin/fl")
        );
        assert_eq!(r.owned_alias("/r/bin/agent-msg").unwrap().target, "fl");
        assert_eq!(r.journal.len(), 1);
        assert_eq!(r.journal[0].seq, 1);
        assert_eq!(r.journal[0].at_ms, 0, "a receipt carries no clock");
        // Deterministic.
        assert_eq!(
            r.to_json(),
            build_receipt(None, &intent("0.1.0+aaaaaaaaaaaa"), 1)
                .unwrap()
                .to_json()
        );
    }

    #[test]
    fn a_second_release_keeps_the_first_ownership_and_moves_current() {
        let first = build_receipt(None, &intent("0.1.0+aaaaaaaaaaaa"), 1).unwrap();
        let second = build_receipt(
            Some(&first),
            &intent("0.2.0+bbbbbbbbbbbb"),
            next_txn(Some(&first)),
        )
        .unwrap();
        assert!(
            second
                .owned_path("/r/fl/releases/0.1.0+aaaaaaaaaaaa/bin/fl")
                .is_some()
        );
        assert!(
            second
                .owned_path("/r/fl/releases/0.2.0+bbbbbbbbbbbb/bin/fl")
                .is_some()
        );
        assert_eq!(
            second
                .owned_path("/r/fl/current")
                .unwrap()
                .link_target
                .as_deref(),
            Some("releases/0.2.0+bbbbbbbbbbbb")
        );
        assert_eq!(second.journal.len(), 2);
        assert_eq!(second.journal[1].seq, 2);
        assert!(has_committed(&second, 1) && has_committed(&second, 2));
        assert_eq!(next_txn(None), 1);
    }

    #[test]
    fn retention_keeps_current_and_two_previous() {
        let mut r = build_receipt(None, &intent("0.1.0+aaaaaaaaaaaa"), 1).unwrap();
        for (i, n) in [
            "0.2.0+bbbbbbbbbbbb",
            "0.3.0+cccccccccccc",
            "0.4.0+dddddddddddd",
        ]
        .iter()
        .enumerate()
        {
            r = build_receipt(Some(&r), &intent(n), 2 + i as u64).unwrap();
        }
        let ret = retention(&r);
        assert_eq!(
            ret.keep,
            [
                "0.4.0+dddddddddddd",
                "0.3.0+cccccccccccc",
                "0.2.0+bbbbbbbbbbbb"
            ]
        );
        assert_eq!(ret.gc_eligible, ["0.1.0+aaaaaaaaaaaa"]);
    }

    #[test]
    fn the_tree_digest_is_order_independent_and_covers_content_size_and_path() {
        let a = ("a".to_string(), sha('1'), 1u64);
        let b = ("b/c".to_string(), sha('2'), 2u64);
        let d = tree_digest(&[a.clone(), b.clone()]);
        assert_eq!(d, tree_digest(&[b.clone(), a.clone()]));
        assert_ne!(d, tree_digest(&[(a.0.clone(), sha('3'), 1), b.clone()]));
        assert_ne!(d, tree_digest(&[(a.0.clone(), a.1.clone(), 9), b.clone()]));
        assert_ne!(d, tree_digest(&[("z".to_string(), a.1.clone(), 1), b]));
        assert_eq!(d.len(), 64);
    }

    #[test]
    fn journal_lines_round_trip_and_carry_no_clock() {
        let line = JournalLine {
            v: JOURNAL_SCHEMA_VERSION,
            txn: 3,
            seq: 0,
            op: JournalOp::Begin {
                intent: Box::new(intent("0.1.0+aaaaaaaaaaaa")),
            },
        };
        let text = line.to_line();
        assert!(text.ends_with('\n') && !text[..text.len() - 1].contains('\n'));
        let back: JournalLine = serde_json::from_str(&text).unwrap();
        assert_eq!(back, line);
        let simple = JournalLine {
            v: 1,
            txn: 3,
            seq: 1,
            op: JournalOp::LinkCurrent,
        };
        assert_eq!(
            simple.to_line(),
            "{\"v\":1,\"txn\":3,\"seq\":1,\"op\":\"link_current\"}\n"
        );
        assert_eq!(simple.op.step(), Some(Step::LinkCurrent));
        assert_eq!(JournalOp::RollbackDone.step(), None);
    }

    #[test]
    fn release_json_is_deterministic() {
        let i = intent("0.1.0+aaaaaaaaaaaa");
        let a = release_json(&i.release, &i.artifacts);
        let b = release_json(&i.release, &i.artifacts);
        assert_eq!(a, b);
        assert!(a.contains("\"name\": \"0.1.0+aaaaaaaaaaaa\""));
        assert!(a.ends_with('\n'));
    }

    #[test]
    fn release_json_says_how_a_source_build_was_made_and_a_release_is_unchanged() {
        let mut i = intent("0.1.0+aaaaaaaaaaaa");
        let plain = release_json(&i.release, &i.artifacts);
        assert!(!plain.contains("\"build\""), "{plain}");
        i.release.kind = ReleaseKind::SourceBuild;
        i.release.build = Some(crate::install::BuildProvenance {
            reviewed: false,
            rustc: "rustc 1.98.0".into(),
            cargo: "cargo 1.98.0".into(),
            build_id: "aaaaaaaaaaaa-bbbbbbbbbbbb".into(),
            build_time_unix: 7,
        });
        let text = release_json(&i.release, &i.artifacts);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["kind"], "source_build");
        assert_eq!(v["build"]["reviewed"], false);
        assert_eq!(v["build"]["rustc"], "rustc 1.98.0");
        assert_eq!(v["build"]["build_time_unix"], 7);
        assert_eq!(text, release_json(&i.release, &i.artifacts));
        // and the receipt that results is schema 2
        let r = build_receipt(None, &i, 1).unwrap();
        assert_eq!(
            r.schema_version,
            crate::install::RECEIPT_SCHEMA_SOURCE_BUILD
        );
    }
}
