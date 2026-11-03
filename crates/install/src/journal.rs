//! `journal.jsonl`: the append-only record of one transaction. A line is written and flushed
//! (`fdatasync`) BEFORE the step it announces runs, so after a crash the journal says what may
//! have happened, and the disk says what did.

use crate::error::{Ctx, InstallError, Result};
use fl_core::install::{JOURNAL_SCHEMA_VERSION, JournalLine, JournalOp, TxnIntent};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// A journal larger than this is refused unread.
pub const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

/// An open journal being appended to.
pub struct Journal {
    file: File,
    path: PathBuf,
    txn: u64,
    seq: u64,
}

/// `fsync` the directory `dir` so a created, renamed or removed entry survives a crash.
pub fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .ctx(|| format!("cannot sync directory {}", dir.display()))
}

impl Journal {
    /// Create the journal of transaction `txn`; it must not exist.
    pub fn create(path: &Path, txn: u64) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ctx(|| format!("cannot create the journal {}", path.display()))?;
        if let Some(dir) = path.parent() {
            sync_dir(dir)?;
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            txn,
            seq: 0,
        })
    }

    /// Reopen an existing journal to append after `last_seq`.
    pub fn reopen(path: &Path, txn: u64, last_seq: u64) -> Result<Self> {
        let bytes = fs::read(path).ctx(|| format!("cannot read {}", path.display()))?;
        let file = OpenOptions::new()
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ctx(|| format!("cannot open the journal {}", path.display()))?;
        // A crash may have left a torn last line: it announced nothing, so cut it off.
        let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if keep != bytes.len() {
            file.set_len(keep as u64)
                .and_then(|()| file.sync_data())
                .ctx(|| format!("cannot repair {}", path.display()))?;
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            txn,
            seq: last_seq + 1,
        })
    }

    /// Append one line and flush it to disk.
    pub fn append(&mut self, op: JournalOp) -> Result<()> {
        let line = JournalLine {
            v: JOURNAL_SCHEMA_VERSION,
            txn: self.txn,
            seq: self.seq,
            op,
        };
        self.file
            .write_all(line.to_line().as_bytes())
            .and_then(|()| self.file.sync_data())
            .ctx(|| format!("cannot append to the journal {}", self.path.display()))?;
        self.seq += 1;
        Ok(())
    }
}

/// What a journal file holds.
#[derive(Debug)]
pub enum Found {
    /// The transaction's `begin` line never reached the disk: nothing was started.
    Empty,
    /// A transaction that has not finished.
    Pending(Box<Pending>),
    /// A transaction that committed (its `commit` line is last); only the file is left.
    Committed,
}

/// Where an unfinished transaction stood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    /// Steps were running.
    InProgress,
    /// An undo began and did not finish.
    RollingBack,
    /// The undo finished; only the tail (lock file, journal, created directories) may be left.
    RolledBack,
}

#[derive(Debug)]
pub struct Pending {
    pub state: PendingState,
    pub txn: u64,
    pub intent: TxnIntent,
    /// Every line after `begin`, in order.
    pub ops: Vec<JournalOp>,
    pub last_seq: u64,
}

impl Pending {
    pub fn announced(&self, f: impl Fn(&JournalOp) -> bool) -> bool {
        self.ops.iter().any(f)
    }
}

