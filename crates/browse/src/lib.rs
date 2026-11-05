//! FerroBrowse: what an agent needs to know about a web page, in few bytes, without a browser.
//!
//! Two tiers:
//!
//! * **Tier 1, the native distiller** ([`fetch`], [`html`], [`distill`]): always available.
//!   It fetches an `http(s)` URL or reads a local file, parses the HTML with a small tolerant
//!   parser, and distills an accessibility outline plus a table of the interactive elements,
//!   each with a handle (`@e1`, `@e2`, ...). It runs no JavaScript and computes no layout.
//! * **Tier 2, a headless browser binary** ([`engine`]): optional. When one is installed and
//!   asked for, it renders the page and the distiller reads the rendered DOM; it can also write
//!   a screenshot.
//!
//! [`audit`] judges the same scan against a short list of UI and accessibility rules, so the
//! result can gate a change.
//!
//! This crate depends on no other FerroLoop crate: it opens no store and needs no project.

pub mod audit;
pub mod distill;
pub mod engine;
pub mod fetch;
pub mod html;

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

pub use audit::{AuditReport, BrowseFinding, Severity, audit_document, audit_html};
pub use distill::{Metrics, Snapshot, distill_document, distill_html};
pub use engine::{EngineInfo, HeadlessEngineProbe};
pub use fetch::{FetchOptions, Target, parse_target};

/// The viewport `check` audits at when none is given: a small phone.
pub const DEFAULT_CHECK_VIEWPORT: Viewport = Viewport {
    width: 320,
    height: 568,
};
/// The window `snapshot` asks an engine for when no viewport is given.
pub const DEFAULT_SNAPSHOT_VIEWPORT: Viewport = Viewport {
    width: 1280,
    height: 800,
};

#[derive(Debug, thiserror::Error)]
pub enum BrowseError {
    #[error(
        "invalid viewport `{0}`: write WIDTHxHEIGHT with whole numbers from 1 to 10000, \
         for example 320x568"
    )]
    InvalidViewport(String),
    #[error("{0}")]
    InvalidTarget(String),
    #[error(
        "unsupported scheme `{0}`: a target is an http:// or https:// URL, a file:// URL or a \
         path to an HTML file"
    )]
    UnsupportedScheme(String),
    #[error(
        "the URL carries a user name or password; fl browse sends no credentials, so remove \
         them from the URL"
    )]
    CredentialsInUrl,
    #[error("could not fetch {url}: {message}")]
    Http { url: String, message: String },
    #[error("not HTML: {0}")]
    NotHtml(String),
    #[error("{what} is larger than the {limit} byte limit")]
    TooLarge { what: String, limit: usize },
    #[error("could not read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{FL_ENGINE_ENV} is set to `{0}`, which is not an executable file; unset it or point it \
         at a headless browser binary"
    )]
    EngineBinInvalid(String),
    #[error("{0}")]
    NoEngine(String),
    #[error("{0}")]
    EngineFailed(String),
    #[error("{0}")]
    Usage(String),
}

const FL_ENGINE_ENV: &str = engine::ENGINE_ENV;

/// A viewport in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

/// The largest side of a viewport.
pub const MAX_VIEWPORT_SIDE: u32 = 10_000;

impl FromStr for Viewport {
    type Err = BrowseError;

    /// `WIDTHxHEIGHT`: two whole numbers of ASCII digits from 1 to [`MAX_VIEWPORT_SIDE`],
    /// a lowercase `x` between them, nothing else (no sign, no space, no leading zero).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || BrowseError::InvalidViewport(s.to_string());
        let (w, h) = s.split_once('x').ok_or_else(bad)?;
        let side = |t: &str| -> Option<u32> {
            if t.is_empty() || t.len() > 5 || !t.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            if t.starts_with('0') {
                return None;
            }
            let v: u32 = t.parse().ok()?;
            (1..=MAX_VIEWPORT_SIDE).contains(&v).then_some(v)
        };
        match (side(w), side(h)) {
            (Some(width), Some(height)) => Ok(Viewport { width, height }),
            _ => Err(bad()),
        }
    }
}

