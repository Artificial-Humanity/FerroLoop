//! `fl store`, output pinned byte for byte: stdout, stderr and the exit code
//! of every subcommand, on fixed fixture stores, compared with the files in
//! `tests/golden/store/`.
//!
//! What varies from run to run is replaced before the comparison, and nothing
//! else:
//!
//! * the sandbox directory → `<DIR>`;
//! * the time a command took → `<T>` (`time: <T>` in text, `"elapsed_ms": <T>`
//!   in JSON);
//! * a table hash → `<HASH#n>` (64 hex digits in JSON) or `<HASH12#n>` (the 12
//!   digits of a mismatch line), `n` counting the distinct hashes of one output
//!   in order of first appearance, so equal hashes stay equal: ids are minted
//!   from the clock, so a table that holds one hashes differently on every run. Only a well-formed hash is
//!   replaced, so a hash printed in another form still fails the comparison;
//! * a minted id → `urn:uuid:<UUID>` (the views compared are named by project
//!   id).
//!
//! Byte sizes are not replaced: the fixtures give the same sizes on every run.
//!
//! Every run gets a private HOME, XDG_CONFIG_HOME, XDG_DATA_HOME,
//! XDG_RUNTIME_DIR and TMPDIR inside the sandbox, with FL_DB and
//! FL_STORE_BACKEND removed from the environment it inherits. A case that
//! needs the redb engine names it on that one command; every other case runs
//! with no engine named.
//!
//! `FL_BLESS_GOLDEN=1 cargo test -p fl-cli --test store_golden` rewrites the
//! files from the current output.

use assert_cmd::Command;
use fl_core::Catalog;
use fl_store::{BackendKind, StoreOptions};
use std::path::{Path, PathBuf};

struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for sub in ["home", "config", "data", "run", "tmp", "stores"] {
            std::fs::create_dir(root.path().join(sub)).unwrap();
        }
        Self { root }
    }

    fn dir(&self, sub: &str) -> PathBuf {
        self.root.path().join(sub)
    }

    /// The store's logical (`.redb`) path.
    fn redb(&self) -> PathBuf {
        self.dir("stores").join("fl.redb")
    }

    fn fls(&self) -> PathBuf {
        self.dir("stores").join("fl.fls")
    }

    /// `fl --db <redb> store <args>`, no engine named.
    fn fl(&self, args: &[&str]) -> Command {
        self.fl_at(&self.redb(), args)
    }

    fn fl_at(&self, db: &Path, args: &[&str]) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.env("HOME", self.dir("home"))
            .env("XDG_CONFIG_HOME", self.dir("config"))
            .env("XDG_DATA_HOME", self.dir("data"))
            .env("XDG_RUNTIME_DIR", self.dir("run"))
            .env("TMPDIR", self.dir("tmp"))
            .env_remove("FL_DB")
            .env_remove("FL_STORE_BACKEND")
            .current_dir(self.dir("home"))
            .arg("--db")
            .arg(db)
            .arg("store")
            .args(args);
        c
    }

    /// The same, on the redb engine.
    fn fl_redb(&self, args: &[&str]) -> Command {
        let mut c = self.fl(args);
        c.env("FL_STORE_BACKEND", "redb");
        c
    }

    /// Run `cmd` and compare its normalised output with `golden/store/<name>.txt`.
    fn check(&self, name: &str, mut cmd: Command, shown: &str) {
        let out = cmd.output().unwrap();
        let got = format!(
            "$ {shown}\nexit: {}\n--- stdout\n{}--- stderr\n{}",
            out.status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string()),
            self.normalise(&String::from_utf8_lossy(&out.stdout)),
            self.normalise(&String::from_utf8_lossy(&out.stderr)),
        );
        let file = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden/store")
            .join(format!("{name}.txt"));
        if std::env::var("FL_BLESS_GOLDEN").is_ok_and(|v| v == "1") {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, &got).unwrap();
            return;
        }
        let want = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("{}: {e} (FL_BLESS_GOLDEN=1 writes it)", file.display()));
        assert_eq!(
            got,
            want,
            "output of `{shown}` differs from {}",
            file.display()
        );
    }

    fn normalise(&self, text: &str) -> String {
        let root = self.root.path().display().to_string();
        let text = text.replace(&root, "<DIR>");
        let mut hashes = Hashes::default();
        let mut out = String::new();
        for line in text.split_inclusive('\n') {
            out.push_str(&normalise_line(line, &mut hashes));
        }
        out
    }
}