/// Read the journal at `path`. `Ok(None)`: there is none.
pub fn read(path: &Path) -> Result<Option<Found>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(InstallError::io(
                format!("cannot inspect {}", path.display()),
                e,
            ));
        }
    };
    if !meta.is_file() {
        return Err(InstallError::refused(format!(
            "{} is not a regular file; it is not touched",
            path.display()
        )));
    }
    if meta.len() > MAX_JOURNAL_BYTES {
        return Err(InstallError::refused(format!(
            "the journal {} is larger than {MAX_JOURNAL_BYTES} bytes",
            path.display()
        )));
    }
    let mut text = String::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .and_then(|f| f.take(MAX_JOURNAL_BYTES + 1).read_to_string(&mut text))
        .ctx(|| format!("cannot read the journal {}", path.display()))?;
    let complete = text.ends_with('\n');
    let mut lines: Vec<&str> = text.lines().collect();
    // A last line the crash cut short never announced anything.
    if !complete && !lines.is_empty() {
        lines.pop();
    }
    let corrupt = |why: String| {
        InstallError::refused(format!(
            "the journal {} is corrupt ({why}); it is not guessed at. Inspect it, then remove it \
             by hand only if you have checked what the interrupted install left",
            path.display()
        ))
    };
    let mut parsed: Vec<JournalLine> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let line: JournalLine =
            serde_json::from_str(l).map_err(|e| corrupt(format!("line {}: {e}", i + 1)))?;
        if line.v != JOURNAL_SCHEMA_VERSION {
            return Err(corrupt(format!("line {} has version {}", i + 1, line.v)));
        }
        parsed.push(line);
    }
    let Some(first) = parsed.first() else {
        return Ok(Some(Found::Empty));
    };
    let JournalOp::Begin { intent } = &first.op else {
        return Err(corrupt("the first line is not `begin`".into()));
    };
    let txn = first.txn;
    let mut ops = Vec::new();
    let mut last_seq = first.seq;
    for l in &parsed[1..] {
        if l.txn != txn {
            return Err(corrupt(format!(
                "line {} belongs to transaction {} inside transaction {txn}",
                l.seq, l.txn
            )));
        }
        if l.seq <= last_seq {
            return Err(corrupt("line numbers do not increase".into()));
        }
        last_seq = l.seq;
        ops.push(l.op.clone());
    }
    let state = match ops.last() {
        Some(JournalOp::Commit) => return Ok(Some(Found::Committed)),
        Some(JournalOp::RollbackDone) => PendingState::RolledBack,
        _ if ops.iter().any(|o| matches!(o, JournalOp::RollbackBegin)) => PendingState::RollingBack,
        _ => PendingState::InProgress,
    };
    Ok(Some(Found::Pending(Box::new(Pending {
        state,
        txn,
        intent: (**intent).clone(),
        ops,
        last_seq,
    }))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::install::{
        OwnedArtifact, Product, Profile, ReceiptLayout, ReceiptRelease, ReleaseKind, Step,
    };

    fn intent() -> TxnIntent {
        TxnIntent {
            release: ReceiptRelease {
                version: "0.1.0".into(),
                name: "0.1.0+aaaaaaaaaaaa".into(),
                kind: ReleaseKind::Release,
                commit: None,
            },
            profile: Profile::Minimal,
            products: vec![Product::Ferroloop],
            layout: ReceiptLayout {
                prefix: "/p".into(),
                bin_dir: "/b".into(),
                unit_dir: "/u".into(),
            },
            artifacts: vec![OwnedArtifact {
                id: "a".into(),
                sha256: "a".repeat(64),
            }],
            files: vec![],
            steps: vec![Step::Begin, Step::Commit],
            release_existed: false,
            prev_current: None,
            launcher: None,
            aliases: vec![],
            backups: vec![],
            created_dirs: vec![],
            lock_created: false,
            prev_receipt: None,
        }
    }

    #[test]
    fn a_journal_round_trips_and_reports_pending_then_closed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("journal.jsonl");
        assert!(read(&p).unwrap().is_none());
        let mut j = Journal::create(&p, 7).unwrap();
        j.append(JournalOp::Begin {
            intent: Box::new(intent()),
        })
        .unwrap();
        j.append(JournalOp::Stage).unwrap();
        match read(&p).unwrap().unwrap() {
            Found::Pending(pd) => {
                assert_eq!(pd.txn, 7);
                assert_eq!(pd.ops, vec![JournalOp::Stage]);
                assert_eq!(pd.last_seq, 1);
            }
            other => panic!("{other:?}"),
        }
        j.append(JournalOp::Commit).unwrap();
        assert!(matches!(read(&p).unwrap().unwrap(), Found::Committed));
        assert!(Journal::create(&p, 8).is_err(), "never overwritten");
    }

    #[test]
    fn a_torn_last_line_never_announced_anything() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("journal.jsonl");
        let mut j = Journal::create(&p, 1).unwrap();
        j.append(JournalOp::Begin {
            intent: Box::new(intent()),
        })
        .unwrap();
        drop(j);
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"{\"v\":1,\"txn\":1,\"seq\":1,\"op\":\"sta")
            .unwrap();
        match read(&p).unwrap().unwrap() {
            Found::Pending(pd) => assert!(pd.ops.is_empty()),
            other => panic!("{other:?}"),
        }
        // Reopening cuts the torn fragment off; the next append is a clean line.
        let mut j = Journal::reopen(&p, 1, 0).unwrap();
        j.append(JournalOp::Stage).unwrap();
        match read(&p).unwrap().unwrap() {
            Found::Pending(pd) => assert_eq!(pd.ops, vec![JournalOp::Stage]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_empty_or_cut_begin_means_nothing_started() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("journal.jsonl");
        fs::write(&p, "").unwrap();
        assert!(matches!(read(&p).unwrap().unwrap(), Found::Empty));
        fs::write(&p, "{\"v\":1,\"txn\":1,\"seq\":0,\"op\":\"beg").unwrap();
        assert!(matches!(read(&p).unwrap().unwrap(), Found::Empty));
    }

    #[test]
    fn corruption_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("journal.jsonl");
        fs::write(&p, "not json\nmore\n").unwrap();
        assert!(read(&p).is_err());
        fs::write(&p, "{\"v\":1,\"txn\":1,\"seq\":0,\"op\":\"stage\"}\n").unwrap();
        assert!(read(&p).is_err(), "must start with begin");
        let link = d.path().join("l");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert!(read(&link).is_err(), "a symlink is not followed");
    }
}
