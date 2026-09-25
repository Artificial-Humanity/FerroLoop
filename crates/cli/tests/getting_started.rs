//! `docs/getting-started.md` opens by claiming "Every command below was
//! actually run to produce the output shown." Until this test existed that
//! claim was kept true by hand, after every change — the same arrangement
//! `WORKFLOW.md` had with CI, and it held for exactly as long as somebody
//! remembered.
//!
//! This runs the document. It extracts every shell block, executes them in
//! order in one shell so `cd` and the file writes carry, and compares each
//! command's real output and exit status against what the page prints.

use std::path::PathBuf;
use std::process::Command;

const DOC: &str = "../../docs/getting-started.md";

/// How many commands the document is expected to verify.
///
/// ⚠ Pinned deliberately. Without it, a block the parser silently failed to
/// pick up would leave this test passing over fewer and fewer commands —
/// vacuously, and most convincingly right when it had stopped checking
/// anything. A count that falls is as much a failure as a mismatch.
const VERIFIED_COMMANDS: usize = 65;

/// Blocks that cannot be reproduced and are skipped on purpose.
///
/// Each is matched on its own text, not its position, so reordering the
/// document cannot quietly change which blocks are exempt. Nothing else is
/// skippable: a block that stops matching becomes a checked block.
const SKIPPED: [&str; 2] = [
    // The versions the page's output came from. These print whatever the
    // reading machine has, which is the point of showing them.
    "rustc --version",
    // Compilation, with a duration in the output and minutes of work to
    // reproduce. `cargo test` has already built the binary this test runs.
    "cargo build --release",
];

/// Normalize, drop blanks, and collapse relayed program output to a marker.
///
/// ⚠ A line beginning with a tab and `| ` is not this project's output — it
/// is the gate program's own, which `check` indents and relays verbatim. The
/// guide's example validator is `python3`, so that excerpt is a CPython
/// traceback, and CPython renders tracebacks differently between versions:
/// this ran green locally on 3.14.4 and red in CI on 3.12, on nothing but
/// frame formatting.
///
/// Pinning it would be pinning another project's output on a version this
/// repository does not control. What is still checked: that the verdict line
/// is exactly right, that the exit status is right, and that an excerpt was
/// relayed at all when the page shows one. What is deliberately NOT checked
/// is the excerpt's contents.
fn collapse_relayed(lines: &[String], norm: &dyn Fn(&str) -> String) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        if line.starts_with("\t| ") || line.starts_with("\t|") {
            if out.last().map(String::as_str) != Some("<RELAYED PROGRAM OUTPUT>") {
                out.push("<RELAYED PROGRAM OUTPUT>".to_string());
            }
            continue;
        }
        let n = norm(line);
        if !n.is_empty() {
            out.push(n);
        }
    }
    out
}

struct Step {
    command: String,
    expected: Vec<String>,
    expected_exit: Option<i32>,
}

/// Replace anything that legitimately differs between two runs.
fn normalize(line: &str, demo_root: &str) -> String {
    let line = line.replace(demo_root, "<ROOT>");
    let bytes: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        // A 40-character hex run is a git commit: different every run.
        if bytes.len() - i >= 40
            && bytes[i..i + 40].iter().all(|c| c.is_ascii_hexdigit())
            && bytes[i..i + 40].iter().all(|c| !c.is_ascii_uppercase())
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && (i + 40 == bytes.len() || !bytes[i + 40].is_ascii_alphanumeric())
        {
            out.push_str("<HASH>");
            i += 40;
            continue;
        }
        // A duration in milliseconds: wall-clock, never identical.
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if bytes.len() - j >= 2 && bytes[j] == 'm' && bytes[j + 1] == 's' {
                out.push_str("<MS>");
                i = j + 2;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out.trim_end().to_string()
}

/// Pull the fenced blocks out of the page, and each block into steps.
fn parse(doc: &str) -> (Vec<Step>, usize) {
    let mut steps = Vec::new();
    let mut skipped = 0;
    let mut in_block = false;
    let mut block: Vec<&str> = Vec::new();

    for line in doc.lines() {
        if line == "```" {
            if in_block {
                if block.first().is_some_and(|l| l.starts_with("$ ")) {
                    let text = block.join("\n");
                    if SKIPPED.iter().any(|s| text.contains(s)) {
                        skipped += 1;
                    } else {
                        steps.extend(parse_block(&block));
                    }
                }
                block.clear();
            }
            in_block = !in_block;
            continue;
        }
        if in_block {
            block.push(line);
        }
    }
    (steps, skipped)
}

fn parse_block(lines: &[&str]) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(rest) = line.strip_prefix("$ ") {
            // `echo "exit: $?"` is not a step. It asserts the status of the
            // command before it, so record it there and drop the echo —
            // running it would report the status of whatever this harness
            // did last, not what the page is talking about.
            if rest.trim() == "echo \"exit: $?\"" {
                let code = lines
                    .get(i + 1)
                    .and_then(|l| l.strip_prefix("exit: "))
                    .and_then(|n| n.trim().parse::<i32>().ok())
                    .expect("`echo \"exit: $?\"` must be followed by `exit: <n>`");
                steps
                    .last_mut()
                    .expect("an exit assertion with no command before it")
                    .expected_exit = Some(code);
                i += 2;
                continue;
            }

            let mut command = rest.to_string();
            // Continuations: a trailing backslash means the next line is
            // part of this command.
            while command.trim_end().ends_with('\\') {
                i += 1;
                command.push('\n');
                command.push_str(lines[i]);
            }
            // Heredocs: everything up to and including the terminator is the
            // command's input, not its output.
            if command.contains("<<'EOF'") {
                loop {
                    i += 1;
                    command.push('\n');
                    command.push_str(lines[i]);
                    if lines[i].trim() == "EOF" {
                        break;
                    }
                }
            }
            steps.push(Step {
                command,
                expected: Vec::new(),
                expected_exit: None,
            });
            i += 1;
            continue;
        }
        // Anything else is output the page claims the last command produced.
        if let Some(last) = steps.last_mut() {
            last.expected.push(line.to_string());
        }
        i += 1;
    }
    steps
}