impl std::fmt::Display for Viewport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// Which tier renders the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineMode {
    /// Tier 1 only. Nothing is spawned.
    Native,
    /// Tier 2 when a binary is installed, otherwise Tier 1.
    Auto,
    /// Tier 2; an error when no binary is installed.
    Cdp,
}

/// Everything a snapshot run needs.
#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    pub viewport: Viewport,
    /// `None` is the default: Tier 1, or Tier 2 when a screenshot is asked for.
    pub engine: Option<EngineMode>,
    pub screenshot: Option<PathBuf>,
    pub probe: HeadlessEngineProbe,
    pub fetch: FetchOptions,
    pub engine_timeout: Duration,
}

impl SnapshotOptions {
    pub fn new(probe: HeadlessEngineProbe) -> Self {
        Self {
            viewport: DEFAULT_SNAPSHOT_VIEWPORT,
            engine: None,
            screenshot: None,
            probe,
            fetch: FetchOptions::default(),
            engine_timeout: engine::DEFAULT_ENGINE_TIMEOUT,
        }
    }
}

/// The result of [`run_snapshot`].
#[derive(Debug, Clone)]
pub struct SnapshotOutcome {
    pub snapshot: Snapshot,
    /// `native`, or `headless:<binary name>`.
    pub engine: String,
    pub screenshot: Option<PathBuf>,
}

fn absolute(p: &Path) -> Result<PathBuf, BrowseError> {
    if p.as_os_str().is_empty() {
        return Err(BrowseError::Usage("the screenshot path is empty".into()));
    }
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(|e| BrowseError::Io {
        path: ".".into(),
        source: e,
    })?;
    Ok(cwd.join(p))
}

/// The URL an engine is given for a target.
fn engine_url(target: &Target) -> Result<String, BrowseError> {
    match target {
        Target::Http(u) => Ok(u.clone()),
        Target::File(p) => {
            let abs = std::fs::canonicalize(p).map_err(|e| BrowseError::Io {
                path: p.display().to_string(),
                source: e,
            })?;
            Ok(fetch::file_url(&abs))
        }
    }
}

/// Resolve the target, render it in the chosen tier and distill it.
pub fn run_snapshot(target: &str, opts: &SnapshotOptions) -> Result<SnapshotOutcome, BrowseError> {
    if opts.engine == Some(EngineMode::Native) && opts.screenshot.is_some() {
        return Err(BrowseError::Usage(
            "--screenshot needs a headless browser, and `--engine native` never starts one; \
             drop `--engine native`, or use `--engine auto` or `--engine cdp`"
                .into(),
        ));
    }
    let parsed = parse_target(target)?;
    let screenshot = opts.screenshot.as_deref().map(absolute).transpose()?;
    if let Some(s) = &screenshot {
        match s.parent() {
            Some(d) if d.is_dir() => {}
            _ => {
                return Err(BrowseError::Usage(format!(
                    "the directory for the screenshot {} does not exist",
                    s.display()
                )));
            }
        }
    }
    let mode = opts.engine.unwrap_or(EngineMode::Native);
    let wants_engine = mode != EngineMode::Native || screenshot.is_some();
    let found = if wants_engine {
        opts.probe.detect()?
    } else {
        None
    };
    match found {
        Some(engine) => {
            let url = engine_url(&parsed)?;
            let dom = engine::dump_dom(
                &engine,
                &url,
                opts.viewport,
                screenshot.as_deref(),
                opts.engine_timeout,
            )?;
            Ok(SnapshotOutcome {
                snapshot: distill_html(&dom),
                engine: format!("headless:{}", engine.name),
                screenshot,
            })
        }
        None if mode == EngineMode::Cdp || screenshot.is_some() => Err(BrowseError::NoEngine(
            no_engine_message(if screenshot.is_some() {
                "--screenshot needs a headless browser"
            } else {
                "`--engine cdp` needs a headless browser"
            }),
        )),
        None => {
            let fetched = fetch::fetch(&parsed, &opts.fetch)?;
            Ok(SnapshotOutcome {
                snapshot: distill_html(&fetched.html),
                engine: "native".into(),
                screenshot: None,
            })
        }
    }
}

