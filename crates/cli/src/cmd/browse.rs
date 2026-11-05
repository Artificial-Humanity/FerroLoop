//! `fl browse`: FerroBrowse, a token-efficient view of a web page and a UI and accessibility
//! gate. Every report comes from `fl-browse`; this module resolves arguments and renders.
//!
//! It opens no store and reads no project or config, so it works anywhere, and `fl gate` can run
//! `fl browse check` directly. The only network access is the fetch of the target itself; the
//! only process it can start is a headless browser binary, and only for `snapshot` with
//! `--engine auto|cdp` or `--screenshot` (and `doctor`, which runs a found binary's
//! `--version`).
//!
//! Exit status: `0` success (for `check`: the gate passes), `1` `check` found findings that fail
//! the gate, `2` any other failure (a bad argument, an unreachable or non-HTML target, a missing
//! engine).

use clap::{Args, Subcommand, ValueEnum};
use fl_browse::{
    AuditReport, BrowseError, DEFAULT_CHECK_VIEWPORT, DEFAULT_SNAPSHOT_VIEWPORT, EngineMode,
    FetchOptions, HeadlessEngineProbe, SnapshotOptions, SnapshotOutcome, Viewport,
};
use serde_json::{Value, json};
use std::path::PathBuf;

/// Exit status of `check` when the gate fails.
pub const EXIT_GATE_FAILED: i32 = 1;

