//! Target resolution and the Tier 1 fetch.
//!
//! A target is an `http://` or `https://` URL, a `file://` URL, or a filesystem path. Any other
//! scheme is refused. The fetch is bounded: a global timeout, a redirect limit, and a body
//! limit that also applies to the decoded body of a compressed response.
//!
//! It sends nothing of the caller's: no proxy from the environment, no cookies, no
//! `Authorization`, no credentials in the URL (a URL that carries a user name or password is
//! refused rather than half-honoured), and a fixed `User-Agent`.

use crate::BrowseError;
use crate::html::MAX_HTML_BYTES;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A `Content-Type` this crate treats as HTML.
const HTML_TYPES: &[&str] = &["text/html", "application/xhtml+xml"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An `http://` or `https://` URL, validated.
    Http(String),
    /// A local file, from a `file://` URL or a path.
    File(PathBuf),
}

/// Limits for a fetch.
#[derive(Debug, Clone, Copy)]
pub struct FetchOptions {
    pub timeout: Duration,
    pub max_bytes: usize,
    pub max_redirects: u32,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(15),
            max_bytes: MAX_HTML_BYTES,
            max_redirects: 5,
        }
    }
}

/// The document a target resolved to.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub html: String,
}

/// The scheme of `s` when it has the `scheme://` form.
fn scheme_of(s: &str) -> Option<&str> {
    let (scheme, _) = s.split_once("://")?;
    let ok = !scheme.is_empty()
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    ok.then_some(scheme)
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// Resolve the text of a target. Does not touch the network or the filesystem.
pub fn parse_target(raw: &str) -> Result<Target, BrowseError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(BrowseError::InvalidTarget(
            "the target is empty; give an http(s) URL, a file:// URL or a path to an HTML file"
                .into(),
        ));
    }
    if let Some(scheme) = scheme_of(s) {
        let scheme = scheme.to_ascii_lowercase();
        return match scheme.as_str() {
            "http" | "https" => parse_http(s),
            "file" => parse_file_url(s),
            _ => Err(BrowseError::UnsupportedScheme(scheme)),
        };
    }
    // Schemes written without `//`.
    if let Some((scheme, _)) = s.split_once(':')
        && matches!(
            scheme.to_ascii_lowercase().as_str(),
            "javascript" | "data" | "about" | "mailto" | "blob" | "view-source" | "chrome"
        )
    {
        return Err(BrowseError::UnsupportedScheme(scheme.to_ascii_lowercase()));
    }
    if s.contains('\0') {
        return Err(BrowseError::InvalidTarget(
            "the path contains a NUL byte".into(),
        ));
    }
    Ok(Target::File(PathBuf::from(s)))
}

fn parse_http(s: &str) -> Result<Target, BrowseError> {
    let uri: ureq::http::Uri = s
        .parse()
        .map_err(|e| BrowseError::InvalidTarget(format!("`{s}` is not a valid URL: {e}")))?;
    let authority = uri
        .authority()
        .ok_or_else(|| BrowseError::InvalidTarget(format!("`{s}` has no host")))?;
    if authority.as_str().contains('@') {
        return Err(BrowseError::CredentialsInUrl);
    }
    if authority.host().is_empty() {
        return Err(BrowseError::InvalidTarget(format!("`{s}` has no host")));
    }
    Ok(Target::Http(s.to_string()))
}

