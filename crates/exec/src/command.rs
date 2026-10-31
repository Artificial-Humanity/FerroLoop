use crate::population::ExecError;
use fl_core::model::{CommandSpec, PopulationDelivery};
use fl_core::verdict::{FailReason, Verdict};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const EXCERPT_LIMIT: usize = 4096;

/// Most output kept per stream. The excerpt shows 4 KiB; the rest is read and thrown away so a
/// chatty command cannot grow this process's memory without bound.
const CAPTURE_LIMIT: usize = 256 * 1024;

/// How long to keep waiting for the output pipes to close once the command itself has exited.
/// A background process that inherited a pipe holds it open long after the command is gone.
const PIPE_GRACE: Duration = Duration::from_millis(500);

pub struct GateOutcome {
    pub verdict: Verdict,
    pub output_excerpt: String,
    pub duration_ms: u64,
}

/// Concatenate stdout and stderr into a bounded excerpt for humans.
///
/// Truncates on a `char` boundary rather than a raw byte offset:
/// `String::truncate` panics if the cut point lands inside a multi-byte
/// UTF-8 sequence, and `String::from_utf8_lossy` does not guarantee output
/// that is a clean multiple of any particular byte width.
fn excerpt(stdout: &[u8], stderr: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(stdout).into_owned();
    if !stderr.is_empty() {
        s.push_str(&String::from_utf8_lossy(stderr));
    }
    if s.len() > EXCERPT_LIMIT {
        let mut end = EXCERPT_LIMIT;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// Run one command gate.
///
/// ⚠ The `population` argument is resolved by the caller and handed in. The
/// command is never asked to find its own work: a command that enumerates for
/// itself can silently examine fewer items than the gate intended, and the
/// resulting green is indistinguishable from a real one.
pub fn run_command_gate(
    root: &Path,
    spec: &CommandSpec,
    population: &[PathBuf],
    min_population: u64,
) -> GateOutcome {
    let started = Instant::now();
    let count = population.len() as u64;

    // The floor is checked before anything is spawned. A gate below its floor
    // (an empty population always counts as below floor, regardless of what
    // the floor itself is set to) has not examined enough to say anything, so
    // nothing runs.
    let floor = min_population.max(1);
    if count < floor {
        return GateOutcome {
            verdict: Verdict::fail_for(FailReason::EmptyPopulation, count),
            output_excerpt: format!("population {count} is below the declared floor of {floor}"),
            duration_ms: started.elapsed().as_millis() as u64,
        };
    }

    let mut cmd = Command::new(&spec.program);
    // Its own process group, so a timeout can take down everything the command started and not
    // just the direct child.
    cmd.args(&spec.args)
        .current_dir(root)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Paths are handed over as the bytes they are: `Path::display` would replace anything that
    // is not UTF-8 with U+FFFD and name a file that does not exist.
    match spec.delivery {
        PopulationDelivery::Args => {
            cmd.args(population);
        }
        PopulationDelivery::Stdin => {
            cmd.stdin(Stdio::piped());
        }
        PopulationDelivery::FileList => {
            // The command's own args are expected to reference the list path
            // via the FL_POPULATION_FILE environment variable.
        }
    }

    let list_file = if matches!(spec.delivery, PopulationDelivery::FileList) {
        match write_list(population) {
            Ok(f) => {
                cmd.env("FL_POPULATION_FILE", f.path());
                Some(f)
            }
            Err(e) => {
                return error_outcome(started, format!("could not write the population list: {e}"));
            }
        }
    } else {
        None
    };

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let detail = ExecError::Spawn(spec.program.clone(), e.to_string()).to_string();
            return error_outcome(started, detail);
        }
    };

    if matches!(spec.delivery, PopulationDelivery::Stdin)
        && let Some(mut stdin) = child.stdin.take()
    {
        // From its own thread: a command that does not read stdin, given a population larger
        // than the pipe buffer, would block this write, and the deadline below would never start.
        let payload = population_bytes(population);
        std::thread::spawn(move || {
            let _ = stdin.write_all(&payload);
            // `stdin` drops here, closing the pipe so the child sees EOF.
        });
    }

    let deadline = Duration::from_secs(spec.timeout_secs.max(1));
    let out = match wait_with_deadline(&mut child, deadline) {
        Ok(Some(out)) => out,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            let detail = ExecError::Timeout(spec.program.clone(), spec.timeout_secs).to_string();
            return error_outcome(started, detail);
        }
        Err(e) => return error_outcome(started, e.to_string()),
    };

    drop(list_file);

    let code = out.status.code().unwrap_or(-1);
    let passed = spec.pass_codes.contains(&code);
    GateOutcome {
        verdict: Verdict::from_predicate(passed, count),
        output_excerpt: excerpt(&out.stdout, &out.stderr),
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

fn error_outcome(started: Instant, detail: String) -> GateOutcome {
    GateOutcome {
        verdict: Verdict::error(detail.clone()),
        output_excerpt: detail,
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

/// Every path followed by a newline, as raw bytes.
fn population_bytes(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in paths {
        out.extend_from_slice(p.as_os_str().as_bytes());
        out.push(b'\n');
    }
    out
}

fn write_list(paths: &[PathBuf]) -> std::io::Result<tempfile::NamedTempFile> {
    let mut f = tempfile::NamedTempFile::new()?;
    f.write_all(&population_bytes(paths))?;
    f.flush()?;
    Ok(f)
}

/// SIGKILL the process group `pgid` (the command was started as its own group leader).
/// `kill` is the shell's built-in, so this needs no `kill` binary and no libc binding.
fn kill_group(pgid: u32) {
    let _ = Command::new("sh")
        .args(["-c", &format!("kill -KILL -{pgid} 2>/dev/null")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// A pipe being read to the end on its own thread, into a buffer that can be read while the
/// thread is still running.
struct Drain {
    handle: JoinHandle<()>,
    buf: Arc<Mutex<Vec<u8>>>,
}

/// Start a thread that reads a pipe to completion.
///
/// Spawned before the wait loop starts, so the pipe is always being drained
/// while the child runs. A child that writes more than the OS pipe buffer
/// (64 KiB on Linux) blocks on the next write until someone reads; a reader
/// that only shows up after the child exits, or that only polls inside the
/// `try_wait` loop without actually draining, never unblocks a child in that
/// state — and the deadline then fires on a process that was only waiting
/// for a reader, not doing anything wrong.
fn spawn_drain(mut pipe: impl Read + Send + 'static) -> Drain {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::clone(&buf);
    let handle = std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut kept = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let room = CAPTURE_LIMIT.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    });
    Drain { handle, buf }
}

/// What `drain` has read so far, once its pipe has closed or `until` passes, whichever is
/// first; `true` when the pipe closed.
fn collect(drain: Option<Drain>, until: Instant) -> (Vec<u8>, bool) {
    let Some(drain) = drain else {
        return (Vec::new(), true);
    };
    while !drain.handle.is_finished() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    let done = drain.handle.is_finished();
    let bytes = drain.buf.lock().unwrap_or_else(|e| e.into_inner()).clone();
    (bytes, done)
}

/// Poll the child until it exits or the deadline passes. `Ok(None)` means the
/// deadline won; the caller is responsible for killing the child in that
/// case.
///
/// Drains `stdout`/`stderr` on background threads started before the poll
/// loop, rather than calling `Child::wait_with_output` (which consumes the
/// child and cannot be interleaved with a `try_wait` polling loop) or reading
/// the pipes synchronously inside the loop (which reintroduces the same
/// full-pipe deadlock this function exists to avoid). `std::process::Output`
/// is a plain struct with public fields, so it can be assembled directly
/// from the drained buffers and the status `try_wait` hands back.
///
/// A timeout kills the command's whole process group. After a normal exit the pipes get
/// [`PIPE_GRACE`] to close; if something the command left running still holds them, the group
/// is killed and the output read so far is used, instead of waiting on that process.
fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    let stdout_drain = child.stdout.take().map(spawn_drain);
    let stderr_drain = child.stderr.take().map(spawn_drain);
    let pgid = child.id();

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= deadline {
            // The drain threads see EOF and finish on their own once the group is dead.
            kill_group(pgid);
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    let until = Instant::now() + PIPE_GRACE;
    let (stdout, out_closed) = collect(stdout_drain, until);
    let (stderr, err_closed) = collect(stderr_drain, until);
    if !(out_closed && err_closed) {
        kill_group(pgid);
    }

    Ok(Some(std::process::Output {
        status,
        stdout,
        stderr,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::model::PopulationDelivery;
    use fl_core::verdict::FailReason;
    use std::fs;

    fn spec(program: &str, args: &[&str], delivery: PopulationDelivery) -> CommandSpec {
        CommandSpec {
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            delivery,
            timeout_secs: 10,
            pass_codes: vec![0],
        }
    }

    // REQUIRED TEST 1 (spec §9): the vacuous pass.
    #[test]
    fn an_empty_population_fails_even_when_the_command_would_succeed() {
        let d = tempfile::tempdir().unwrap();
        let out = run_command_gate(
            d.path(),
            &spec("true", &[], PopulationDelivery::Args),
            &[],
            1,
        );
        assert_eq!(
            out.verdict,
            Verdict::fail_for(FailReason::EmptyPopulation, 0)
        );
    }

    // REQUIRED TEST 2 (spec §9): the framework enumerates, not the command.
    #[test]
    fn the_command_receives_every_path_the_framework_resolved() {
        let d = tempfile::tempdir().unwrap();
        for n in ["a.txt", "b.txt", "c.txt"] {
            fs::write(d.path().join(n), "x").unwrap();
        }
        let pop: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|n| d.path().join(n))
            .collect();

        // `wc -l <files>` prints one line per file plus a total line.
        let out = run_command_gate(
            d.path(),
            &spec("wc", &["-l"], PopulationDelivery::Args),
            &pop,
            1,
        );
        assert_eq!(out.verdict, Verdict::from_predicate(true, 3));
        assert!(
            out.output_excerpt.contains("total"),
            "got {}",
            out.output_excerpt
        );
    }

    #[test]
    fn a_population_below_the_declared_floor_fails() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.txt"), "x").unwrap();
        let out = run_command_gate(
            d.path(),
            &spec("true", &[], PopulationDelivery::Args),
            &[d.path().join("a.txt")],
            5,
        );
        assert_eq!(
            out.verdict,
            Verdict::fail_for(FailReason::EmptyPopulation, 1)
        );
    }

    #[test]
    fn a_nonzero_exit_fails_on_the_predicate() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.txt"), "x").unwrap();
        let out = run_command_gate(
            d.path(),
            &spec("false", &[], PopulationDelivery::Args),
            &[d.path().join("a.txt")],
            1,
        );
        assert_eq!(out.verdict, Verdict::fail_for(FailReason::Predicate, 1));
    }

    // REQUIRED TEST 4 (spec §9), part one: a missing command is not a pass.
    #[test]
    fn a_missing_command_is_an_error_and_not_a_pass() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.txt"), "x").unwrap();
        let out = run_command_gate(
            d.path(),
            &spec(
                "definitely-not-a-real-program-9f3x",
                &[],
                PopulationDelivery::Args,
            ),
            &[d.path().join("a.txt")],
            1,
        );
        assert!(
            matches!(out.verdict, Verdict::Error { .. }),
            "got {:?}",
            out.verdict
        );
        assert_eq!(out.verdict.exit_code(), 2);
    }

    // REQUIRED TEST 4 (spec §9), part two: a timeout is not a pass.
    //
    // Deviation from the brief's literal fixture: the brief uses
    // `PopulationDelivery::Args` here. Under `Args` delivery the resolved
    // population is appended to the command's own argv (that is the entire
    // point of `Args` delivery — see
    // `the_command_receives_every_path_the_framework_resolved` above), so
    // the child actually spawned is `sleep 30 <path-to-a.txt>`. This
    // system's `sleep` (uutils coreutils 0.8.0 — same vendor as the `wc`
    // used elsewhere in this file, confirmed with `sleep --version`) treats
    // every positional argument as a time interval and rejects a
    // non-numeric one immediately: `sleep 30 /tmp/xyz/a.txt` exits 1 in a
    // few milliseconds with "invalid time interval", never sleeps, and the
    // gate observes an ordinary nonzero exit (`Fail { reason: Predicate }`),
    // not a timeout — verified directly against the installed binary before
    // making this change.
    // `Stdin` delivery keeps the population off `sleep`'s argv (it goes to
    // the child's stdin instead, which `sleep` never reads), so `sleep 30`
    // runs as written and the 1s deadline is what actually ends it.
    #[test]
    fn a_timeout_is_an_error_and_not_a_pass() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.txt"), "x").unwrap();
        let mut s = spec("sleep", &["30"], PopulationDelivery::Stdin);
        s.timeout_secs = 1;
        let out = run_command_gate(d.path(), &s, &[d.path().join("a.txt")], 1);
        assert!(
            matches!(out.verdict, Verdict::Error { .. }),
            "got {:?}",
            out.verdict
        );
    }

    #[test]
    fn stdin_delivery_hands_the_population_to_the_command_one_path_per_line() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.txt"), "x").unwrap();
        fs::write(d.path().join("b.txt"), "x").unwrap();
        let pop = vec![d.path().join("a.txt"), d.path().join("b.txt")];
        let out = run_command_gate(
            d.path(),
            &spec("wc", &["-l"], PopulationDelivery::Stdin),
            &pop,
            1,
        );
        assert_eq!(out.verdict, Verdict::from_predicate(true, 2));
        assert!(
            out.output_excerpt.trim().starts_with('2'),
            "got {}",
            out.output_excerpt
        );
    }

    // Task 7 review, Important 2: rule 1 ("the framework enumerates, the
    // command never does") is only actually verified for a delivery mode if
    // the command reads back what it was handed and we check that against
    // the framework's own count — not just that some plausible-looking
    // verdict came back. `Args` and `Stdin` had that end-to-end check;
    // `FileList` did not, so a command reading a stale/wrong/empty
    // FL_POPULATION_FILE could have shipped silently green. This command
    // reads $FL_POPULATION_FILE itself and reports the line count it
    // actually saw, so a wrong file (or no file) shows up as a wrong number
    // or a shell error, not a false pass.
    #[test]
    fn filelist_delivery_hands_the_command_a_list_file_with_every_path() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.txt"), "x").unwrap();
        fs::write(d.path().join("b.txt"), "x").unwrap();
        fs::write(d.path().join("c.txt"), "x").unwrap();
        let pop = vec![
            d.path().join("a.txt"),
            d.path().join("b.txt"),
            d.path().join("c.txt"),
        ];

        let out = run_command_gate(
            d.path(),
            &spec(
                "sh",
                &["-c", "wc -l < \"$FL_POPULATION_FILE\""],
                PopulationDelivery::FileList,
            ),
            &pop,
            1,
        );
        assert_eq!(
            out.verdict,
            Verdict::from_predicate(true, 3),
            "got {:?}",
            out.verdict
        );
        assert_eq!(out.output_excerpt.trim(), "3", "got {}", out.output_excerpt);
    }

    // Not in the brief's required list. This is the drain proof the brief
    // asks for by name: a population large enough that `wc -l`'s own output
    // (one line per path, plus a total line) exceeds the 64 KiB pipe buffer
    // Linux gives a child process. A version that reads stdout only after
    // the child exits (or only inside the polling loop, undrained) either
    // deadlocks here or spuriously times out, because the child blocks on a
    // full pipe long before its 20s timeout budget is spent.
    #[test]
    fn a_command_whose_output_exceeds_the_pipe_buffer_does_not_deadlock_the_drain() {
        let d = tempfile::tempdir().unwrap();
        // 2000 files measured ~64011 bytes of `wc -l` output on this
        // system — just under the 64 KiB (65536 byte) pipe buffer, so it
        // would not reliably prove anything. 4000 gives roughly double that
        // with margin to spare regardless of path-length variance across
        // machines.
        const COUNT: usize = 4000;
        let mut pop = Vec::with_capacity(COUNT);
        for n in 0..COUNT {
            let name = format!("f{n:05}.txt");
            fs::write(d.path().join(&name), "x").unwrap();
            pop.push(d.path().join(name));
        }

        // Prove the premise first: this exact population, run through the
        // same `wc -l` the gate below will run, produces more than 64 KiB
        // (65536 bytes) of stdout on its own — enough to overrun a Linux
        // pipe buffer if nothing on our end is reading it while the child
        // is still writing.
        let paths: Vec<String> = pop.iter().map(|p| p.display().to_string()).collect();
        let raw = std::process::Command::new("wc")
            .arg("-l")
            .args(&paths)
            .current_dir(d.path())
            .output()
            .expect("wc must be runnable directly for this to be a meaningful proof");
        let raw_len = raw.stdout.len();
        assert!(
            raw_len > 65536,
            "fixture does not exceed the 64 KiB pipe buffer (got {raw_len} bytes); \
             this test proves nothing without that margin"
        );

        // Now run it through the gate with a timeout short enough that a
        // deadlocked/undrained implementation would time out well before a
        // human would give up waiting on this test.
        let mut s = spec("wc", &["-l"], PopulationDelivery::Args);
        s.timeout_secs = 10;
        let out = run_command_gate(d.path(), &s, &pop, 1);

        // The `Pass { population: COUNT }` verdict is the actual proof: it
        // is only reachable if `wc -l` ran to completion and reported exit
        // 0 within the 10s deadline, which it cannot do unless the drain
        // kept its pipes empty while it was still writing.
        assert_eq!(
            out.verdict,
            Verdict::from_predicate(true, COUNT as u64),
            "got {:?}",
            out.verdict
        );
        // The excerpt is capped at EXCERPT_LIMIT and this fixture's raw
        // output is well over that, so the excerpt itself is expected to be
        // a truncated prefix — not the "total" line, which is the very
        // last thing `wc -l` would have written.
        assert_eq!(
            out.output_excerpt.len(),
            EXCERPT_LIMIT,
            "got {} bytes",
            out.output_excerpt.len()
        );
        // This system's `wc` (uutils coreutils 0.8.0) right-justifies the
        // per-file count to a column width derived from the row count
        // (4000 files + 1 total line needs 4 digits), so a zero count reads
        // as 3 spaces of padding then "0" — verified against the live
        // binary rather than assumed from GNU wc's behaviour.
        assert!(
            out.output_excerpt.starts_with("   0 "),
            "got {:?}",
            out.output_excerpt
        );
    }

    /// Audit C-1: the population was written to the child's stdin before the deadline clock
    /// started. A command that never reads stdin and a population over the 64 KiB pipe buffer
    /// blocked that write, so `timeout_secs` never applied.
    #[test]
    fn a_command_that_ignores_a_large_stdin_population_still_times_out() {
        let d = tempfile::tempdir().unwrap();
        let population: Vec<PathBuf> = (0..20_000)
            .map(|i| d.path().join(format!("file-{i:08}.txt")))
            .collect();
        let mut s = spec("sleep", &["6"], PopulationDelivery::Stdin);
        s.timeout_secs = 1;
        let started = Instant::now();
        let out = run_command_gate(d.path(), &s, &population, 1);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the gate ran for {:?} against a 1 s timeout",
            started.elapsed()
        );
        assert!(!out.verdict.is_pass());
        assert!(
            out.output_excerpt.contains("timed out") || out.output_excerpt.contains("timeout"),
            "{}",
            out.output_excerpt
        );
    }

    /// Audit C-2: a gate command that leaves a background process holding its stdout pipe made
    /// the runner wait for that process after the command itself had exited, with no deadline.
    #[test]
    fn a_background_process_holding_the_pipes_does_not_hold_the_gate() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.txt"), "x").unwrap();
        let mut s = spec(
            "sh",
            &["-c", "sleep 6 & echo started"],
            PopulationDelivery::FileList,
        );
        s.timeout_secs = 1;
        let started = Instant::now();
        let out = run_command_gate(d.path(), &s, &[d.path().join("a.txt")], 1);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the gate waited {:?} on a background process",
            started.elapsed()
        );
        assert!(out.verdict.is_pass(), "{:?}", out.verdict);
        assert!(
            out.output_excerpt.contains("started"),
            "{}",
            out.output_excerpt
        );
    }

    #[test]
    fn a_timeout_kills_the_whole_process_group_not_just_the_shell() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.txt"), "x").unwrap();
        let marker = d.path().join("grandchild-survived");
        let script = format!("(sleep 2; touch '{}') & sleep 30", marker.display());
        let mut s = spec("sh", &["-c", &script], PopulationDelivery::FileList);
        s.timeout_secs = 1;
        let out = run_command_gate(d.path(), &s, &[d.path().join("a.txt")], 1);
        assert!(!out.verdict.is_pass());
        std::thread::sleep(Duration::from_millis(2500));
        assert!(!marker.exists(), "a grandchild outlived the gate's timeout");
    }

    /// Audit C-3: population paths went to the command through `Path::display`, which replaces
    /// bytes that are not UTF-8 with U+FFFD, so the command was handed a file that does not exist.
    #[test]
    fn a_path_that_is_not_utf8_reaches_the_command_byte_for_byte() {
        use std::os::unix::ffi::OsStrExt;
        let d = tempfile::tempdir().unwrap();
        let odd = d.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9.txt"));
        if fs::write(&odd, "x").is_err() {
            return; // a filesystem that forbids non-UTF-8 names has nothing to test
        }
        let out = run_command_gate(
            d.path(),
            &spec("test", &["-e"], PopulationDelivery::Args),
            std::slice::from_ref(&odd),
            1,
        );
        assert!(
            out.verdict.is_pass(),
            "{:?}: {}",
            out.verdict,
            out.output_excerpt
        );
        // Stdin and the list file carry the same bytes.
        let out = run_command_gate(
            d.path(),
            &spec(
                "sh",
                &["-c", "read p; test -e \"$p\""],
                PopulationDelivery::Stdin,
            ),
            std::slice::from_ref(&odd),
            1,
        );
        assert!(out.verdict.is_pass(), "stdin: {:?}", out.verdict);
        let out = run_command_gate(
            d.path(),
            &spec(
                "sh",
                &["-c", "read p < \"$FL_POPULATION_FILE\"; test -e \"$p\""],
                PopulationDelivery::FileList,
            ),
            std::slice::from_ref(&odd),
            1,
        );
        assert!(out.verdict.is_pass(), "file list: {:?}", out.verdict);
    }

    #[test]
    fn the_excerpt_is_bounded_on_a_char_boundary_and_keeps_stderr_visible_when_short() {
        let big = "é".repeat(EXCERPT_LIMIT); // 2 bytes each: the cut lands mid-sequence at odd limits
        let cut = excerpt(big.as_bytes(), b"");
        assert!(cut.len() <= EXCERPT_LIMIT);
        assert!(cut.chars().all(|c| c == 'é'));
        assert_eq!(excerpt(b"out", b"err"), "outerr");
        assert_eq!(excerpt(b"", b""), "");
        assert!(excerpt(&[0xff, 0xfe, 0xfd], b"").contains('\u{fffd}'));
    }

    #[test]
    fn an_unrecognised_exit_code_and_a_signal_death_are_failures() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.txt"), "x").unwrap();
        let pop = [d.path().join("a.txt")];
        let mut s = spec("sh", &["-c", "exit 3"], PopulationDelivery::FileList);
        s.pass_codes = vec![0, 2];
        assert!(!run_command_gate(d.path(), &s, &pop, 1).verdict.is_pass());
        s.pass_codes = vec![0, 3];
        assert!(run_command_gate(d.path(), &s, &pop, 1).verdict.is_pass());
        // Killed by a signal: no exit code, never a pass for a gate that lists only 0.
        let mut s = spec("sh", &["-c", "kill -KILL $$"], PopulationDelivery::FileList);
        s.pass_codes = vec![0];
        assert!(!run_command_gate(d.path(), &s, &pop, 1).verdict.is_pass());
        // An empty pass list can never pass.
        s = spec("true", &[], PopulationDelivery::FileList);
        s.pass_codes = vec![];
        assert!(!run_command_gate(d.path(), &s, &pop, 1).verdict.is_pass());
    }
}
