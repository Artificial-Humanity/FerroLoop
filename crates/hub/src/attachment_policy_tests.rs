//! The chat-attachment rules (see [`crate::attachment_policy`]): what `POST
//! /api/v1/sessions/:id/prompt` accepts, how `GET /api/v1/attachments/:filename` serves it, and
//! how static UI files are confined. Every hub here lives under a temporary directory.

use std::fs;
use std::path::PathBuf;

use base64::Engine;
use fl_core::pr_policy::ReviewerApps;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::attachment_policy::{
    BLOCKED_UPLOAD_EXTENSIONS, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS_PER_PROMPT,
    MAX_ATTACHMENTS_TOTAL_BYTES, MAX_STATIC_FILE_BYTES, sanitize_upload_name,
};
use crate::{HttpRequest, HttpResponse, HubServerConfig, HubState, handle_http_request};

const OWNER: &str = "owner-tok-secret";
const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4];

struct Env {
    _tmp: TempDir,
    base: PathBuf,
    state: HubState,
}

impl Env {
    fn attachments(&self) -> PathBuf {
        self.base.join("data/attachments")
    }

    fn ui(&self) -> PathBuf {
        self.base.join("ui")
    }

    /// How many entries the attachments directory holds (0 when it does not exist).
    fn stored_count(&self) -> usize {
        fs::read_dir(self.attachments()).map_or(0, Iterator::count)
    }

    /// Put a file straight into the attachments directory, as an older hub or a person might.
    fn plant(&self, name: &str, bytes: &[u8]) {
        fs::create_dir_all(self.attachments()).unwrap();
        fs::write(self.attachments().join(name), bytes).unwrap();
    }
}

