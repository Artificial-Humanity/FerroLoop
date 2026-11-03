//! `G7.path-ownership`: every path the receipt owns is still what the receipt says.

use super::{CheckStatus, DoctorCheck, DoctorInputs, ReceiptView, ids, joined, make};
use crate::install::{BUDGET_EXCEEDED, OwnedPathKind, PathState};

const TITLE: &str = "Path ownership";
const SHOWN_PROBLEMS: usize = 8;

/// What the receipt says one owned path is.
enum Expect<'a> {
    Dir,
    File { sha256: &'a str },
    Symlink { target: &'a str },
    Unit { sha256: &'a str },
}

struct Entry<'a> {
    kind: &'static str,
    path: &'a str,
    expect: Expect<'a>,
}

pub(super) fn check(inputs: &DoctorInputs<'_>, receipt: &ReceiptView) -> DoctorCheck {
    let (status, detail) = match receipt {
        ReceiptView::Absent => (
            CheckStatus::NotInstalled,
            "unmanaged: there is no install receipt to compare paths against".to_string(),
        ),
        ReceiptView::Valid(r) => {
            let mut entries: Vec<Entry<'_>> = Vec::new();
            for p in &r.paths {
                let expect = match p.kind {
                    OwnedPathKind::Dir => Expect::Dir,
                    OwnedPathKind::File => Expect::File {
                        sha256: p.sha256.as_deref().unwrap_or(""),
                    },
                    OwnedPathKind::Symlink => Expect::Symlink {
                        target: p.link_target.as_deref().unwrap_or(""),
                    },
                };
                entries.push(Entry {
                    kind: "path",
                    path: &p.path,
                    expect,
                });
            }
            for a in &r.aliases {
                entries.push(Entry {
                    kind: "alias",
                    path: &a.path,
                    expect: Expect::Symlink { target: &a.target },
                });
            }
            for u in &r.units {
                entries.push(Entry {
                    kind: "unit",
                    path: &u.path,
                    expect: Expect::Unit { sha256: &u.sha256 },
                });
            }
            entries.sort_by(|a, b| (a.path, a.kind).cmp(&(b.path, b.kind)));
            if entries.is_empty() {
                (
                    CheckStatus::Warn,
                    "the receipt lists no owned paths, so there is nothing to verify".to_string(),
                )
            } else {
                evaluate(
                    inputs,
                    &entries,
                    r.catalog_entries.len() + r.config_blocks.len(),
                )
            }
        }
        // The receipt cannot be trusted, so nothing can be compared with it.
        ReceiptView::Invalid(_) | ReceiptView::Unreadable(_) | ReceiptView::Unlocatable(_) => (
            CheckStatus::Unsupported,
            "cannot check paths: the install receipt is not usable (see G7.receipt-integrity)"
                .to_string(),
        ),
    };
    make(ids::PATH_OWNERSHIP, TITLE, status, detail)
}

fn evaluate(
    inputs: &DoctorInputs<'_>,
    entries: &[Entry<'_>],
    unchecked_blocks: usize,
) -> (CheckStatus, String) {
    let mut fails: Vec<String> = Vec::new();
    let mut warns: Vec<String> = Vec::new();
    for e in entries {
        let state = inputs.probe.probe(e.path);
        let label = format!("{} {}", e.kind, e.path);
        match judge(&e.expect, &state) {
            Verdict::Ok => {}
            Verdict::Warn(why) => warns.push(format!("{label} {why}")),
            Verdict::Fail(why) => fails.push(format!("{label} {why}")),
        }
    }
    let checked = entries.len();
    let tail = if unchecked_blocks > 0 {
        format!(
            "; {unchecked_blocks} recorded catalog entries and config blocks are not compared in this release"
        )
    } else {
        String::new()
    };
    if !fails.is_empty() {
        let mut all = fails.clone();
        all.extend(warns.iter().cloned());
        (
            CheckStatus::Fail,
            format!(
                "{} of {checked} owned entries failed: {}{tail}",
                fails.len(),
                joined(&all, SHOWN_PROBLEMS)
            ),
        )
    } else if !warns.is_empty() {
        (
            CheckStatus::Warn,
            format!(
                "{} of {checked} owned entries differ from the receipt: {}{tail}",
                warns.len(),
                joined(&warns, SHOWN_PROBLEMS)
            ),
        )
    } else {
        (
            CheckStatus::Pass,
            format!("all {checked} owned entries match the receipt{tail}"),
        )
    }
}

enum Verdict {
    Ok,
    /// Edited: still the right kind, different content or target.
    Warn(String),
    /// Missing, foreign or the wrong kind.
    Fail(String),
}

fn judge(expect: &Expect<'_>, state: &PathState) -> Verdict {
    use Verdict::{Fail, Ok, Warn};
    if let PathState::Unreadable { reason } = state {
        return if reason == BUDGET_EXCEEDED {
            Warn("was not checked: the time budget ran out".into())
        } else {
            Fail(format!("cannot be read ({reason})"))
        };
    }
    if matches!(state, PathState::Missing) {
        return Fail("is missing".into());
    }
    match expect {
        Expect::Dir => match state {
            PathState::Dir => Ok,
            other => Fail(format!("should be a directory but is {}", describe(other))),
        },
        Expect::File { sha256 } => match state {
            PathState::File(fp) if fp.sha256 == *sha256 => Ok,
            PathState::File(_) => Warn("was edited (its hash differs from the receipt)".into()),
            other => Fail(format!("should be a file but is {}", describe(other))),
        },
        Expect::Symlink { target } => match state {
            PathState::Symlink { target: found } if found == target => Ok,
            PathState::Symlink { target: found } => Warn(format!(
                "was edited (it points to {found}, the receipt says {target})"
            )),
            other => Fail(format!("should be a symlink but is {}", describe(other))),
        },
        Expect::Unit { sha256 } => match state {
            PathState::File(fp) if fp.sha256 == *sha256 => Ok,
            PathState::File(fp) if fp.has_unit_marker => {
                Warn("was edited (its hash differs from the receipt)".into())
            }
            PathState::File(_) => {
                Fail("is foreign (no generated-by marker and a different hash)".into())
            }
            other => Fail(format!("should be a unit file but is {}", describe(other))),
        },
    }
}

fn describe(state: &PathState) -> &'static str {
    match state {
        PathState::Missing => "missing",
        PathState::Dir => "a directory",
        PathState::File(_) => "a file",
        PathState::Symlink { .. } => "a symlink",
        PathState::Other => "neither a file, directory nor symlink",
        PathState::Unreadable { .. } => "unreadable",
    }
}
