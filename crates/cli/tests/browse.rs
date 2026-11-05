//! `fl browse`, black box: snapshots (handles, metrics, JSON), the `check` gate and its exit
//! codes, `doctor`, and the optional headless-binary tier through a fake executable.
//! Hermetic: a temporary home and working directory, no store, no real browser, and no network
//! beyond 127.0.0.1.

use assert_cmd::Command;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn write(&self, name: &str, body: &str) -> PathBuf {
        let p = self.path(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    /// An executable shell script.
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let p = self.write(name, &format!("#!/bin/sh\n{body}\n"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// `fl` with a private home and working directory, no engine unless a test sets one.
    fn fl(&self) -> Command {
        let mut c = Command::cargo_bin("fl").unwrap();
        c.current_dir(self.dir.path())
            .env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("PATH", self.dir.path())
            .env_remove("FL_DB")
            .env("FL_BROWSE_ENGINE_BIN", "none");
        c
    }
}

const GOOD: &str = r#"<!doctype html><html lang=en><head><title>Shop</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>body{margin:0}.a{color:red}.b{color:blue}</style>
<script>var tracking = "a very long inline script that an agent has no use for";</script></head>
<body><header><a href="/" data-testid="logo">Home</a></header>
<nav aria-label="Primary"><a href="/shop">Shop</a><a href="/cart">Cart</a></nav>
<main><h1>Welcome</h1><form>
<label for="email">Email</label><input id="email" name="email" type="email" autocomplete="email">
<input type="password" name="pw" value="hunter2" aria-label="Password">
<input type="hidden" name="csrf" value="tok">
<button type="submit">Sign in</button>
<button hidden>Never shown</button>
</form></main><footer><p>Copyright</p></footer></body></html>"#;

const BAD: &str = r#"<html><head><title>Bad</title></head><body>
<button></button>
<textarea aria-label="Code" autocomplete="one-time-code"></textarea>
<div style="min-width: 400px">wide</div>
</body></html>"#;

const WARN_ONLY: &str = r#"<html><head>
<meta name="viewport" content="width=device-width"></head><body>
<div style="min-width:400px">wide</div></body></html>"#;

fn json_out(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A local HTTP server answering `routes` until dropped.
struct Server {
    url: String,
    _stop: mpsc::Sender<()>,
}

fn serve(routes: Vec<(&'static str, &'static str, Vec<u8>)>) -> Server {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr().to_ip().unwrap());
    let (stop, rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        while rx.try_recv().is_err() {
            let Ok(Some(req)) = server.recv_timeout(Duration::from_millis(50)) else {
                continue;
            };
            let url = req.url().to_string();
            match routes.iter().find(|(p, ..)| *p == url) {
                Some((_, ty, body)) => {
                    let r = tiny_http::Response::from_data(body.clone()).with_header(
                        tiny_http::Header::from_bytes("Content-Type", ty.as_bytes()).unwrap(),
                    );
                    let _ = req.respond(r);
                }
                None => {
                    let _ = req.respond(tiny_http::Response::empty(404));
                }
            }
        }
    });
    Server { url, _stop: stop }
}

// ---- snapshot ----------------------------------------------------------------------------

#[test]
fn snapshot_prints_handles_in_document_order_and_the_metrics() {
    let env = Env::new();
    let page = env.write("good.html", GOOD);
    let out = env
        .fl()
        .args(["browse", "snapshot"])
        .arg(&page)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("engine: native"), "{text}");
    assert!(text.contains("title: Shop"), "{text}");
    assert!(
        text.contains("viewport-meta: width=device-width, initial-scale=1"),
        "{text}"
    );
    assert!(text.contains("navigation \"Primary\""), "{text}");
    assert!(text.contains("heading[1] \"Welcome\""), "{text}");
    let rows: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with('@') && l.contains('\t'))
        .collect();
    let handles: Vec<&str> = rows.iter().map(|r| r.split('\t').next().unwrap()).collect();
    assert_eq!(handles, ["@e1", "@e2", "@e3", "@e4", "@e5", "@e6"]);
    let name_of = |h: &str| {
        rows.iter()
            .find(|r| r.starts_with(&format!("{h}\t")))
            .map(|r| r.split('\t').nth(3).unwrap())
            .unwrap()
    };
    assert_eq!(name_of("@e1"), "Home");
    assert_eq!(name_of("@e2"), "Shop");
    assert_eq!(name_of("@e3"), "Cart");
    assert_eq!(name_of("@e4"), "Email");
    assert_eq!(name_of("@e5"), "Password");
    assert_eq!(name_of("@e6"), "Sign in");
    // The hidden input and the hidden button have no handle; the password value is not shown.
    assert!(
        !text.contains("Never shown") && !text.contains("csrf"),
        "{text}"
    );
    assert!(!text.contains("hunter2"), "{text}");
    for key in ["raw_bytes", "distilled_bytes", "token_savings_pct"] {
        assert!(text.contains(&format!("\n{key}\t")), "{key}: {text}");
    }
}