fn env() -> Env {
    let tmp = TempDir::new().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    for dir in ["data", "repo", "ui"] {
        fs::create_dir_all(base.join(dir)).unwrap();
    }
    fs::write(base.join("ui/index.html"), "<html>spa</html>").unwrap();
    let state = HubState::new(HubServerConfig {
        listen: "127.0.0.1:0".into(),
        store_path: base.join("data/store.redb"),
        repo_root: base.join("repo"),
        ui_dir: Some(base.join("ui")),
        owner_login: "lmcfarlin".into(),
        reviewer_apps: ReviewerApps::default(),
        owner_token: Some(OWNER.into()),
        peer_token: Some("peer-tok-secret".into()),
        trust_tailscale_headers: false,
        enable_terminal: false,
        session_ttl_secs: 3600,
        home_override: Some(base.join("data")),
        github: crate::github_pr::HubGithub::default(),
    });
    Env {
        _tmp: tmp,
        base,
        state,
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn attachment(name: &str, media_type: &str, bytes: &[u8]) -> Value {
    json!({ "name": name, "media_type": media_type, "data_base64": b64(bytes) })
}

fn post_prompt(env: &Env, attachments: Vec<Value>) -> HttpResponse {
    handle_http_request(
        &env.state,
        &HttpRequest::new("POST", "/api/v1/sessions/ferris/prompt")
            .with_header("Authorization", format!("Bearer {OWNER}"))
            .with_json_body(&json!({ "prompt": "look", "attachments": attachments })),
    )
}

fn get(env: &Env, path: &str) -> HttpResponse {
    handle_http_request(
        &env.state,
        &HttpRequest::new("GET", path).with_header("Authorization", format!("Bearer {OWNER}")),
    )
}

fn header<'a>(resp: &'a HttpResponse, name: &str) -> Option<&'a str> {
    resp.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Assert the headers every served attachment carries, whatever its type.
fn assert_locked_down(resp: &HttpResponse) {
    assert_eq!(header(resp, "X-Content-Type-Options"), Some("nosniff"));
    assert_eq!(
        header(resp, "Content-Security-Policy"),
        Some("default-src 'none'; sandbox")
    );
}

#[test]
fn active_and_unknown_content_is_served_as_a_download_with_nosniff_and_csp() {
    let env = env();
    let html = b"<script>fetch('/api/v1/gate/secrets')</script>";
    for name in [
        "1-0-pwn.html",
        "1-0-pwn.HTM",
        "1-0-pwn.xhtml",
        "1-0-logo.svg",
        "1-0-run.js",
        "1-0-run.mjs",
        "1-0-feed.xml",
        "1-0-style.xsl",
        "1-0-noextension",
        "1-0-weird.zzz",
        "1-0-doc.pdf",
        // An extension of an inert type does not vouch for content that is not that type.
        "1-0-fake.png",
    ] {
        env.plant(name, html);
        let resp = get(&env, &format!("/api/v1/attachments/{name}"));
        assert_eq!(resp.status, 200, "{name}");
        assert_eq!(resp.content_type, "application/octet-stream", "{name}");
        let disposition = header(&resp, "Content-Disposition").unwrap();
        assert!(
            disposition.starts_with("attachment;"),
            "{name}: {disposition}"
        );
        assert_locked_down(&resp);
        assert_eq!(resp.body, html);
    }
}

#[test]
fn inert_types_stay_inline_with_their_own_type_and_the_same_lock_down() {
    let env = env();
    let up = post_prompt(&env, vec![attachment("shot.png", "image/png", PNG)]);
    assert_eq!(up.status, 200);
    let body = up.json_value().unwrap();
    let url = body["attachments"][0]["url"].as_str().unwrap().to_string();
    let resp = get(&env, &url);
    assert_eq!(resp.status, 200);
    assert_eq!(resp.content_type, "image/png");
    assert!(
        header(&resp, "Content-Disposition")
            .unwrap()
            .starts_with("inline;")
    );
    assert_locked_down(&resp);
    assert_eq!(resp.body, PNG);

    for (name, bytes, media_type) in [
        ("1-0-a.jpg", &[0xFF, 0xD8, 0xFF, 0xE0][..], "image/jpeg"),
        ("1-0-a.gif", b"GIF89a....", "image/gif"),
        ("1-0-a.webp", b"RIFF\0\0\0\0WEBPVP8 ", "image/webp"),
        ("1-0-a.txt", b"<b>plain</b>", "text/plain; charset=utf-8"),
        ("1-0-a.json", b"{}", "application/json"),
    ] {
        env.plant(name, bytes);
        let resp = get(&env, &format!("/api/v1/attachments/{name}"));
        assert_eq!(resp.content_type, media_type, "{name}");
        assert_locked_down(&resp);
    }
}

#[test]
fn a_hostile_file_name_cannot_inject_response_headers() {
    let env = env();
    let hostile = "1-0-a\"b\r\nX-Evil: 1\\.txt";
    env.plant(hostile, b"hi");
    let url = "/api/v1/attachments/1-0-a%22b%0D%0AX-Evil%3A%201%5C.txt";
    let resp = get(&env, url);
    // A backslash in the requested name is refused outright; drop it to reach the file.
    assert_eq!(resp.status, 400);
    env.plant("1-0-a\"b\r\nX-Evil: 1.txt", b"hi");
    let resp = get(&env, "/api/v1/attachments/1-0-a%22b%0D%0AX-Evil%3A%201.txt");
    assert_eq!(resp.status, 200);
    let wire = String::from_utf8_lossy(&resp.to_http_bytes(false)).to_string();
    let head = wire.split("\r\n\r\n").next().unwrap();
    assert!(
        !head
            .lines()
            .any(|l| l.to_ascii_lowercase().starts_with("x-evil")),
        "{head}"
    );
    assert_eq!(
        header(&resp, "Content-Disposition"),
        Some("inline; filename=\"1-0-a_b__X-Evil__1.txt\"")
    );
}

#[test]
fn a_symlink_out_of_the_attachments_directory_is_refused() {
    let env = env();
    let outside = env.base.join("outside-secret.txt");
    fs::write(&outside, "TOP SECRET").unwrap();
    fs::create_dir_all(env.attachments()).unwrap();
    std::os::unix::fs::symlink(&outside, env.attachments().join("1-0-link.txt")).unwrap();
    let resp = get(&env, "/api/v1/attachments/1-0-link.txt");
    assert_eq!(resp.status, 403);
    assert!(!String::from_utf8_lossy(&resp.body).contains("SECRET"));

    // A symlinked attachments directory itself is still followed (documented behaviour).
    let real = env.base.join("real-attachments");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("1-0-ok.txt"), "fine").unwrap();
    fs::remove_dir_all(env.attachments()).unwrap();
    std::os::unix::fs::symlink(&real, env.attachments()).unwrap();
    let resp = get(&env, "/api/v1/attachments/1-0-ok.txt");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"fine");
}

#[test]
fn an_oversize_attachment_file_is_not_served() {
    let env = env();
    fs::create_dir_all(env.attachments()).unwrap();
    let f = fs::File::create(env.attachments().join("1-0-big.txt")).unwrap();
    f.set_len(MAX_ATTACHMENT_BYTES as u64 + 1).unwrap();
    assert_eq!(get(&env, "/api/v1/attachments/1-0-big.txt").status, 413);
}