fn no_engine_message(why: &str) -> String {
    format!(
        "{why}, and none is installed. Install one of {} (found on PATH or in the standard \
         directories), or set {FL_ENGINE_ENV} to its path. `fl browse doctor` shows what is \
         detected",
        engine::ENGINE_CANDIDATES.join(", ")
    )
}

/// Fetch (or read) the target and audit it at `viewport`. This is Tier 1 only: nothing is
/// spawned.
pub fn audit_target(
    target: &str,
    viewport: Viewport,
    fetch_opts: &FetchOptions,
) -> Result<AuditReport, BrowseError> {
    let fetched = fetch::fetch(&parse_target(target)?, fetch_opts)?;
    Ok(audit_html(&fetched.html, viewport))
}

/// The state of one tier, for `doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TierStatus {
    pub id: String,
    pub tier: u8,
    /// `ready`, `detected`, `not-installed` or `misconfigured`.
    pub status: String,
    pub path: Option<String>,
    pub version: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub tiers: Vec<TierStatus>,
}

impl DoctorReport {
    /// Whether every tier is usable as configured: the engine may be absent, but not
    /// misconfigured.
    pub fn ok(&self) -> bool {
        self.tiers.iter().all(|t| t.status != "misconfigured")
    }
}

/// Report both tiers. Tier 2 runs `<binary> --version` (and nothing else) when it finds one.
pub fn doctor(probe: &HeadlessEngineProbe) -> DoctorReport {
    let native = TierStatus {
        id: "native-distiller".into(),
        tier: 1,
        status: "ready".into(),
        path: None,
        version: Some(env!("CARGO_PKG_VERSION").into()),
        detail: "built in; fetches http(s) URLs and reads local files, runs no JavaScript".into(),
    };
    let headless = match probe.detect() {
        Ok(Some(e)) => TierStatus {
            id: "headless-binary".into(),
            tier: 2,
            status: "detected".into(),
            version: engine::engine_version(&e),
            detail: format!(
                "{} (found via {})",
                e.name,
                if e.from_env { FL_ENGINE_ENV } else { "PATH" }
            ),
            path: Some(e.path.display().to_string()),
        },
        Ok(None) => TierStatus {
            id: "headless-binary".into(),
            tier: 2,
            status: "not-installed".into(),
            path: None,
            version: None,
            detail: format!(
                "none of {} found; optional, needed only for --engine auto|cdp and --screenshot",
                engine::ENGINE_CANDIDATES.join(", ")
            ),
        },
        Err(e) => TierStatus {
            id: "headless-binary".into(),
            tier: 2,
            status: "misconfigured".into(),
            path: None,
            version: None,
            detail: e.to_string(),
        },
    };
    DoctorReport {
        schema_version: 1,
        tiers: vec![native, headless],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn vp(s: &str) -> Result<Viewport, BrowseError> {
        s.parse()
    }

    #[test]
    fn viewport_accepts_whole_numbers_only() {
        assert_eq!(vp("320x568").unwrap(), DEFAULT_CHECK_VIEWPORT);
        assert_eq!(
            vp("1x1").unwrap(),
            Viewport {
                width: 1,
                height: 1
            }
        );
        assert_eq!(
            vp("10000x10000").unwrap(),
            Viewport {
                width: 10_000,
                height: 10_000
            }
        );
        assert_eq!(vp("1280x800").unwrap().to_string(), "1280x800");
    }

    #[test]
    fn viewport_rejects_everything_else_with_an_actionable_message() {
        for bad in [
            "",
            "x",
            "320",
            "320x",
            "x568",
            "320X568",
            "320*568",
            "320,568",
            "320 x 568",
            " 320x568",
            "320x568 ",
            "0x568",
            "320x0",
            "-320x568",
            "+320x568",
            "320x-1",
            "0320x568",
            "10001x1",
            "100000x1",
            "1.5x2",
            "3e2x2",
            "320x568x2",
            "abcxdef",
            "320x568\n",
            "\u{663}x1",
        ] {
            let e = vp(bad).unwrap_err();
            assert!(matches!(e, BrowseError::InvalidViewport(_)), "{bad:?}");
            assert!(e.to_string().contains("WIDTHxHEIGHT") && e.to_string().contains("320x568"));
        }
    }

    fn fake_engine(dir: &Path) -> PathBuf {
        let p = dir.join("chromium");
        std::fs::write(
            &p,
            "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in --screenshot=*) printf png > \"${a#--screenshot=}\";; esac; done\n\
             echo '<html><body><button>Rendered</button></body></html>'\n",
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn page(dir: &Path) -> PathBuf {
        let p = dir.join("p.html");
        std::fs::write(&p, "<html><body><button>Native</button></body></html>").unwrap();
        p
    }

    fn opts(engine_bin: Option<&Path>) -> SnapshotOptions {
        SnapshotOptions::new(HeadlessEngineProbe {
            engine_bin: engine_bin.map(|p| p.into()),
            path: Some("".into()),
            extra_dirs: vec![],
        })
    }

    #[test]
    fn native_is_the_default_and_never_probes_for_a_binary() {
        let dir = tempfile::tempdir().unwrap();
        let p = page(dir.path());
        // A bad engine variable would be an error if it were consulted.
        let mut o = opts(Some(Path::new("/nonexistent")));
        let out = run_snapshot(p.to_str().unwrap(), &o).unwrap();
        assert_eq!(out.engine, "native");
        assert_eq!(out.snapshot.elements[0].name, "Native");
        o.engine = Some(EngineMode::Native);
        assert_eq!(
            run_snapshot(p.to_str().unwrap(), &o).unwrap().engine,
            "native"
        );
    }

    #[test]
    fn auto_falls_back_to_native_and_cdp_requires_an_engine() {
        let dir = tempfile::tempdir().unwrap();
        let p = page(dir.path());
        let mut o = opts(None);
        o.engine = Some(EngineMode::Auto);
        let out = run_snapshot(p.to_str().unwrap(), &o).unwrap();
        assert_eq!(out.engine, "native");
        o.engine = Some(EngineMode::Cdp);
        let e = run_snapshot(p.to_str().unwrap(), &o).unwrap_err();
        assert!(matches!(e, BrowseError::NoEngine(_)));
        assert!(e.to_string().contains("FL_BROWSE_ENGINE_BIN"), "{e}");
    }

    #[test]
    fn auto_and_cdp_use_the_engine_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_engine(dir.path());
        let p = page(dir.path());
        for mode in [EngineMode::Auto, EngineMode::Cdp] {
            let mut o = opts(Some(&bin));
            o.engine = Some(mode);
            let out = run_snapshot(p.to_str().unwrap(), &o).unwrap();
            assert_eq!(out.engine, "headless:chromium");
            assert_eq!(out.snapshot.elements[0].name, "Rendered");
        }
    }

    #[test]
    fn a_screenshot_forces_the_engine_and_is_never_silently_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = page(dir.path());
        let shot = dir.path().join("s.png");
        // No engine: an error, with nothing written.
        let mut o = opts(None);
        o.screenshot = Some(shot.clone());
        let e = run_snapshot(p.to_str().unwrap(), &o).unwrap_err();
        assert!(matches!(e, BrowseError::NoEngine(_)), "{e}");
        assert!(e.to_string().contains("--screenshot"), "{e}");
        assert!(!shot.exists());
        // `native` plus a screenshot is a contradiction.
        o.engine = Some(EngineMode::Native);
        assert!(matches!(
            run_snapshot(p.to_str().unwrap(), &o),
            Err(BrowseError::Usage(_))
        ));
        // With an engine, the default mode uses it.
        let bin = fake_engine(dir.path());
        let mut o = opts(Some(&bin));
        o.screenshot = Some(shot.clone());
        let out = run_snapshot(p.to_str().unwrap(), &o).unwrap();
        assert_eq!(out.engine, "headless:chromium");
        assert_eq!(out.screenshot.as_deref(), Some(shot.as_path()));
        assert_eq!(std::fs::read(&shot).unwrap(), b"png");
    }

    #[test]
    fn a_screenshot_directory_that_does_not_exist_is_refused_before_anything_runs() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_engine(dir.path());
        let mut o = opts(Some(&bin));
        o.screenshot = Some(dir.path().join("nope/s.png"));
        let e = run_snapshot(page(dir.path()).to_str().unwrap(), &o).unwrap_err();
        assert!(matches!(e, BrowseError::Usage(_)), "{e}");
    }

    #[test]
    fn a_bad_engine_variable_is_an_error_in_auto_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut o = opts(Some(&dir.path().join("missing")));
        o.engine = Some(EngineMode::Auto);
        let e = run_snapshot(page(dir.path()).to_str().unwrap(), &o).unwrap_err();
        assert!(matches!(e, BrowseError::EngineBinInvalid(_)), "{e}");
    }

    #[test]
    fn the_engine_is_given_a_file_url_for_a_path_and_the_url_for_http() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("chromium");
        std::fs::write(&bin, "#!/bin/sh\nprintf '<p>%s</p>' \"$*\"\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = dir.path().join("a b.html");
        std::fs::write(&p, "x").unwrap();
        let mut o = opts(Some(&bin));
        o.engine = Some(EngineMode::Cdp);
        let out = run_snapshot(p.to_str().unwrap(), &o).unwrap();
        let canon = std::fs::canonicalize(&p).unwrap();
        assert!(out.snapshot.outline.is_empty());
        // The engine echoes its arguments into the page; read them through the DOM title.
        let dom = engine::dump_dom(
            &EngineInfo {
                path: bin.clone(),
                name: "chromium".into(),
                from_env: true,
            },
            &fetch::file_url(&canon),
            o.viewport,
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(dom.contains("-- file://"), "{dom}");
        assert!(dom.contains("a%20b.html"), "{dom}");
        let out = run_snapshot("http://127.0.0.1:9/x", &o).unwrap();
        assert_eq!(out.engine, "headless:chromium");
    }

    #[test]
    fn doctor_reports_both_tiers() {
        let dir = tempfile::tempdir().unwrap();
        let none = doctor(&HeadlessEngineProbe::default());
        assert_eq!(none.schema_version, 1);
        assert_eq!(none.tiers[0].id, "native-distiller");
        assert_eq!(none.tiers[0].status, "ready");
        assert_eq!(none.tiers[1].id, "headless-binary");
        assert_eq!(none.tiers[1].status, "not-installed");
        assert!(none.ok());

        let bin = dir.path().join("chromium");
        std::fs::write(&bin, "#!/bin/sh\necho 'Fake 1.2.3'\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let found = doctor(&HeadlessEngineProbe {
            engine_bin: Some(bin.clone().into()),
            ..Default::default()
        });
        assert_eq!(found.tiers[1].status, "detected");
        assert_eq!(found.tiers[1].version.as_deref(), Some("Fake 1.2.3"));
        assert_eq!(found.tiers[1].path.as_deref(), Some(bin.to_str().unwrap()));

        let bad = doctor(&HeadlessEngineProbe {
            engine_bin: Some("/nonexistent/x".into()),
            ..Default::default()
        });
        assert_eq!(bad.tiers[1].status, "misconfigured");
        assert!(!bad.ok());
    }

    #[test]
    fn audit_target_reads_a_file_and_audits_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.html");
        std::fs::write(&p, "<button></button>").unwrap();
        let r = audit_target(
            p.to_str().unwrap(),
            DEFAULT_CHECK_VIEWPORT,
            &FetchOptions::default(),
        )
        .unwrap();
        assert_eq!(r.error_count, 2);
        assert!(audit_target("ftp://x", DEFAULT_CHECK_VIEWPORT, &FetchOptions::default()).is_err());
    }
}