#[test]
fn snapshot_json_has_the_documented_shape_and_honest_metrics() {
    let env = Env::new();
    let page = env.write("good.html", GOOD);
    let out = env
        .fl()
        .args(["browse", "snapshot", "--json"])
        .arg(&page)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let v = json_out(&out);
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["engine"], "native");
    assert_eq!(
        v["viewport"],
        serde_json::json!({"width": 1280, "height": 800})
    );
    assert_eq!(v["screenshot"], Value::Null);
    assert_eq!(v["title"], "Shop");
    assert_eq!(v["truncated"], false);
    let els = v["elements"].as_array().unwrap();
    assert_eq!(els.len(), 6);
    for (i, e) in els.iter().enumerate() {
        assert_eq!(e["handle"], format!("@e{}", i + 1));
    }
    assert_eq!(els[0]["test_id"], "logo");
    assert_eq!(els[0]["href"], "/");
    assert_eq!(els[3]["autocomplete"], "email");
    assert_eq!(els[3]["attrs"]["type"], "email");
    assert_eq!(els[4]["attrs"].get("value"), None, "no password value");
    assert_eq!(v["landmarks"][1]["role"], "navigation");
    assert_eq!(v["headings"][0]["text"], "Welcome");

    let m = &v["metrics"];
    let raw = m["raw_bytes"].as_u64().unwrap();
    let distilled = m["distilled_bytes"].as_u64().unwrap();
    assert_eq!(raw, GOOD.len() as u64);
    assert!(distilled > 0 && distilled < raw);
    let want = ((1.0 - distilled as f64 / raw as f64) * 1000.0).round() / 10.0;
    assert_eq!(m["token_savings_pct"].as_f64().unwrap(), want);
    // The distilled size is the size of the printed outline and table.
    let text = env
        .fl()
        .args(["browse", "snapshot"])
        .arg(&page)
        .output()
        .unwrap();
    let text = stdout(&text);
    let body_start = text.find("title: ").unwrap();
    let body_end = text.find("\n# metrics").unwrap();
    assert_eq!(distilled as usize, text[body_start..body_end].len());
}

#[test]
fn snapshot_output_is_byte_identical_across_runs() {
    let env = Env::new();
    let page = env.write("good.html", GOOD);
    let run = |extra: &[&str]| {
        env.fl()
            .args(["browse", "snapshot"])
            .args(extra)
            .arg(&page)
            .output()
            .unwrap()
            .stdout
    };
    assert_eq!(run(&[]), run(&[]));
    assert_eq!(run(&["--json"]), run(&["--json"]));
}

#[test]
fn snapshot_reads_file_urls_and_http_targets_alike() {
    let env = Env::new();
    let page = env.write("good.html", GOOD);
    let server = serve(vec![(
        "/shop",
        "text/html; charset=utf-8",
        GOOD.as_bytes().to_vec(),
    )]);
    let from_file = env
        .fl()
        .args(["browse", "snapshot", "--json"])
        .arg(&page)
        .output()
        .unwrap();
    let from_url = env
        .fl()
        .args(["browse", "snapshot", "--json"])
        .arg(format!("file://{}", page.display()))
        .output()
        .unwrap();
    let from_http = env
        .fl()
        .args(["browse", "snapshot", "--json"])
        .arg(format!("{}/shop", server.url))
        .output()
        .unwrap();
    assert!(from_file.status.success() && from_url.status.success() && from_http.status.success());
    let (a, b, c) = (
        json_out(&from_file),
        json_out(&from_url),
        json_out(&from_http),
    );
    for key in ["elements", "outline", "metrics", "headings", "landmarks"] {
        assert_eq!(a[key], b[key], "{key}");
        assert_eq!(a[key], c[key], "{key}");
    }
}