fn parse_file_url(s: &str) -> Result<Target, BrowseError> {
    let rest = &s["file://".len().min(s.len())..];
    let rest = rest.split(['#', '?']).next().unwrap_or("");
    let path = if rest.starts_with('/') {
        rest
    } else {
        // `file://host/path`: only the local host is a file here.
        match rest.split_once('/') {
            Some((host, _)) if host.eq_ignore_ascii_case("localhost") => &rest[host.len()..],
            _ => {
                return Err(BrowseError::InvalidTarget(format!(
                    "`{s}` is not a local file URL; write file:///absolute/path"
                )));
            }
        }
    };
    let bytes = percent_decode(path);
    if bytes.contains(&0) {
        return Err(BrowseError::InvalidTarget(
            "the path contains a NUL byte".into(),
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        BrowseError::InvalidTarget(format!("`{s}` is not valid UTF-8 once decoded"))
    })?;
    Ok(Target::File(PathBuf::from(text)))
}

/// `file://` URL (percent-encoded) for an absolute path.
pub fn file_url(path: &Path) -> String {
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_encoded_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(2048).any(|&b| b == 0)
}

fn to_html(bytes: Vec<u8>, what: &str) -> Result<Fetched, BrowseError> {
    if looks_binary(&bytes) {
        return Err(BrowseError::NotHtml(format!(
            "{what} holds binary data (a NUL byte in the first 2 KiB)"
        )));
    }
    let html = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    Ok(Fetched { html })
}

fn read_file(path: &Path, opts: &FetchOptions) -> Result<Fetched, BrowseError> {
    let io = |source| BrowseError::Io {
        path: path.display().to_string(),
        source,
    };
    let meta = std::fs::metadata(path).map_err(io)?;
    if !meta.is_file() {
        return Err(BrowseError::NotHtml(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .map_err(io)?
        .take(opts.max_bytes as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(io)?;
    if buf.len() > opts.max_bytes {
        return Err(BrowseError::TooLarge {
            what: path.display().to_string(),
            limit: opts.max_bytes,
        });
    }
    to_html(buf, &path.display().to_string())
}

fn http_error(url: &str, e: ureq::Error, opts: &FetchOptions) -> BrowseError {
    match e {
        ureq::Error::BodyExceedsLimit(_) => BrowseError::TooLarge {
            what: url.to_string(),
            limit: opts.max_bytes,
        },
        ureq::Error::TooManyRedirects => BrowseError::Http {
            url: url.to_string(),
            message: format!("more than {} redirects", opts.max_redirects),
        },
        other => BrowseError::Http {
            url: url.to_string(),
            message: other.to_string(),
        },
    }
}

fn fetch_http(url: &str, opts: &FetchOptions) -> Result<Fetched, BrowseError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(opts.timeout))
        .max_redirects(opts.max_redirects)
        .max_redirects_will_error(true)
        .http_status_as_error(false)
        .proxy(None)
        .user_agent(concat!("fl-browse/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut resp = agent
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.1")
        .call()
        .map_err(|e| http_error(url, e, opts))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(BrowseError::Http {
            url: url.to_string(),
            message: format!("the server answered HTTP {status}"),
        });
    }
    if let Some(ty) = resp.body().mime_type()
        && !HTML_TYPES.contains(&ty.to_ascii_lowercase().as_str())
    {
        return Err(BrowseError::NotHtml(format!(
            "{url} answered `{ty}`, not HTML"
        )));
    }
    if let Some(len) = resp.body().content_length()
        && len > opts.max_bytes as u64
    {
        return Err(BrowseError::TooLarge {
            what: url.to_string(),
            limit: opts.max_bytes,
        });
    }
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(opts.max_bytes as u64)
        .read_to_vec()
        .map_err(|e| http_error(url, e, opts))?;
    to_html(bytes, url)
}

/// Fetch (or read) the document a target names.
pub fn fetch(target: &Target, opts: &FetchOptions) -> Result<Fetched, BrowseError> {
    match target {
        Target::Http(url) => fetch_http(url, opts),
        Target::File(path) => read_file(path, opts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    /// A one-shot-per-request local server answering `routes` (path -> (status, headers, body)).
    struct Server {
        url: String,
        _stop: mpsc::Sender<()>,
    }

    type Route = (&'static str, u16, Vec<(&'static str, String)>, Vec<u8>);

    fn serve(routes: Vec<Route>) -> Server {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        let (stop, rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            while rx.try_recv().is_err() {
                let Ok(Some(req)) = server.recv_timeout(Duration::from_millis(50)) else {
                    continue;
                };
                let path = req.url().to_string();
                match routes.iter().find(|(p, ..)| *p == path) {
                    Some((_, status, headers, body)) => {
                        let mut r =
                            tiny_http::Response::from_data(body.clone()).with_status_code(*status);
                        for (k, v) in headers {
                            r.add_header(
                                tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap(),
                            );
                        }
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

    fn html_route(path: &'static str, body: &str) -> Route {
        (
            path,
            200,
            vec![("Content-Type", "text/html; charset=utf-8".into())],
            body.as_bytes().to_vec(),
        )
    }

    fn get(url: String, opts: FetchOptions) -> Result<Fetched, BrowseError> {
        fetch(&parse_target(&url)?, &opts)
    }

    #[test]
    fn parses_http_file_and_path_targets() {
        assert_eq!(
            parse_target("http://localhost:8080/a?b=1#c").unwrap(),
            Target::Http("http://localhost:8080/a?b=1#c".into())
        );
        assert!(matches!(
            parse_target("HTTPS://example.com").unwrap(),
            Target::Http(_)
        ));
        assert_eq!(
            parse_target("file:///tmp/a%20b.html").unwrap(),
            Target::File("/tmp/a b.html".into())
        );
        assert_eq!(
            parse_target("file://localhost/tmp/x.html#frag").unwrap(),
            Target::File("/tmp/x.html".into())
        );
        assert_eq!(
            parse_target("./page.html").unwrap(),
            Target::File("./page.html".into())
        );
        assert_eq!(
            parse_target("  rel/dir/p.html ").unwrap(),
            Target::File("rel/dir/p.html".into())
        );
    }

    #[test]
    fn refuses_other_schemes_with_the_scheme_named() {
        for (t, scheme) in [
            ("ftp://example.com/x", "ftp"),
            ("javascript:alert(1)", "javascript"),
            ("data:text/html,<p>x</p>", "data"),
            ("gopher://x", "gopher"),
            ("ws://x", "ws"),
        ] {
            match parse_target(t) {
                Err(BrowseError::UnsupportedScheme(s)) => assert_eq!(s, scheme),
                other => panic!("{t}: {other:?}"),
            }
        }
        let msg = parse_target("ftp://x").unwrap_err().to_string();
        assert!(msg.contains("http") && msg.contains("file"), "{msg}");
    }

    #[test]
    fn refuses_bad_urls_and_credentials_in_urls() {
        assert!(matches!(
            parse_target("http://user:pw@example.com/"),
            Err(BrowseError::CredentialsInUrl)
        ));
        assert!(matches!(
            parse_target("http://tok@example.com/"),
            Err(BrowseError::CredentialsInUrl)
        ));
        for t in [
            "",
            "   ",
            "http://",
            "http:///x",
            "file://otherhost/x",
            "file://x",
            "a\0b",
        ] {
            assert!(
                matches!(parse_target(t), Err(BrowseError::InvalidTarget(_))),
                "{t:?}: {:?}",
                parse_target(t)
            );
        }
    }

    #[test]
    fn file_urls_round_trip() {
        for p in ["/tmp/a b/c.html", "/tmp/\u{e9}.html", "/tmp/100%/x#y.html"] {
            let url = file_url(Path::new(p));
            assert_eq!(parse_target(&url).unwrap(), Target::File(p.into()), "{url}");
        }
    }

    #[test]
    fn reads_a_local_file_and_rejects_the_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.html");
        std::fs::write(&p, "<p>hi</p>").unwrap();
        let f = fetch(&Target::File(p.clone()), &FetchOptions::default()).unwrap();
        assert_eq!(f.html, "<p>hi</p>");

        let missing = fetch(
            &Target::File(dir.path().join("no.html")),
            &FetchOptions::default(),
        );
        assert!(matches!(missing, Err(BrowseError::Io { .. })));
        let is_dir = fetch(
            &Target::File(dir.path().to_path_buf()),
            &FetchOptions::default(),
        );
        assert!(matches!(is_dir, Err(BrowseError::NotHtml(_))));

        let bin = dir.path().join("b.bin");
        std::fs::write(&bin, [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
        assert!(matches!(
            fetch(&Target::File(bin), &FetchOptions::default()),
            Err(BrowseError::NotHtml(_))
        ));

        let big = dir.path().join("big.html");
        std::fs::write(&big, "x".repeat(101)).unwrap();
        let opts = FetchOptions {
            max_bytes: 100,
            ..FetchOptions::default()
        };
        assert!(matches!(
            fetch(&Target::File(big), &opts),
            Err(BrowseError::TooLarge { limit: 100, .. })
        ));
    }

    #[test]
    fn invalid_utf8_is_replaced_not_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("l.html");
        std::fs::write(&p, b"<p>caf\xe9</p>").unwrap();
        let f = fetch(&Target::File(p), &FetchOptions::default()).unwrap();
        assert_eq!(f.html, "<p>caf\u{fffd}</p>");
    }

    #[test]
    fn fetches_html_over_http() {
        let s = serve(vec![html_route("/", "<title>Hi</title>")]);
        let f = get(format!("{}/", s.url), FetchOptions::default()).unwrap();
        assert_eq!(f.html, "<title>Hi</title>");
    }

    #[test]
    fn sends_a_fixed_user_agent_and_no_credentials() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", server.server_addr().to_ip().unwrap());
        let h = thread::spawn(move || {
            let req = server.recv().unwrap();
            let headers: Vec<(String, String)> = req
                .headers()
                .iter()
                .map(|h| (h.field.to_string().to_lowercase(), h.value.to_string()))
                .collect();
            let _ =
                req.respond(tiny_http::Response::from_string("<p>x</p>").with_header(
                    tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap(),
                ));
            headers
        });
        get(url, FetchOptions::default()).unwrap();
        let headers = h.join().unwrap();
        let ua = headers.iter().find(|(k, _)| k == "user-agent").unwrap();
        assert!(ua.1.starts_with("fl-browse/"), "{ua:?}");
        for (k, _) in &headers {
            assert!(
                !matches!(
                    k.as_str(),
                    "authorization" | "cookie" | "proxy-authorization"
                ),
                "{k}"
            );
        }
    }

    #[test]
    fn http_errors_and_non_html_are_clear() {
        let s = serve(vec![
            html_route("/ok", "<p>x</p>"),
            (
                "/json",
                200,
                vec![("Content-Type", "application/json".into())],
                b"{}".to_vec(),
            ),
            (
                "/png",
                200,
                vec![("Content-Type", "image/png".into())],
                vec![0x89, 0],
            ),
            (
                "/err",
                500,
                vec![("Content-Type", "text/html".into())],
                b"boom".to_vec(),
            ),
        ]);
        let e = get(format!("{}/json", s.url), FetchOptions::default()).unwrap_err();
        assert!(
            matches!(&e, BrowseError::NotHtml(m) if m.contains("application/json")),
            "{e}"
        );
        let e = get(format!("{}/png", s.url), FetchOptions::default()).unwrap_err();
        assert!(matches!(e, BrowseError::NotHtml(_)));
        let e = get(format!("{}/err", s.url), FetchOptions::default()).unwrap_err();
        assert!(e.to_string().contains("HTTP 500"), "{e}");
        let e = get(format!("{}/missing", s.url), FetchOptions::default()).unwrap_err();
        assert!(e.to_string().contains("HTTP 404"), "{e}");
    }

    #[test]
    fn an_oversize_body_is_refused() {
        let s = serve(vec![html_route("/big", &"x".repeat(5000))]);
        let opts = FetchOptions {
            max_bytes: 1000,
            ..FetchOptions::default()
        };
        let e = get(format!("{}/big", s.url), opts).unwrap_err();
        assert!(
            matches!(e, BrowseError::TooLarge { limit: 1000, .. }),
            "{e}"
        );
        assert!(e.to_string().contains("1000"), "{e}");
    }

    #[test]
    fn follows_a_bounded_number_of_redirects() {
        let redirect = |to: &str| vec![("Location", to.to_string())];
        let s = serve(vec![
            html_route("/end", "<p>end</p>"),
            ("/r1", 302, redirect("/end"), vec![]),
            ("/loop", 302, redirect("/loop"), vec![]),
        ]);
        let f = get(format!("{}/r1", s.url), FetchOptions::default()).unwrap();
        assert_eq!(f.html, "<p>end</p>");
        let e = get(format!("{}/loop", s.url), FetchOptions::default()).unwrap_err();
        assert!(e.to_string().contains("redirect"), "{e}");
    }

    #[test]
    fn a_redirect_to_a_non_http_scheme_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.html");
        std::fs::write(&secret, "<p>local file</p>").unwrap();
        let s = serve(vec![(
            "/r",
            302,
            vec![("Location", file_url(&secret))],
            vec![],
        )]);
        let e = get(format!("{}/r", s.url), FetchOptions::default()).unwrap_err();
        assert!(matches!(e, BrowseError::Http { .. }), "{e:?}");
    }

    #[test]
    fn a_slow_server_times_out() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", server.server_addr().to_ip().unwrap());
        let h = thread::spawn(move || {
            let req = server.recv().unwrap();
            thread::sleep(Duration::from_millis(600));
            let _ = req.respond(tiny_http::Response::from_string("late"));
        });
        let opts = FetchOptions {
            timeout: Duration::from_millis(150),
            ..FetchOptions::default()
        };
        let e = get(url, opts).unwrap_err();
        assert!(matches!(e, BrowseError::Http { .. }), "{e:?}");
        h.join().unwrap();
    }

    #[test]
    fn a_refused_connection_is_an_http_error() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let e = get(format!("http://127.0.0.1:{port}/"), FetchOptions::default()).unwrap_err();
        assert!(matches!(e, BrowseError::Http { .. }), "{e:?}");
    }
}