#[test]
fn one_file_over_the_per_file_cap_is_413_and_writes_nothing() {
    let env = env();
    let resp = post_prompt(
        &env,
        vec![
            attachment("small.png", "image/png", PNG),
            attachment(
                "big.bin",
                "application/octet-stream",
                &vec![0u8; MAX_ATTACHMENT_BYTES + 1],
            ),
        ],
    );
    assert_eq!(resp.status, 413);
    let body = resp.json_value().unwrap();
    assert_eq!(body["limit_bytes"], MAX_ATTACHMENT_BYTES);
    assert!(body["error"].as_str().unwrap().contains("big.bin"));
    assert_eq!(env.stored_count(), 0);

    // Exactly the cap is accepted.
    let ok = post_prompt(
        &env,
        vec![attachment(
            "exact.bin",
            "",
            &vec![0u8; MAX_ATTACHMENT_BYTES],
        )],
    );
    assert_eq!(ok.status, 200);
}

#[test]
fn files_that_are_each_under_the_cap_but_over_the_aggregate_are_413_and_write_nothing() {
    let env = env();
    let each = MAX_ATTACHMENTS_TOTAL_BYTES / 2 + 1;
    assert!(each <= MAX_ATTACHMENT_BYTES);
    let resp = post_prompt(
        &env,
        vec![
            attachment("a.bin", "", &vec![1u8; each]),
            attachment("b.bin", "", &vec![2u8; each]),
        ],
    );
    assert_eq!(resp.status, 413);
    assert_eq!(
        resp.json_value().unwrap()["limit_bytes"],
        MAX_ATTACHMENTS_TOTAL_BYTES
    );
    assert_eq!(env.stored_count(), 0);
}

#[test]
fn too_many_attachments_is_413_and_writes_nothing() {
    let env = env();
    let many = |n: usize| {
        (0..n)
            .map(|i| attachment(&format!("f{i}.txt"), "", b"x"))
            .collect()
    };
    let resp = post_prompt(&env, many(MAX_ATTACHMENTS_PER_PROMPT + 1));
    assert_eq!(resp.status, 413);
    assert_eq!(
        resp.json_value().unwrap()["limit_count"],
        MAX_ATTACHMENTS_PER_PROMPT
    );
    assert_eq!(env.stored_count(), 0);

    let ok = post_prompt(&env, many(MAX_ATTACHMENTS_PER_PROMPT));
    assert_eq!(ok.status, 200);
    assert_eq!(env.stored_count(), MAX_ATTACHMENTS_PER_PROMPT);
}

#[test]
fn a_long_name_cannot_hide_a_blocked_extension_in_the_cut_off_tail() {
    let env = env();
    // The raw extension is `htmlBBBB...`; the saved name is cut to 120 characters and ends `.html`.
    let sneaky = format!("{}.html{}", "A".repeat(115), "B".repeat(30));
    assert!(sanitize_upload_name(&sneaky).ends_with(".html"));
    let resp = post_prompt(&env, vec![attachment(&sneaky, "text/plain", b"hi")]);
    assert_eq!(resp.status, 415);
    assert!(
        resp.json_value().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains(&sneaky),
        "the 415 names the raw name"
    );
    assert_eq!(env.stored_count(), 0);

    // An ordinary long .png is still accepted.
    let long_png = format!("{}.png", "n".repeat(200));
    let ok = post_prompt(&env, vec![attachment(&long_png, "image/png", PNG)]);
    assert_eq!(ok.status, 200, "{:?}", ok.json_value());
    assert_eq!(env.stored_count(), 1);
}

#[test]
fn a_blocked_extension_is_415_names_the_file_and_writes_nothing() {
    let env = env();
    for bad in [
        "evil.html",
        "EVIL.HTML",
        "x.svg",
        "x.js",
        "x.xml",
        "x.exe",
        "x.sh",
        "x.html.",
        "x.html ",
    ] {
        let resp = post_prompt(
            &env,
            vec![
                attachment("fine.png", "image/png", PNG),
                attachment(bad, "text/plain", b"hi"),
            ],
        );
        assert_eq!(resp.status, 415, "{bad}");
        assert!(
            resp.json_value().unwrap()["error"]
                .as_str()
                .unwrap()
                .contains(bad),
            "{bad}"
        );
        assert_eq!(env.stored_count(), 0, "{bad}");
    }
    // The whole list is blocked, and an inert double extension is not.
    for ext in BLOCKED_UPLOAD_EXTENSIONS {
        assert!(crate::attachment_policy::blocked_upload_extension(&format!("a.{ext}")).is_some());
    }
    assert!(crate::attachment_policy::blocked_upload_extension("page.html.png").is_none());
    assert!(crate::attachment_policy::blocked_upload_extension("html").is_none());
}