#[test]
fn snapshot_errors_are_clear_and_exit_2() {
    let env = Env::new();
    let server = serve(vec![
        ("/data.json", "application/json", b"{}".to_vec()),
        ("/big", "text/html", vec![b'x'; 5 * 1024 * 1024]),
    ]);
    let fail = |target: &str| {
        let out = env
            .fl()
            .args(["browse", "snapshot", target])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{target}: {}", stderr(&out));
        assert!(stdout(&out).is_empty(), "{target}");
        stderr(&out)
    };
    assert!(fail(&format!("{}/data.json", server.url)).contains("not HTML"));
    assert!(fail(&format!("{}/big", server.url)).contains("larger than"));
    assert!(fail(&format!("{}/missing", server.url)).contains("HTTP 404"));
    assert!(fail("ftp://example.com/x").contains("unsupported scheme `ftp`"));
    assert!(fail("javascript:alert(1)").contains("unsupported scheme"));
    assert!(fail("http://user:secret@127.0.0.1/").contains("user name or password"));
    assert!(fail(env.path("nope.html").to_str().unwrap()).contains("could not read"));
    let bin = env.dir.path().join("b.html");
    std::fs::write(&bin, [0u8, 1, 2, 3]).unwrap();
    assert!(fail(bin.to_str().unwrap()).contains("not HTML"));
}

