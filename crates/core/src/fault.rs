//! What is wrong with a shared ledger, one variant per remedy (GitHub
//! ledger spec §3.3, §3.5, §7). Every message says what to do.
//!
//! ⚠ None of these clears up by waiting: reading again reads the same
//! damage. `StoreError::is_transient` is false for every one.

/// A shared ledger that is not there, or not as fl wrote it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LedgerFault {
    #[error(
        "the repository {repo} has no GitHub ledger yet: it has no `fl/ledger` branch, and this \
         machine records no anchor for one. Run `fl github ledger init` to set it up"
    )]
    NotSetUp { repo: String },
    #[error(
        "the GitHub ledger of {repo} was deleted: its `fl/ledger` branch is gone, but this \
         machine records its first commit, {root}. fl will not start a new ledger, because a new \
         one would hide the deletion. Restore the branch at a commit that descends from {root}, \
         then retry"
    )]
    Deleted { repo: String, root: String },
    #[error(
        "this machine records no anchor for the GitHub ledger of {repo}, so it cannot check that \
         ledger's history. Import the manifest that `fl github ledger init` committed (`fl \
         manifest import`), or run `fl github ledger init` again"
    )]
    NoAnchor { repo: String },
    #[error(
        "the GitHub ledger of {repo} was rewritten: its head {head} does not descend from \
         {against}, {base} ({how}). Run `fl github ledger verify` to see where its history \
         departs, and find out who rewrote it before trusting it"
    )]
    Rewritten {
        repo: String,
        head: String,
        against: &'static str,
        base: String,
        how: String,
    },
    #[error(
        "the GitHub ledger of {repo} was altered: `{file}` {what} (seen at commit {commit}). A \
         ledger file only ever grows. Run `fl github ledger verify` to find the commit that \
         changed it, and find out who made it before trusting the ledger"
    )]
    Altered {
        repo: String,
        file: String,
        what: String,
        commit: String,
    },
    #[error(
        "the GitHub ledger of {repo} was altered: `{file}` line {line} is about {belongs}, which \
         that directory does not hold (added by commit {commit}). fl writes every line into its \
         own directory, so someone else wrote this one. Run `fl github ledger verify`, and find \
         out who wrote it before trusting the ledger; then `fl github ledger quarantine {file} \
         {line} --by <name> --reason <text>` lets readers skip it. Nothing is ever removed"
    )]
    Misplaced {
        repo: String,
        file: String,
        line: u64,
        belongs: String,
        commit: String,
    },
    #[error(
        "the GitHub ledger of {repo} holds an unreadable line: `{file}` line {line}, added by \
         commit {commit}: {cause}. A newer fl may have written it — then upgrade fl. Otherwise \
         find out who wrote it, and run `fl github ledger quarantine {file} {line} --by <name> \
         --reason <text>` so readers skip it; nothing is ever removed"
    )]
    Unreadable {
        repo: String,
        file: String,
        line: u64,
        commit: String,
        cause: String,
    },
    #[error(
        "the GitHub ledger of {repo} is format `{found}`, and this version of fl reads format \
         1. Upgrade fl to read it"
    )]
    UnknownFormat { repo: String, found: String },
    #[error(
        "the shared ledger answered with an entry that has no id ({detail}). Every published \
         entry carries one, so the ledger was written by hand or is damaged. Run `fl github \
         ledger verify`"
    )]
    Unidentified { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variant's name. ⚠ An exhaustive match: a variant added to
    /// `LedgerFault` does not compile here until it is named, and then
    /// `every_fault` must carry a sample of it.
    fn variant(f: &LedgerFault) -> &'static str {
        match f {
            LedgerFault::NotSetUp { .. } => "not_set_up",
            LedgerFault::Deleted { .. } => "deleted",
            LedgerFault::NoAnchor { .. } => "no_anchor",
            LedgerFault::Rewritten { .. } => "rewritten",
            LedgerFault::Altered { .. } => "altered",
            LedgerFault::Misplaced { .. } => "misplaced",
            LedgerFault::Unreadable { .. } => "unreadable",
            LedgerFault::UnknownFormat { .. } => "unknown_format",
            LedgerFault::Unidentified { .. } => "unidentified",
        }
    }

    /// One sample of every variant, with the remedy its message must name.
    fn every_fault() -> Vec<(LedgerFault, &'static str)> {
        let repo = || "acme/widgets".to_string();
        vec![
            (
                LedgerFault::NotSetUp { repo: repo() },
                "fl github ledger init",
            ),
            (
                LedgerFault::Deleted {
                    repo: repo(),
                    root: "c0".into(),
                },
                "Restore the branch",
            ),
            (LedgerFault::NoAnchor { repo: repo() }, "fl manifest import"),
            (
                LedgerFault::Rewritten {
                    repo: repo(),
                    head: "c9".into(),
                    against: "the ledger's first commit",
                    base: "c0".into(),
                    how: "GitHub compares them as `diverged`".into(),
                },
                "fl github ledger verify",
            ),
            (
                LedgerFault::Altered {
                    repo: repo(),
                    file: "runs/k/1.jsonl".into(),
                    what: "changed after it was closed".into(),
                    commit: "c9".into(),
                },
                "fl github ledger verify",
            ),
            (
                LedgerFault::Misplaced {
                    repo: repo(),
                    file: "runs/k/1.jsonl".into(),
                    line: 3,
                    belongs: "urn:uuid:x".into(),
                    commit: "c9".into(),
                },
                "fl github ledger quarantine runs/k/1.jsonl 3",
            ),
            (
                LedgerFault::Unreadable {
                    repo: repo(),
                    file: "runs/k/1.jsonl".into(),
                    line: 3,
                    commit: "c9".into(),
                    cause: "not JSON".into(),
                },
                "fl github ledger quarantine runs/k/1.jsonl 3",
            ),
            (
                LedgerFault::UnknownFormat {
                    repo: repo(),
                    found: "2".into(),
                },
                "Upgrade fl",
            ),
            (
                LedgerFault::Unidentified {
                    detail: "a run".into(),
                },
                "fl github ledger verify",
            ),
        ]
    }

    // Spec §7: every error says what to do.
    #[test]
    fn every_fault_names_its_remedy() {
        let faults = every_fault();
        let named: std::collections::BTreeSet<&str> =
            faults.iter().map(|(f, _)| variant(f)).collect();
        assert_eq!(named.len(), 9, "one sample per variant: {named:?}");
        for (fault, remedy) in faults {
            let msg = fault.to_string();
            assert!(msg.contains(remedy), "{msg}");
        }
    }

    #[test]
    fn a_fault_names_the_file_line_and_commit_it_is_about() {
        let msg = LedgerFault::Unreadable {
            repo: "acme/widgets".into(),
            file: "runs/k/2.jsonl".into(),
            line: 7,
            commit: "c42".into(),
            cause: "not JSON".into(),
        }
        .to_string();
        for part in [
            "runs/k/2.jsonl",
            "line 7",
            "c42",
            "not JSON",
            "acme/widgets",
            "upgrade fl",
        ] {
            assert!(msg.contains(part), "{part}: {msg}");
        }
    }

    // Spec §7: a line in the wrong directory is tampering, naming the file
    // and the commit; the message may also offer the quarantine command.
    #[test]
    fn a_misplaced_line_is_reported_as_tampering() {
        let msg = LedgerFault::Misplaced {
            repo: "acme/widgets".into(),
            file: "runs/k/2.jsonl".into(),
            line: 4,
            belongs: "urn:uuid:x".into(),
            commit: "c42".into(),
        }
        .to_string();
        for part in [
            "was altered",
            "runs/k/2.jsonl",
            "line 4",
            "c42",
            "fl github ledger verify",
            "fl github ledger quarantine runs/k/2.jsonl 4",
        ] {
            assert!(msg.contains(part), "{part}: {msg}");
        }
    }
}
