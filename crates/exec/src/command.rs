use crate::population::ExecError;
use fl_core::model::{CommandSpec, PopulationDelivery};
use fl_core::verdict::{FailReason, Verdict};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const EXCERPT_LIMIT: usize = 4096;

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
    cmd.args(&spec.args).current_dir(root).stdout(Stdio::piped()).stderr(Stdio::piped());

    let paths: Vec<String> = population.iter().map(|p| p.display().to_string()).collect();
    match spec.delivery {
        PopulationDelivery::Args => {
            cmd.args(&paths);
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
        match write_list(&paths) {
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
        let payload = paths.join("\n");
        let _ = stdin.write_all(payload.as_bytes());
        let _ = stdin.write_all(b"\n");
        // `stdin` drops here, closing the pipe so the child sees EOF.
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

fn write_list(paths: &[String]) -> std::io::Result<tempfile::NamedTempFile> {
    let mut f = tempfile::NamedTempFile::new()?;
    for p in paths {
        writeln!(f, "{p}")?;
    }
    f.flush()?;
    Ok(f)
}

/// Start a thread that reads a pipe to completion into an owned buffer.
///
/// Spawned before the wait loop starts, so the pipe is always being drained
/// while the child runs. A child that writes more than the OS pipe buffer
/// (64 KiB on Linux) blocks on the next write until someone reads; a reader
/// that only shows up after the child exits, or that only polls inside the
/// `try_wait` loop without actually draining, never unblocks a child in that
/// state — and the deadline then fires on a process that was only waiting
/// for a reader, not doing anything wrong.
fn spawn_drain(mut pipe: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

fn join_drain(handle: JoinHandle<Vec<u8>>) -> Vec<u8> {
    handle.join().unwrap_or_default()
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
fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    let stdout_handle = child.stdout.take().map(spawn_drain);
    let stderr_handle = child.stderr.take().map(spawn_drain);

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= deadline {
            // The drain threads are left running; they will see EOF and
            // finish on their own once the caller kills the child, and a
            // detached JoinHandle dropped without joining leaks nothing but
            // the thread's own stack until then.
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    let stdout = stdout_handle.map(join_drain).unwrap_or_default();
    let stderr = stderr_handle.map(join_drain).unwrap_or_default();

    Ok(Some(std::process::Output { status, stdout, stderr }))
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
        assert_eq!(out.verdict, Verdict::fail_for(FailReason::EmptyPopulation, 0));
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
        assert!(out.output_excerpt.contains("total"), "got {}", out.output_excerpt);
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
        assert_eq!(out.verdict, Verdict::fail_for(FailReason::EmptyPopulation, 1));
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
            &spec("definitely-not-a-real-program-9f3x", &[], PopulationDelivery::Args),
            &[d.path().join("a.txt")],
            1,
        );
        assert!(matches!(out.verdict, Verdict::Error { .. }), "got {:?}", out.verdict);
        assert_eq!(out.verdict.exit_code(), 2);
    }

    // REQUIRED TEST 4 (spec §9), part two: a timeout is not a pass.
    //
    // Deviation from the brief's literal fixture: the brief uses
    // `PopulationDelivery::Args` here. Under `Args` delivery the resolved
    // population is appended to the command's own argv (that is the entire
    // point of `Args` delivery — see
    // `the_command_receives_every_path_the_framework_resolved` above), so
    // the child actually spawned is `sleep 30 <path-to-a.txt>`. GNU
    // coreutils' `sleep` treats every positional argument as a time
    // interval and rejects a non-numeric one immediately: `sleep 30
    // /tmp/xyz/a.txt` exits 1 in a few milliseconds with "invalid time
    // interval", never sleeps, and the gate observes an ordinary nonzero
    // exit (`Fail { reason: Predicate }`), not a timeout — verified
    // directly against the system `sleep` binary before making this change.
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
        assert!(matches!(out.verdict, Verdict::Error { .. }), "got {:?}", out.verdict);
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
        assert!(out.output_excerpt.trim().starts_with('2'), "got {}", out.output_excerpt);
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
        assert_eq!(out.verdict, Verdict::from_predicate(true, COUNT as u64), "got {:?}", out.verdict);
        // The excerpt is capped at EXCERPT_LIMIT and this fixture's raw
        // output is well over that, so the excerpt itself is expected to be
        // a truncated prefix — not the "total" line, which is the very
        // last thing `wc -l` would have written.
        assert_eq!(out.output_excerpt.len(), EXCERPT_LIMIT, "got {} bytes", out.output_excerpt.len());
        // This system's `wc` (uutils coreutils 0.8.0) right-justifies the
        // per-file count to a column width derived from the row count
        // (4000 files + 1 total line needs 4 digits), so a zero count reads
        // as 3 spaces of padding then "0" — verified against the live
        // binary rather than assumed from GNU wc's behaviour.
        assert!(out.output_excerpt.starts_with("   0 "), "got {:?}", out.output_excerpt);
    }
}