#[test]
fn snapshot_refuses_a_bad_viewport_and_db_and_ignores_a_broken_config() {
    let env = Env::new();
    let page = env.write("good.html", GOOD);
    let out = env
        .fl()
        .args(["browse", "snapshot", "--viewport", "wide"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("WIDTHxHEIGHT"), "{}", stderr(&out));

    let out = env
        .fl()
        .args(["--db", "x.fls", "browse", "snapshot"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("opens no store"), "{}", stderr(&out));

    // A config that cannot be parsed does not stop it, and it writes nothing.
    std::fs::create_dir_all(env.path("config/fl")).unwrap();
    std::fs::write(env.path("config/fl/config.toml"), "this is = = not toml").unwrap();
    let before: Vec<_> = std::fs::read_dir(env.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let out = env
        .fl()
        .args(["browse", "snapshot"])
        .arg(&page)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let mut after: Vec<_> = std::fs::read_dir(env.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let mut before = before;
    before.sort();
    after.sort();
    assert_eq!(before, after, "nothing was created");
}

// ---- check -------------------------------------------------------------------------------

#[test]
fn check_passes_a_clean_page_with_exit_0() {
    let env = Env::new();
    let page = env.write("good.html", GOOD);
    let out = env
        .fl()
        .args(["browse", "check"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("pass: 0 error(s), 0 warning(s) at 320x568"),
        "{}",
        stdout(&out)
    );
    let strict = env
        .fl()
        .args(["browse", "check", "--strict"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(strict.status.code(), Some(0));
}

#[test]
fn check_fails_a_bad_page_with_exit_1_and_names_each_finding() {
    let env = Env::new();
    let page = env.write("bad.html", BAD);
    let out = env
        .fl()
        .args(["browse", "check"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    let rows: Vec<Vec<&str>> = text
        .lines()
        .skip(1)
        .filter(|l| l.contains('\t'))
        .map(|l| l.split('\t').collect())
        .collect();
    let codes: Vec<(&str, &str, &str)> = rows.iter().map(|r| (r[0], r[1], r[2])).collect();
    assert_eq!(
        codes,
        vec![
            ("error", "missing-viewport-meta", "-"),
            ("error", "unlabeled-control", "@e1"),
            ("error", "ios-contact-autofill-hazard", "@e2"),
            ("warning", "fixed-min-width-overflow-hazard", "-"),
        ]
    );
    assert!(
        text.contains("FAIL: 3 error(s), 1 warning(s) at 320x568"),
        "{text}"
    );
}

#[test]
fn check_json_has_the_documented_shape() {
    let env = Env::new();
    let page = env.write("bad.html", BAD);
    let out = env
        .fl()
        .args(["browse", "check", "--json"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v = json_out(&out);
    assert_eq!(v["schema_version"], 1);
    assert_eq!(
        v["viewport"],
        serde_json::json!({"width": 320, "height": 568})
    );
    assert_eq!(v["strict"], false);
    assert_eq!(v["passed"], false);
    assert_eq!(v["error_count"], 3);
    assert_eq!(v["warning_count"], 1);
    let f = v["findings"].as_array().unwrap();
    assert_eq!(f.len(), 4);
    assert_eq!(f[1]["severity"], "error");
    assert_eq!(f[1]["code"], "unlabeled-control");
    assert_eq!(f[1]["handle"], "@e1");
    assert!(f[1]["message"].as_str().unwrap().contains("<button>"));
    assert_eq!(f[0]["handle"], Value::Null);
    assert_eq!(f[3]["severity"], "warning");
    for finding in f {
        let mut keys: Vec<_> = finding.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["code", "handle", "message", "severity"]);
    }
}

#[test]
fn check_warnings_pass_unless_strict() {
    let env = Env::new();
    let page = env.write("warn.html", WARN_ONLY);
    let lax = env
        .fl()
        .args(["browse", "check", "--json"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(lax.status.code(), Some(0), "{}", stderr(&lax));
    let v = json_out(&lax);
    assert_eq!(
        (v["error_count"].as_u64(), v["warning_count"].as_u64()),
        (Some(0), Some(1))
    );
    assert_eq!(v["passed"], true);

    let strict = env
        .fl()
        .args(["browse", "check", "--strict", "--json"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(strict.status.code(), Some(1));
    let v = json_out(&strict);
    assert_eq!(v["strict"], true);
    assert_eq!(v["passed"], false);
}

#[test]
fn check_honours_the_viewport_and_validates_it_strictly() {
    let env = Env::new();
    let page = env.write("warn.html", WARN_ONLY);
    let wide = env
        .fl()
        .args(["browse", "check", "--strict", "--viewport", "1280x800"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(
        wide.status.code(),
        Some(0),
        "{}{}",
        stdout(&wide),
        stderr(&wide)
    );
    assert!(stdout(&wide).contains("at 1280x800"));

    for bad in [
        "320",
        "320X568",
        "0x10",
        "-1x5",
        "320x568x1",
        "",
        "1e3x5",
        " 320x568",
    ] {
        let out = env
            .fl()
            .args(["browse", "check", &format!("--viewport={bad}")])
            .arg(&page)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{bad:?}");
        assert!(
            stderr(&out).contains("WIDTHxHEIGHT"),
            "{bad:?}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn check_works_over_http_and_fails_closed_on_a_bad_target() {
    let env = Env::new();
    let server = serve(vec![
        ("/good", "text/html", GOOD.as_bytes().to_vec()),
        ("/bad", "text/html", BAD.as_bytes().to_vec()),
    ]);
    let good = env
        .fl()
        .args(["browse", "check"])
        .arg(format!("{}/good", server.url))
        .output()
        .unwrap();
    assert_eq!(good.status.code(), Some(0));
    let bad = env
        .fl()
        .args(["browse", "check"])
        .arg(format!("{}/bad", server.url))
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(1));
    // A target that cannot be read is not a pass: exit 2, not 0.
    let gone = env
        .fl()
        .args(["browse", "check"])
        .arg(format!("{}/gone", server.url))
        .output()
        .unwrap();
    assert_eq!(gone.status.code(), Some(2));
    assert!(stderr(&gone).contains("HTTP 404"));
}

#[test]
fn check_never_starts_an_engine() {
    let env = Env::new();
    let marker = env.path("engine-ran");
    let engine = env.script(
        "chromium",
        &format!(": > {}; echo '<p>x</p>'", marker.display()),
    );
    let page = env.write("good.html", GOOD);
    let out = env
        .fl()
        .env("FL_BROWSE_ENGINE_BIN", &engine)
        .args(["browse", "check"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(!marker.exists());
}

// ---- engine tier -------------------------------------------------------------------------

/// A fake browser: prints a DOM that echoes the arguments it was given, and writes the
/// screenshot when asked.
fn fake_engine(env: &Env, marker: &Path) -> PathBuf {
    env.script(
        "fake-chromium",
        &format!(
            r#": > '{}'
case "$1" in --version) echo 'FakeChromium 9.9'; exit 0;; esac
for a in "$@"; do case "$a" in --screenshot=*) printf PNGDATA > "${{a#--screenshot=}}";; esac; done
printf '<html><head><title>Rendered</title></head><body><main><button data-testid=args>%s</button><a href=/x>Link</a></main></body></html>' "$*""#,
            marker.display()
        ),
    )
}

#[test]
fn native_is_the_default_and_never_spawns_the_engine() {
    let env = Env::new();
    let marker = env.path("ran");
    let engine = fake_engine(&env, &marker);
    let page = env.write("good.html", GOOD);
    for extra in [&[][..], &["--engine", "native"][..]] {
        let out = env
            .fl()
            .env("FL_BROWSE_ENGINE_BIN", &engine)
            .args(["browse", "snapshot"])
            .args(extra)
            .arg(&page)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(stdout(&out).contains("engine: native"));
        assert!(!marker.exists(), "the engine was started with {extra:?}");
    }
}

#[test]
fn cdp_and_auto_distill_the_dom_the_engine_prints() {
    let env = Env::new();
    let marker = env.path("ran");
    let engine = fake_engine(&env, &marker);
    let page = env.write("good.html", GOOD);
    for mode in ["cdp", "auto"] {
        let out = env
            .fl()
            .env("FL_BROWSE_ENGINE_BIN", &engine)
            .args([
                "browse",
                "snapshot",
                "--json",
                "--viewport",
                "390x844",
                "--engine",
                mode,
            ])
            .arg(&page)
            .output()
            .unwrap();
        assert!(out.status.success(), "{mode}: {}", stderr(&out));
        let v = json_out(&out);
        assert_eq!(v["engine"], "headless:fake-chromium");
        assert_eq!(v["title"], "Rendered");
        assert_eq!(
            v["viewport"],
            serde_json::json!({"width": 390, "height": 844})
        );
        let els = v["elements"].as_array().unwrap();
        assert_eq!(els[0]["handle"], "@e1");
        assert_eq!(els[1]["name"], "Link");
        // The engine was given separate arguments, the window size, and the target after `--`.
        let args = els[0]["name"].as_str().unwrap();
        let canon = std::fs::canonicalize(&page).unwrap();
        assert!(
            args.starts_with("--headless --dump-dom --window-size=390,844 "),
            "{args}"
        );
        assert!(
            args.ends_with(&format!("-- file://{}", canon.display())),
            "{args}"
        );
        assert!(!args.contains("--screenshot"));
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();
    }
}

#[test]
fn a_screenshot_is_written_through_the_engine() {
    let env = Env::new();
    let engine = fake_engine(&env, &env.path("ran"));
    let page = env.write("good.html", GOOD);
    let shot = env.path("shot.png");
    let out = env
        .fl()
        .env("FL_BROWSE_ENGINE_BIN", &engine)
        .args(["browse", "snapshot", "--json", "--screenshot", "shot.png"])
        .arg(&page)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let v = json_out(&out);
    assert_eq!(v["engine"], "headless:fake-chromium");
    assert_eq!(v["screenshot"], shot.to_str().unwrap());
    assert_eq!(std::fs::read(&shot).unwrap(), b"PNGDATA");
    let args = v["elements"][0]["name"].as_str().unwrap();
    assert!(
        args.contains(&format!("--screenshot={}", shot.display())),
        "{args}"
    );
}

#[test]
fn a_screenshot_without_an_engine_fails_with_an_actionable_error() {
    let env = Env::new();
    let page = env.write("good.html", GOOD);
    let shot = env.path("shot.png");
    // `none` switches detection off, so this holds on a machine with a browser installed.
    let out = env
        .fl()
        .args(["browse", "snapshot", "--screenshot"])
        .arg(&shot)
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stdout(&out).is_empty());
    let err = stderr(&out);
    assert!(
        err.contains("--screenshot needs a headless browser"),
        "{err}"
    );
    assert!(
        err.contains("FL_BROWSE_ENGINE_BIN") && err.contains("chromium"),
        "{err}"
    );
    assert!(!shot.exists());

    // Spelling out `--engine native` with a screenshot is a contradiction, not a skip.
    let out = env
        .fl()
        .args(["browse", "snapshot", "--engine", "native", "--screenshot"])
        .arg(&shot)
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("never starts one"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn cdp_without_an_engine_fails_and_auto_falls_back_to_native() {
    let env = Env::new();
    let page = env.write("good.html", GOOD);
    let cdp = env
        .fl()
        .args(["browse", "snapshot", "--engine", "cdp"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(cdp.status.code(), Some(2));
    assert!(
        stderr(&cdp).contains("none is installed"),
        "{}",
        stderr(&cdp)
    );
    let auto = env
        .fl()
        .args(["browse", "snapshot", "--engine", "auto"])
        .arg(&page)
        .output()
        .unwrap();
    assert!(auto.status.success());
    assert!(stdout(&auto).contains("engine: native"));
}

#[test]
fn a_failing_engine_is_an_error_not_an_empty_snapshot() {
    let env = Env::new();
    let engine = env.script("chromium", "echo 'cannot open display' >&2; exit 9");
    let page = env.write("good.html", GOOD);
    let out = env
        .fl()
        .env("FL_BROWSE_ENGINE_BIN", &engine)
        .args(["browse", "snapshot", "--engine", "cdp"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("cannot open display"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn an_unusable_engine_variable_is_an_error_when_an_engine_is_wanted() {
    let env = Env::new();
    let page = env.write("good.html", GOOD);
    let out = env
        .fl()
        .env("FL_BROWSE_ENGINE_BIN", env.path("missing"))
        .args(["browse", "snapshot", "--engine", "auto"])
        .arg(&page)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("not an executable file"),
        "{}",
        stderr(&out)
    );
}

// ---- doctor ------------------------------------------------------------------------------

#[test]
fn doctor_reports_native_ready_and_the_engine_missing() {
    let env = Env::new();
    let out = env.fl().args(["browse", "doctor"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("native-distiller\tready"), "{text}");
    assert!(text.contains("headless-binary\tnot-installed"), "{text}");

    let out = env
        .fl()
        .args(["browse", "doctor", "--json"])
        .output()
        .unwrap();
    let v = json_out(&out);
    assert_eq!(v["schema_version"], 1);
    let tiers = v["tiers"].as_array().unwrap();
    assert_eq!(tiers[0]["id"], "native-distiller");
    assert_eq!(tiers[0]["tier"], 1);
    assert_eq!(tiers[0]["status"], "ready");
    assert_eq!(tiers[1]["id"], "headless-binary");
    assert_eq!(tiers[1]["tier"], 2);
    assert_eq!(tiers[1]["status"], "not-installed");
    assert_eq!(tiers[1]["path"], Value::Null);
}

#[test]
fn doctor_detects_the_engine_with_its_path_and_version() {
    let env = Env::new();
    let engine = fake_engine(&env, &env.path("ran"));
    let out = env
        .fl()
        .env("FL_BROWSE_ENGINE_BIN", &engine)
        .args(["browse", "doctor", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let t = &json_out(&out)["tiers"][1];
    assert_eq!(t["status"], "detected");
    assert_eq!(t["path"], engine.to_str().unwrap());
    assert_eq!(t["version"], "FakeChromium 9.9");

    let human = env
        .fl()
        .env("FL_BROWSE_ENGINE_BIN", &engine)
        .args(["browse", "doctor"])
        .output()
        .unwrap();
    assert!(
        stdout(&human).contains("FakeChromium 9.9"),
        "{}",
        stdout(&human)
    );
}

#[test]
fn doctor_finds_an_engine_on_path_by_name() {
    let env = Env::new();
    let bin_dir = env.path("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let p = bin_dir.join("chromium-headless-shell");
    std::fs::write(&p, "#!/bin/sh\necho 'Headless Shell 1'\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = env
        .fl()
        .env_remove("FL_BROWSE_ENGINE_BIN")
        .env("PATH", &bin_dir)
        .args(["browse", "doctor", "--json"])
        .output()
        .unwrap();
    let t = &json_out(&out)["tiers"][1];
    // A real browser in a standard directory may rank earlier; a detected engine is the claim.
    assert_eq!(t["status"], "detected", "{t}");
}

#[test]
fn doctor_fails_only_for_a_misconfigured_engine_variable() {
    let env = Env::new();
    let out = env
        .fl()
        .env("FL_BROWSE_ENGINE_BIN", env.path("nope"))
        .args(["browse", "doctor", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json_out(&out)["tiers"][1]["status"], "misconfigured");
}

#[test]
fn help_documents_the_checks_and_severities() {
    let env = Env::new();
    let out = env
        .fl()
        .args(["browse", "check", "--help"])
        .output()
        .unwrap();
    let text = stdout(&out);
    for line in [
        "missing-viewport-meta (error)",
        "unlabeled-control (error)",
        "ios-contact-autofill-hazard (error)",
        "fixed-min-width-overflow-hazard (warning)",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
}