#[test]
fn every_command_in_the_getting_started_guide_produces_the_output_it_prints() {
    let doc_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DOC);
    let doc = std::fs::read_to_string(&doc_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", doc_path.display()));

    let (steps, skipped) = parse(&doc);
    assert_eq!(
        skipped,
        SKIPPED.len(),
        "a block listed in SKIPPED no longer matches, or a new one was exempted"
    );

    // The page's example gates validate JSON with `python3`, and its setup
    // drives `git`. Fail naming the missing tool rather than letting it
    // surface as a dozen unexplained gate errors — and fail rather than
    // skip, because a guard that quietly opts out is the thing this test
    // was written to stop.
    for tool in ["git", "python3", "sh"] {
        assert!(
            Command::new(tool).arg("--version").output().is_ok(),
            "docs/getting-started.md runs `{tool}`, which is not on PATH"
        );
    }

    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("gs-demo");
    let root_str = root.display().to_string();

    // The page's own paths are rewritten to this run's scratch directory —
    // in the commands AND in the expected output, so both sides move
    // together and the comparison stays honest.
    let rewrite = |s: &str| s.replace("/tmp/gs-demo", &root_str);

    let bin = assert_cmd::cargo::cargo_bin("fl");
    let bin_dir = bin.parent().unwrap().display().to_string();
    let path = format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default());

    // One script, so `cd` and every file the page writes carry between
    // commands exactly as they do for a reader following along.
    // ⚠ `exec 2>&1` inside the script, not two streams merged afterwards.
    // The page interleaves stderr with stdout the way a terminal does, so
    // the ordering has to be produced by the shell. Merging captured streams
    // after the fact put every error line after every marker, which read as
    // "the command printed nothing" for each of the five refusals the page
    // documents.
    let mut script = String::from("exec 2>&1\nset +e\n");
    for (n, step) in steps.iter().enumerate() {
        // `target/release/fl` is how the page refers to the binary it just
        // built; this run's binary is wherever cargo put it.
        script.push_str(
            &rewrite(&step.command).replace("target/release/fl", &bin.display().to_string()),
        );
        script.push('\n');
        script.push_str(&format!("printf '<<<STEP %d %d>>>\\n' {n} \"$?\"\n"));
    }

    let out = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .current_dir(home.path())
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        // Fix round 1 — Important 5: every command below passes `--db`
        // explicitly (which only confines which store it uses), but `fl`
        // reads and validates the user's config unconditionally — this
        // script, unlike the other suites, does not clear its environment,
        // so without this it would read the developer's own
        // ~/.config/fl/config.toml.
        .env("XDG_CONFIG_HOME", home.path())
        .output()
        .expect("the document's commands could not be run");
    let combined = String::from_utf8_lossy(&out.stdout).to_string();

    // Split the transcript back into per-command chunks.
    let mut actual: Vec<(Vec<String>, i32)> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in combined.lines() {
        if let Some(rest) = line.strip_prefix("<<<STEP ") {
            let rest = rest.trim_end_matches(">>>");
            let mut parts = rest.split_whitespace();
            let idx: usize = parts.next().unwrap().parse().unwrap();
            let code: i32 = parts.next().unwrap().parse().unwrap();
            assert_eq!(
                idx,
                actual.len(),
                "the transcript's markers are out of order"
            );
            actual.push((std::mem::take(&mut current), code));
            continue;
        }
        current.push(line.to_string());
    }
    assert_eq!(
        actual.len(),
        steps.len(),
        "the document has {} commands but the transcript has {} — a command killed the shell",
        steps.len(),
        actual.len()
    );

    let mut checked = 0usize;
    let mut failures = Vec::new();
    for (n, (step, (got, code))) in steps.iter().zip(actual.iter()).enumerate() {
        let want = collapse_relayed(&step.expected, &|l| normalize(&rewrite(l), &root_str));
        let have = collapse_relayed(got, &|l| normalize(l, &root_str));
        if want != have {
            failures.push(format!(
                "command {n}: `{}`\n  the page prints:\n{}\n  it actually printed:\n{}",
                step.command.lines().next().unwrap_or(""),
                want.iter()
                    .map(|l| format!("    {l}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                have.iter()
                    .map(|l| format!("    {l}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
        }
        if let Some(expected) = step.expected_exit
            && expected != *code
        {
            failures.push(format!(
                "command {n}: `{}` — the page says exit {expected}, it exited {code}",
                step.command.lines().next().unwrap_or("")
            ));
        }
        checked += 1;
    }

    assert!(
        failures.is_empty(),
        "docs/getting-started.md does not match what the commands do:\n\n{}",
        failures.join("\n\n")
    );
    assert_eq!(
        checked, VERIFIED_COMMANDS,
        "the guide now has {checked} verifiable commands, not {VERIFIED_COMMANDS}. \
         If that is deliberate, update VERIFIED_COMMANDS; if it fell, the parser \
         has stopped seeing part of the page."
    );
}