#[derive(Args, Debug)]
pub struct BrowseArgs {
    #[command(subcommand)]
    pub cmd: BrowseCmd,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum EngineArg {
    /// Tier 1 only: nothing is spawned.
    Native,
    /// A headless browser binary when one is installed, otherwise Tier 1.
    Auto,
    /// A headless browser binary; an error when none is installed.
    Cdp,
}

impl From<EngineArg> for EngineMode {
    fn from(e: EngineArg) -> Self {
        match e {
            EngineArg::Native => EngineMode::Native,
            EngineArg::Auto => EngineMode::Auto,
            EngineArg::Cdp => EngineMode::Cdp,
        }
    }
}

fn parse_viewport(s: &str) -> Result<Viewport, String> {
    s.parse::<Viewport>().map_err(|e| e.to_string())
}

#[derive(Subcommand, Debug)]
pub enum BrowseCmd {
    /// Distill a page into an accessibility outline and a table of its interactive elements.
    ///
    /// <TARGET> is an http:// or https:// URL, a file:// URL, or a path to an HTML file. The
    /// elements get handles @e1, @e2, ... in document order. Tier 1 runs no JavaScript and
    /// computes no layout; `--engine auto|cdp` or `--screenshot` renders the page in a headless
    /// browser binary (found at $FL_BROWSE_ENGINE_BIN or on PATH) and distills the rendered DOM.
    /// The report ends with the size metrics: raw_bytes (the document), distilled_bytes (the
    /// printed outline and table) and token_savings_pct, which is
    /// round((1 - distilled_bytes / raw_bytes) * 1000) / 10 (a byte ratio, not a token count).
    Snapshot {
        /// An http(s) URL, a file:// URL or a path to an HTML file.
        target: String,
        /// The window size an engine renders at, WIDTHxHEIGHT (default 1280x800).
        #[arg(long, value_name = "WxH", value_parser = parse_viewport)]
        viewport: Option<Viewport>,
        /// Which tier renders the page (default native).
        #[arg(long, value_enum)]
        engine: Option<EngineArg>,
        /// Also write a screenshot to this path. Needs a headless browser binary; fails when
        /// there is none.
        #[arg(long, value_name = "PATH")]
        screenshot: Option<PathBuf>,
        /// Print one JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Audit a page against the UI and accessibility rules and exit 1 when the gate fails.
    ///
    /// Always Tier 1: nothing is spawned. Exit 0 when there is no error finding (and, with
    /// --strict, no warning), exit 1 otherwise, so `fl gate` can run it directly.
    ///
    /// Checks (code and severity):
    ///   missing-viewport-meta (error)
    ///   unlabeled-control (error)
    ///   ios-contact-autofill-hazard (error)
    ///   fixed-min-width-overflow-hazard (warning)
    #[command(verbatim_doc_comment)]
    Check {
        /// An http(s) URL, a file:// URL or a path to an HTML file.
        target: String,
        /// The viewport to audit at, WIDTHxHEIGHT (default 320x568).
        #[arg(long, value_name = "WxH", value_parser = parse_viewport)]
        viewport: Option<Viewport>,
        /// Fail on warnings too.
        #[arg(long)]
        strict: bool,
        /// Print one JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Report the two tiers: the native distiller (always ready) and the optional headless
    /// browser binary. Exit 1 only when $FL_BROWSE_ENGINE_BIN names something that cannot run.
    Doctor {
        /// Print one JSON object.
        #[arg(long)]
        json: bool,
    },
}

type Failure = (i32, String);

fn fail(e: BrowseError) -> Failure {
    (2, e.to_string())
}

pub fn run(args: BrowseArgs) -> Result<i32, Failure> {
    match args.cmd {
        BrowseCmd::Snapshot {
            target,
            viewport,
            engine,
            screenshot,
            json,
        } => snapshot(&target, viewport, engine, screenshot, json),
        BrowseCmd::Check {
            target,
            viewport,
            strict,
            json,
        } => check(&target, viewport, strict, json),
        BrowseCmd::Doctor { json } => doctor(json),
    }
}

fn print_json(v: &Value) -> Result<(), Failure> {
    let text = serde_json::to_string_pretty(v)
        .map_err(|e| (2, format!("could not encode the JSON report: {e}")))?;
    println!("{text}");
    Ok(())
}

fn snapshot(
    target: &str,
    viewport: Option<Viewport>,
    engine: Option<EngineArg>,
    screenshot: Option<PathBuf>,
    json: bool,
) -> Result<i32, Failure> {
    let mut opts = SnapshotOptions::new(HeadlessEngineProbe::from_env());
    opts.viewport = viewport.unwrap_or(DEFAULT_SNAPSHOT_VIEWPORT);
    opts.engine = engine.map(Into::into);
    opts.screenshot = screenshot;
    let out = fl_browse::run_snapshot(target, &opts).map_err(fail)?;
    if json {
        print_json(&snapshot_json(target, opts.viewport, &out))?;
    } else {
        print!("{}", snapshot_text(target, opts.viewport, &out));
    }
    Ok(0)
}

fn snapshot_json(target: &str, viewport: Viewport, out: &SnapshotOutcome) -> Value {
    let s = &out.snapshot;
    json!({
        "schema_version": 1,
        "target": target,
        "engine": out.engine,
        "viewport": viewport,
        "screenshot": out.screenshot.as_ref().map(|p| p.display().to_string()),
        "truncated": s.truncated,
        "title": s.title,
        "viewport_meta": s.viewport_meta,
        "landmarks": s.landmarks,
        "headings": s.headings,
        "elements": s.elements,
        "outline": s.outline,
        "metrics": s.metrics,
    })
}

fn snapshot_text(target: &str, viewport: Viewport, out: &SnapshotOutcome) -> String {
    let s = &out.snapshot;
    let mut t = format!(
        "# fl browse snapshot\ntarget: {target}\nengine: {}\nviewport: {viewport}\n",
        out.engine
    );
    if let Some(p) = &out.screenshot {
        t.push_str(&format!("screenshot: {}\n", p.display()));
    }
    if s.truncated {
        t.push_str("note: the document hit a parser limit; this is part of it\n");
    }
    t.push('\n');
    t.push_str(&s.render_body());
    t.push_str(&format!(
        "\n# metrics\nraw_bytes\t{}\ndistilled_bytes\t{}\ntoken_savings_pct\t{}\n",
        s.metrics.raw_bytes, s.metrics.distilled_bytes, s.metrics.token_savings_pct
    ));
    t
}

fn check(
    target: &str,
    viewport: Option<Viewport>,
    strict: bool,
    json: bool,
) -> Result<i32, Failure> {
    let viewport = viewport.unwrap_or(DEFAULT_CHECK_VIEWPORT);
    let report =
        fl_browse::audit_target(target, viewport, &FetchOptions::default()).map_err(fail)?;
    let passed = report.passed(strict);
    if json {
        print_json(&check_json(target, strict, passed, &report))?;
    } else {
        print!("{}", check_text(target, strict, passed, &report));
    }
    Ok(if passed { 0 } else { EXIT_GATE_FAILED })
}

fn check_json(target: &str, strict: bool, passed: bool, r: &AuditReport) -> Value {
    json!({
        "schema_version": 1,
        "target": target,
        "viewport": r.viewport,
        "strict": strict,
        "passed": passed,
        "error_count": r.error_count,
        "warning_count": r.warning_count,
        "findings": r.findings,
    })
}

fn check_text(target: &str, strict: bool, passed: bool, r: &AuditReport) -> String {
    let mut t = String::from("severity\tcode\thandle\tmessage\n");
    for f in &r.findings {
        t.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            f.severity.as_str(),
            f.code,
            f.handle.as_deref().unwrap_or("-"),
            f.message.replace(['\t', '\n', '\r'], " ")
        ));
    }
    t.push_str(&format!(
        "\n{}: {} error(s), {} warning(s) at {} for {target}{}\n",
        if passed { "pass" } else { "FAIL" },
        r.error_count,
        r.warning_count,
        r.viewport,
        if strict { " (strict)" } else { "" }
    ));
    t
}

fn doctor(json: bool) -> Result<i32, Failure> {
    let report = fl_browse::doctor(&HeadlessEngineProbe::from_env());
    if json {
        print_json(&serde_json::to_value(&report).map_err(|e| (2, e.to_string()))?)?;
    } else {
        println!("tier\tid\tstatus\tpath\tversion\tdetail");
        for t in &report.tiers {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                t.tier,
                t.id,
                t.status,
                t.path.as_deref().unwrap_or("-"),
                t.version.as_deref().unwrap_or("-"),
                t.detail
            );
        }
    }
    Ok(if report.ok() { 0 } else { 1 })
}

/// The codes `check` can emit, with their severity.
#[cfg(test)]
fn check_codes() -> Vec<(&'static str, &'static str)> {
    fl_browse::audit::CHECKS
        .iter()
        .map(|(c, s)| (*c, s.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_help_names_every_check_with_its_severity() {
        let help = {
            use clap::CommandFactory;
            #[derive(clap::Parser)]
            struct W {
                #[command(subcommand)]
                c: BrowseCmd,
            }
            let mut cmd = W::command();
            let sub = cmd.find_subcommand_mut("check").unwrap();
            sub.render_long_help().to_string()
        };
        for (code, sev) in check_codes() {
            assert!(help.contains(&format!("{code} ({sev})")), "{code}: {help}");
        }
    }

    #[test]
    fn a_bad_viewport_is_refused_at_parse_time() {
        assert!(parse_viewport("320x568").is_ok());
        let e = parse_viewport("wide").unwrap_err();
        assert!(e.contains("WIDTHxHEIGHT"), "{e}");
    }
}