#[test]
fn the_stored_media_type_comes_from_the_file_not_the_client() {
    let env = env();
    let resp = post_prompt(
        &env,
        vec![
            attachment("a.png", "text/html", PNG),
            attachment("b.bin", "<script>alert(1)</script>", b"data"),
            attachment("c.png", "image/png", b"<html>not a png</html>"),
            attachment("d.txt", "application/javascript", b"hello"),
            attachment("e", "image/svg+xml", b"<svg/>"),
            attachment("f.pdf", "", b"%PDF-1.4"),
        ],
    );
    assert_eq!(resp.status, 200);
    let body = resp.json_value().unwrap();
    let types: Vec<&str> = body["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["media_type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "image/png",
            "application/octet-stream",
            "application/octet-stream",
            "text/plain; charset=utf-8",
            "application/octet-stream",
            "application/pdf",
        ]
    );
}

#[test]
fn static_files_over_the_cap_or_behind_an_escaping_symlink_are_refused() {
    let env = env();
    fs::write(env.ui().join("app.js"), "console.log(1)").unwrap();
    let resp = get(&env, "/app.js");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.content_type, "application/javascript; charset=utf-8");

    let f = fs::File::create(env.ui().join("huge.js")).unwrap();
    f.set_len(MAX_STATIC_FILE_BYTES + 1).unwrap();
    assert_eq!(get(&env, "/huge.js").status, 413);

    let outside = env.base.join("outside.js");
    fs::write(&outside, "SECRET").unwrap();
    std::os::unix::fs::symlink(&outside, env.ui().join("leak.js")).unwrap();
    let resp = get(&env, "/leak.js");
    assert_eq!(resp.status, 403);
    assert!(!String::from_utf8_lossy(&resp.body).contains("SECRET"));

    // A missing path still falls back to the SPA shell.
    let resp = get(&env, "/some/route");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"<html>spa</html>");
}

#[test]
fn a_name_ending_in_a_direction_mark_or_nul_is_stored_harmless_and_served_as_a_download() {
    let env = env();
    let html = b"<script>1</script>";
    for raw in [
        "a.html\u{200F}",
        "a.html\u{200B}",
        "a.html\0",
        "a.html\u{202E}",
    ] {
        let resp = post_prompt(&env, vec![attachment(raw, "text/html", html)]);
        assert_eq!(resp.status, 200, "{raw:?}");
        let body = resp.json_value().unwrap();
        let name = body["attachments"][0]["name"].as_str().unwrap();
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')),
            "{name}"
        );
        assert_eq!(
            body["attachments"][0]["media_type"],
            "application/octet-stream"
        );
        let served = get(&env, body["attachments"][0]["url"].as_str().unwrap());
        assert_eq!(served.status, 200);
        assert_eq!(served.content_type, "application/octet-stream");
        assert!(
            header(&served, "Content-Disposition")
                .unwrap()
                .starts_with("attachment;")
        );
        assert_locked_down(&served);
    }
}

#[test]
fn head_carries_the_same_headers_as_get() {
    let env = env();
    env.plant("1-0-page.html", b"<script>1</script>");
    env.plant("1-0-pic.png", PNG);
    for name in ["1-0-page.html", "1-0-pic.png"] {
        let url = format!("/api/v1/attachments/{name}");
        let got = get(&env, &url);
        let head = handle_http_request(
            &env.state,
            &HttpRequest::new("HEAD", &url).with_header("Authorization", format!("Bearer {OWNER}")),
        );
        assert_eq!(head.status, 200, "{name}");
        assert_eq!(head.content_type, got.content_type, "{name}");
        assert_locked_down(&head);
        assert_eq!(
            header(&head, "Content-Disposition"),
            header(&got, "Content-Disposition"),
            "{name}"
        );
        let wire = String::from_utf8_lossy(&head.to_http_bytes(true)).to_string();
        assert!(wire.contains("X-Content-Type-Options: nosniff"), "{wire}");
        assert!(wire.ends_with("\r\n\r\n"), "no body on the wire for HEAD");
    }
}

