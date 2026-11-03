//! Audit P1-13: the credential proxy must ignore ambient `HTTP(S)_PROXY` / `ALL_PROXY`.
//!
//! This test is alone in its own test binary because it edits the process environment, which
//! no other test in the same process may race with. Every listener is on 127.0.0.1.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use fl_gate::{ProxyRequest, SetSecretArgs, Vault, execute_proxied_request};

/// A 127.0.0.1 listener that counts accepted connections and answers each with `200 ok`.
struct Counter {
    port: u16,
    accepts: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Counter {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (a, s) = (accepts.clone(), stop.clone());
        let handle = thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        a.fetch_add(1, Ordering::SeqCst);
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self {
            port,
            accepts,
            stop,
            handle: Some(handle),
        }
    }

    fn accepts(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[test]
fn ambient_proxy_variables_are_ignored_and_the_request_goes_direct() {
    let target = Counter::start();
    let decoy_proxy = Counter::start();

    let dir = tempfile::tempdir().unwrap();
    let mut vault = Vault::open(dir.path().join("vault.enc"), [5u8; 32]).unwrap();
    vault
        .set(SetSecretArgs {
            name: "UPSTREAM_KEY".into(),
            value: "throwaway-key-0123456789".into(),
            allowed_hosts: vec!["127.0.0.1".into()],
            allow_private_network: true,
            ..Default::default()
        })
        .unwrap();

    let proxy_url = format!("http://127.0.0.1:{}", decoy_proxy.port);
    let vars = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];
    let saved: Vec<(&str, Option<String>)> =
        vars.iter().map(|v| (*v, std::env::var(v).ok())).collect();
    let saved_no_proxy = (
        std::env::var("NO_PROXY").ok(),
        std::env::var("no_proxy").ok(),
    );
    // SAFETY: this binary holds one test, so nothing else reads or writes the environment now.
    unsafe {
        for v in vars {
            std::env::set_var(v, &proxy_url);
        }
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("no_proxy");
    }

    let req = ProxyRequest {
        method: "GET".into(),
        url: format!("http://127.0.0.1:{}/ping", target.port),
        headers: BTreeMap::from([("X-Key".into(), "{{FERROGATE:UPSTREAM_KEY}}".into())]),
        body: None,
        caller_agent: Some("tester".into()),
        timeout_secs: Some(5),
    };
    let result = execute_proxied_request(&vault, &req);

    // SAFETY: as above.
    unsafe {
        for (v, old) in saved {
            match old {
                Some(val) => std::env::set_var(v, val),
                None => std::env::remove_var(v),
            }
        }
        if let Some(val) = saved_no_proxy.0 {
            std::env::set_var("NO_PROXY", val);
        }
        if let Some(val) = saved_no_proxy.1 {
            std::env::set_var("no_proxy", val);
        }
    }
    thread::sleep(Duration::from_millis(150));

    let resp = result.expect("the request must succeed direct");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "ok");
    assert_eq!(target.accepts(), 1, "the request reached its target");
    assert_eq!(
        decoy_proxy.accepts(),
        0,
        "the ambient proxy must never be contacted"
    );
}