/// Replace the digits after `prefix` up to `until` with `<T>`, when they are
/// all digits or a decimal point.
fn time_after(line: &str, prefix: &str, until: &str) -> Option<String> {
    let at = line.find(prefix)? + prefix.len();
    let rest = &line[at..];
    let end = rest.find(until)?;
    let value = &rest[..end];
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    Some(format!("{}<T>{}", &line[..at], &rest[end..]))
}

fn is_hash(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `urn:uuid:<36>` → `urn:uuid:<UUID>`, for each well-formed uuid in `line`.
fn uuids(line: &str) -> String {
    const URN: &str = "urn:uuid:";
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find(URN) {
        let start = at + URN.len();
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let candidate = rest.get(..36).unwrap_or("");
        let well_formed = candidate.len() == 36
            && candidate.bytes().enumerate().all(|(i, b)| {
                if [8, 13, 18, 23].contains(&i) {
                    b == b'-'
                } else {
                    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
                }
            });
        if well_formed {
            out.push_str("<UUID>");
            rest = &rest[36..];
        }
    }
    out.push_str(rest);
    out
}

/// The distinct hashes of one output, in order of first appearance, so a
/// golden keeps which hashes are equal (`<HASH#1>` twice) and which are not.
#[derive(Default)]
struct Hashes {
    full: Vec<String>,
    short: Vec<String>,
}

impl Hashes {
    /// `<HASH#n>` for a 64-digit hash, `<HASH12#n>` for a 12-digit one.
    fn placeholder(&mut self, hash: &str) -> String {
        let (seen, label) = if hash.len() == 64 {
            (&mut self.full, "HASH")
        } else {
            (&mut self.short, "HASH12")
        };
        let n = seen.iter().position(|h| h == hash).unwrap_or_else(|| {
            seen.push(hash.to_string());
            seen.len() - 1
        });
        format!("<{label}#{}>", n + 1)
    }
}

fn normalise_line(line: &str, hashes: &mut Hashes) -> String {
    let line = &uuids(line);
    if let Some(l) = time_after(line, "time: ", "s\n") {
        return l;
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with("\"elapsed_ms\": ") {
        let end = if line.ends_with(",\n") { ",\n" } else { "\n" };
        if let Some(l) = time_after(line, "\"elapsed_ms\": ", end) {
            return l;
        }
    }
    for key in ["\"redb_hash\": \"", "\"ferro_hash\": \""] {
        if let Some(at) = line.find(key) {
            let start = at + key.len();
            if let Some(len) = line[start..].find('"')
                && is_hash(&line[start..start + len], 64)
            {
                return format!(
                    "{}{}{}",
                    &line[..start],
                    hashes.placeholder(&line[start..start + len]),
                    &line[start + len..]
                );
            }
        }
    }
    // `  <table>: redb <n> rows <hash12>, ferro <n> rows <hash12>`
    if line.starts_with("  ") && line.contains(": redb ") && line.contains(", ferro ") {
        let words: Vec<String> = line
            .trim_end_matches('\n')
            .split(' ')
            .map(|w| {
                let bare = w.trim_end_matches(',');
                if is_hash(bare, 12) {
                    w.replacen(bare, &hashes.placeholder(bare), 1)
                } else {
                    w.to_string()
                }
            })
            .collect();
        return format!("{}\n", words.join(" "));
    }
    line.to_string()
}

// ---- fixtures, made with the library, never with the command under test ----

/// A redb store holding two projects.
fn redb_store(path: &Path) {
    let store = fl_store::RedbStore::open(path).unwrap();
    store.add_project("/golden/project-a").unwrap();
    store.add_project("/golden/project-b").unwrap();
}

/// A `.fls` with earlier checkpoints behind it, so it has dead bytes.
fn ferro_store(path: &Path) {
    let store = fl_store::FerroStore::open(path).unwrap();
    for round in 0..3 {
        for i in 0..10 {
            store
                .put_doc(
                    "docs",
                    &format!("r{round}-{i:02}"),
                    &serde_json::json!({"round": round, "i": i}),
                )
                .unwrap();
        }
        store.checkpoint().unwrap();
    }
}

/// A redb store and its verified `.fls`, made by the library's migration.
fn migrated_pair(sb: &Sandbox) {
    redb_store(&sb.redb());
    let report = fl_store::migrate_locked(&sb.redb(), &sb.fls()).unwrap();
    assert!(report.is_clean());
}

/// A migrated pair whose `.fls` then gains a project the redb does not hold.
fn mismatched_pair(sb: &Sandbox) {
    migrated_pair(sb);
    let ferro = fl_store::open_store_with(
        &sb.redb(),
        &StoreOptions {
            backend: Some(BackendKind::Ferro),
            ..StoreOptions::default()
        },
    )
    .unwrap();
    ferro.add_project("/golden/only-in-ferro").unwrap();
}

// ---- status ----

#[test]
fn status_on_a_missing_redb_store() {
    let sb = Sandbox::new();
    sb.check(
        "status-redb-missing",
        sb.fl_redb(&["status"]),
        "FL_STORE_BACKEND=redb fl store status",
    );
    sb.check(
        "status-redb-missing-json",
        sb.fl_redb(&["status", "--json"]),
        "FL_STORE_BACKEND=redb fl store status --json",
    );
}

#[test]
fn status_on_a_redb_store() {
    let sb = Sandbox::new();
    redb_store(&sb.redb());
    sb.check(
        "status-redb",
        sb.fl_redb(&["status"]),
        "FL_STORE_BACKEND=redb fl store status",
    );
    sb.check(
        "status-redb-json",
        sb.fl_redb(&["status", "--json"]),
        "FL_STORE_BACKEND=redb fl store status --json",
    );
}

#[test]
fn status_on_an_empty_redb_file() {
    let sb = Sandbox::new();
    std::fs::write(sb.redb(), b"").unwrap();
    sb.check(
        "status-redb-empty",
        sb.fl_redb(&["status"]),
        "FL_STORE_BACKEND=redb fl store status",
    );
}

#[test]
fn status_on_a_ferro_store() {
    let sb = Sandbox::new();
    ferro_store(&sb.fls());
    sb.check("status-ferro", sb.fl(&["status"]), "fl store status");
    sb.check(
        "status-ferro-json",
        sb.fl(&["status", "--json"]),
        "fl store status --json",
    );
}

#[test]
fn status_on_a_missing_ferro_store() {
    let sb = Sandbox::new();
    sb.check(
        "status-ferro-missing",
        sb.fl(&["status"]),
        "fl store status",
    );
    sb.check(
        "status-ferro-missing-json",
        sb.fl(&["status", "--json"]),
        "fl store status --json",
    );
}

#[test]
fn status_on_a_garbage_fls() {
    let sb = Sandbox::new();
    std::fs::write(sb.fls(), b"not a store").unwrap();
    sb.check(
        "status-ferro-garbage",
        sb.fl(&["status"]),
        "fl store status",
    );
    sb.check(
        "status-ferro-garbage-json",
        sb.fl(&["status", "--json"]),
        "fl store status --json",
    );
}

#[test]
fn status_on_a_lone_redb_under_ferro() {
    let sb = Sandbox::new();
    redb_store(&sb.redb());
    sb.check(
        "status-redb-beside-ferro",
        sb.fl(&["status"]),
        "fl store status",
    );
    sb.check(
        "status-redb-beside-ferro-json",
        sb.fl(&["status", "--json"]),
        "fl store status --json",
    );
}

#[test]
fn status_on_an_unknown_engine() {
    let sb = Sandbox::new();
    let mut cmd = sb.fl(&["status"]);
    cmd.env("FL_STORE_BACKEND", "sqlite");
    sb.check(
        "status-unknown-engine",
        cmd,
        "FL_STORE_BACKEND=sqlite fl store status",
    );
}

// ---- checkpoint ----

#[test]
fn checkpoint_on_redb() {
    let sb = Sandbox::new();
    redb_store(&sb.redb());
    sb.check(
        "checkpoint-redb",
        sb.fl_redb(&["checkpoint"]),
        "FL_STORE_BACKEND=redb fl store checkpoint",
    );
}

#[test]
fn checkpoint_on_ferro() {
    let sb = Sandbox::new();
    ferro_store(&sb.fls());
    sb.check(
        "checkpoint-ferro",
        sb.fl(&["checkpoint"]),
        "fl store checkpoint",
    );
}

#[test]
fn checkpoint_on_a_missing_ferro_store() {
    let sb = Sandbox::new();
    sb.check(
        "checkpoint-ferro-missing",
        sb.fl(&["checkpoint"]),
        "fl store checkpoint",
    );
}

// ---- compact ----

#[test]
fn compact_on_ferro() {
    let sb = Sandbox::new();
    ferro_store(&sb.fls());
    sb.check("compact-ferro", sb.fl(&["compact"]), "fl store compact");
}

#[test]
fn compact_on_ferro_json() {
    let sb = Sandbox::new();
    ferro_store(&sb.fls());
    sb.check(
        "compact-ferro-json",
        sb.fl(&["compact", "--json"]),
        "fl store compact --json",
    );
}

#[test]
fn compact_on_redb_is_refused() {
    let sb = Sandbox::new();
    redb_store(&sb.redb());
    sb.check(
        "compact-redb",
        sb.fl_redb(&["compact"]),
        "FL_STORE_BACKEND=redb fl store compact",
    );
    sb.check(
        "compact-redb-json",
        sb.fl_redb(&["compact", "--json"]),
        "FL_STORE_BACKEND=redb fl store compact --json",
    );
}

#[test]
fn compact_on_a_missing_ferro_store() {
    let sb = Sandbox::new();
    sb.check(
        "compact-ferro-missing",
        sb.fl(&["compact"]),
        "fl store compact",
    );
}

// ---- migrate ----

#[test]
fn migrate() {
    let sb = Sandbox::new();
    redb_store(&sb.redb());
    sb.check("migrate", sb.fl(&["migrate"]), "fl store migrate");
    // A second migration is refused.
    sb.check(
        "migrate-already-migrated",
        sb.fl(&["migrate"]),
        "fl store migrate",
    );
}

#[test]
fn migrate_json() {
    let sb = Sandbox::new();
    redb_store(&sb.redb());
    sb.check(
        "migrate-json",
        sb.fl(&["migrate", "--json"]),
        "fl store migrate --json",
    );
}

#[test]
fn migrate_dry_run() {
    let sb = Sandbox::new();
    redb_store(&sb.redb());
    sb.check(
        "migrate-dry-run",
        sb.fl(&["migrate", "--dry-run"]),
        "fl store migrate --dry-run",
    );
    sb.check(
        "migrate-dry-run-json",
        sb.fl(&["migrate", "--dry-run", "--json"]),
        "fl store migrate --dry-run --json",
    );
}

#[test]
fn migrate_refuses_what_it_cannot_do() {
    let sb = Sandbox::new();
    sb.check("migrate-no-redb", sb.fl(&["migrate"]), "fl store migrate");
    sb.check(
        "migrate-fls-path",
        sb.fl_at(&sb.fls(), &["migrate"]),
        "fl --db <DIR>/stores/fl.fls store migrate",
    );
}

// ---- verify ----

#[test]
fn verify_a_clean_pair() {
    let sb = Sandbox::new();
    migrated_pair(&sb);
    sb.check("verify", sb.fl(&["verify"]), "fl store verify");
    sb.check(
        "verify-json",
        sb.fl(&["verify", "--json"]),
        "fl store verify --json",
    );
}

#[test]
fn verify_a_pair_that_differs() {
    let sb = Sandbox::new();
    mismatched_pair(&sb);
    sb.check("verify-mismatch", sb.fl(&["verify"]), "fl store verify");
    sb.check(
        "verify-mismatch-json",
        sb.fl(&["verify", "--json"]),
        "fl store verify --json",
    );
}

#[test]
fn verify_refuses_a_missing_fls() {
    let sb = Sandbox::new();
    redb_store(&sb.redb());
    sb.check("verify-no-fls", sb.fl(&["verify"]), "fl store verify");
    sb.check(
        "verify-fls-path",
        sb.fl_at(&sb.fls(), &["verify"]),
        "fl --db <DIR>/stores/fl.fls store verify",
    );
}