#[test]
fn a_long_name_is_cut_but_keeps_its_extension_for_every_decision() {
    let env = env();
    let long = "x".repeat(400);
    // A long name still gets the blocked-extension check on its real extension.
    let blocked = post_prompt(&env, vec![attachment(&format!("{long}.html"), "", b"<p>")]);
    assert_eq!(blocked.status, 415);
    assert_eq!(env.stored_count(), 0);

    let resp = post_prompt(
        &env,
        vec![attachment(&format!("{long}.png"), "text/html", PNG)],
    );
    assert_eq!(resp.status, 200);
    let body = resp.json_value().unwrap();
    let name = body["attachments"][0]["name"].as_str().unwrap();
    assert_eq!(name.len(), crate::attachment_policy::MAX_SAVED_NAME_CHARS);
    assert!(name.ends_with(".png"), "{name}");
    assert_eq!(body["attachments"][0]["media_type"], "image/png");
    let served = get(&env, body["attachments"][0]["url"].as_str().unwrap());
    assert_eq!(served.status, 200);
    assert_eq!(served.content_type, "image/png");

    // A long name with no usable extension, and the degenerate shapes.
    use crate::attachment_policy::sanitize_upload_name as clean;
    assert_eq!(clean(&"y".repeat(500)).len(), 120);
    let weird = format!("{}.{}", "z".repeat(200), "e".repeat(40));
    assert_eq!(clean(&weird).len(), 120);
    assert_eq!(clean("..."), "attachment.bin");
    assert_eq!(clean("..hidden.txt"), "hidden.txt");
    assert_eq!(clean("a b\"c.txt"), "a_b_c.txt");
}

#[test]
fn a_taken_name_gets_a_counter_instead_of_being_overwritten() {
    let env = env();
    fs::create_dir_all(env.attachments()).unwrap();
    let dir = env.attachments();
    fs::write(dir.join("7-0-a.txt"), "someone else's").unwrap();
    let stored = crate::attachment_policy::save_attachment_files(
        &dir,
        7,
        &[("a.txt", &b"mine"[..]), ("b.txt", &b"also mine"[..])],
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(dir.join("7-0-a.txt")).unwrap(),
        "someone else's"
    );
    assert_ne!(stored[0].saved_name, "7-0-a.txt");
    assert!(
        stored[0].saved_name.ends_with("_a.txt"),
        "{}",
        stored[0].saved_name
    );
    assert_eq!(fs::read(&stored[0].path).unwrap(), b"mine");
    assert_eq!(stored[1].saved_name, "7-1-b.txt");
    // The retry name is still upload-shaped, so the sweep can expire it.
    let parts: Vec<&str> = stored[0].saved_name.splitn(3, '-').collect();
    assert!(parts[0].parse::<u64>().is_ok() && parts[1].parse::<u64>().is_ok());
}

#[test]
fn a_failed_save_removes_exactly_the_files_this_call_created() {
    let env = env();
    fs::create_dir_all(env.attachments()).unwrap();
    let dir = env.attachments();
    // Another prompt's file has the first-try name of our first file.
    fs::write(dir.join("9-0-a.txt"), "concurrent").unwrap();
    fs::write(dir.join("9-2-c.txt"), "also concurrent").unwrap();
    let calls = std::cell::Cell::new(0);
    let result = crate::attachment_policy::save_attachment_files_with(
        &dir,
        9,
        &[
            ("a.txt", &b"one"[..]),
            ("b.txt", &b"two"[..]),
            ("c.txt", &b"three"[..]),
        ],
        |file, bytes| {
            use std::io::Write;
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                // The third file is half written and then fails.
                file.write_all(&bytes[..1])?;
                return Err(std::io::Error::other("disk full"));
            }
            file.write_all(bytes)
        },
    );
    assert!(result.is_err());
    let mut left: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, ["9-0-a.txt", "9-2-c.txt"]);
    assert_eq!(
        fs::read_to_string(dir.join("9-0-a.txt")).unwrap(),
        "concurrent"
    );
    assert_eq!(
        fs::read_to_string(dir.join("9-2-c.txt")).unwrap(),
        "also concurrent"
    );
}

#[test]
fn the_aggregate_limit_is_reachable_inside_the_http_request_cap() {
    use crate::attachment_policy::HTTP_ENVELOPE_ALLOWANCE_BYTES;
    assert_eq!(MAX_ATTACHMENTS_TOTAL_BYTES, 18_087_936);
    let base64_of_all = MAX_ATTACHMENTS_TOTAL_BYTES.div_ceil(3) * 4;
    assert!(
        base64_of_all + HTTP_ENVELOPE_ALLOWANCE_BYTES <= crate::ws::MAX_HTTP_REQUEST_BYTES,
        "a request at the aggregate limit must fit the HTTP reader cap"
    );
}
