//! Zero-knowledge credential proxy (`{{FERROGATE:KEY}}` placeholder substitution),
//! domain/method/agent ACL enforcement, SSRF blocking, and response secret scrubbing (spec §3).

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::GateError;
use crate::vault::{Vault, normalize_secret_name};

pub const PLACEHOLDER_PREFIX: &str = "{{FERROGATE:";
pub const PLACEHOLDER_SUFFIX: &str = "}}";

/// Outbound HTTP request passed to [`execute_proxied_request`] or [`prepare_proxied_request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub caller_agent: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Prepared request after placeholder substitution and policy/SSRF verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedProxyRequest {
    pub method: String,
    pub url: String,
    pub host: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
    pub injected_secrets: Vec<String>,
}

/// Scrubbed HTTP response returned to the caller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
    pub scrubbed_secrets: Vec<String>,
}

/// Extract all `{{FERROGATE:<KEY>}}` secret names referenced across `text`.
pub fn extract_placeholder_keys(text: &str) -> Result<Vec<String>, GateError> {
    let mut keys = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(PLACEHOLDER_PREFIX) {
        let after_prefix = &rest[start + PLACEHOLDER_PREFIX.len()..];
        let Some(end) = after_prefix.find(PLACEHOLDER_SUFFIX) else {
            return Err(GateError::InvalidPlaceholder {
                reason: "unclosed `{{FERROGATE:...}}` placeholder".to_string(),
            });
        };
        let raw_key = &after_prefix[..end];
        let norm = normalize_secret_name(raw_key)?;
        if !keys.contains(&norm) {
            keys.push(norm);
        }
        rest = &after_prefix[end + PLACEHOLDER_SUFFIX.len()..];
    }
    Ok(keys)
}

/// Parse URL scheme, host, and port from `raw_url`.
pub fn parse_url_scheme_host_port(raw_url: &str) -> Result<(String, String, u16), GateError> {
    let trimmed = raw_url.trim();
    let (scheme, after_scheme) =
        trimmed
            .split_once("://")
            .ok_or_else(|| GateError::InvalidUrl {
                url: raw_url.to_string(),
                reason: "URL must start with https:// or http://".to_string(),
            })?;
    let scheme_lower = scheme.to_ascii_lowercase();
    if scheme_lower != "https" && scheme_lower != "http" {
        return Err(GateError::InvalidUrl {
            url: raw_url.to_string(),
            reason: format!("unsupported URL scheme `{scheme_lower}`"),
        });
    }
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return Err(GateError::InvalidUrl {
            url: raw_url.to_string(),
            reason: "URL authority must be a valid host without embedded credentials".to_string(),
        });
    }

    let (host, port) = if authority.starts_with('[') {
        let Some(close_bracket) = authority.find(']') else {
            return Err(GateError::InvalidUrl {
                url: raw_url.to_string(),
                reason: "unclosed IPv6 bracket in host".to_string(),
            });
        };
        let h = authority[1..close_bracket].to_ascii_lowercase();
        let rest_port = &authority[close_bracket + 1..];
        let p = if let Some(p_str) = rest_port.strip_prefix(':') {
            p_str.parse::<u16>().map_err(|_| GateError::InvalidUrl {
                url: raw_url.to_string(),
                reason: "invalid port number".to_string(),
            })?
        } else if scheme_lower == "https" {
            443
        } else {
            80
        };
        (h, p)
    } else if let Some((h, p_str)) = authority.rsplit_once(':') {
        let p = p_str.parse::<u16>().map_err(|_| GateError::InvalidUrl {
            url: raw_url.to_string(),
            reason: "invalid port number".to_string(),
        })?;
        (h.to_ascii_lowercase(), p)
    } else {
        let p = if scheme_lower == "https" { 443 } else { 80 };
        (authority.to_ascii_lowercase(), p)
    };

    if host.is_empty() {
        return Err(GateError::InvalidUrl {
            url: raw_url.to_string(),
            reason: "empty host in URL".to_string(),
        });
    }
    Ok((scheme_lower, host, port))
}

/// Return `true` if `host` matches `pattern` (exact match or `*.domain.tld` wildcard).
pub fn host_matches_pattern(host: &str, pattern: &str) -> bool {
    let h = host.trim().to_ascii_lowercase();
    let p = pattern.trim().to_ascii_lowercase();
    if p == "*" {
        return true;
    }
    if let Some(suffix) = p.strip_prefix("*.") {
        h == suffix
            || (h.ends_with(suffix) && h.as_bytes().get(h.len() - suffix.len() - 1) == Some(&b'.'))
    } else {
        h == p
    }
}

/// Return `true` if `ip` is loopback, link-local, unspecified, multicast, or RFC1918/ULA private.
pub fn is_private_or_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_private_or_loopback_ip(IpAddr::V4(v4)))
        }
    }
}

/// Check whether `host` resolves to or is a loopback/private address (SSRF guard).
pub fn verify_host_ssrf(
    host: &str,
    port: u16,
    allow_private_network: bool,
) -> Result<(), GateError> {
    if allow_private_network {
        return Ok(());
    }
    let lower = host.trim().to_ascii_lowercase();
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
    {
        return Err(GateError::SsrfBlocked {
            host: host.to_string(),
            reason: "local hostname is blocked unless `allow_private_network = true`".to_string(),
        });
    }
    if let Ok(ip) = lower.parse::<IpAddr>() {
        if is_private_or_loopback_ip(ip) {
            return Err(GateError::SsrfBlocked {
                host: host.to_string(),
                reason: format!("private or loopback IP `{ip}` is blocked"),
            });
        }
        return Ok(());
    }
    if let Ok(addrs) = (lower.as_str(), port).to_socket_addrs() {
        for sock in addrs {
            if is_private_or_loopback_ip(sock.ip()) {
                return Err(GateError::SsrfBlocked {
                    host: host.to_string(),
                    reason: format!("host resolves to private or loopback IP `{}`", sock.ip()),
                });
            }
        }
    }
    Ok(())
}

/// Validate all `{{FERROGATE:KEY}}` placeholders in `req` against `vault` policies and SSRF rules,
/// returning a [`PreparedProxyRequest`] ready for transport execution.
pub fn prepare_proxied_request(
    vault: &Vault,
    req: &ProxyRequest,
) -> Result<PreparedProxyRequest, GateError> {
    let method = req.method.trim().to_ascii_uppercase();
    if method.is_empty() {
        return Err(GateError::InvalidUrl {
            url: req.url.clone(),
            reason: "HTTP method must not be empty".to_string(),
        });
    }

    // Collect all referenced placeholders across URL, headers, and body BEFORE parsing URL
    // (in case a query parameter has a placeholder).
    let mut referenced = BTreeSet::new();
    for k in extract_placeholder_keys(&req.url)? {
        referenced.insert(k);
    }
    for v in req.headers.values() {
        for k in extract_placeholder_keys(v)? {
            referenced.insert(k);
        }
    }
    if let Some(ref b) = req.body {
        for k in extract_placeholder_keys(b)? {
            referenced.insert(k);
        }
    }

    // Ensure placeholders never appear in the scheme/authority portion of the URL.
    let authority_part = req
        .url
        .split_once('?')
        .map(|(before, _)| before)
        .unwrap_or(&req.url);
    if authority_part.contains(PLACEHOLDER_PREFIX) {
        return Err(GateError::InvalidPlaceholder {
            reason: "secret placeholders are not permitted in the URL host or path before `?`"
                .to_string(),
        });
    }

    let (scheme, host, port) = parse_url_scheme_host_port(&req.url)?;

    let mut decrypted_map = BTreeMap::new();
    let mut allow_private = false;

    for secret_name in &referenced {
        let (meta, secret_val) = vault.decrypt_raw(secret_name)?;

        if !meta.allow_private_network && scheme != "https" {
            return Err(GateError::PolicyDenied {
                secret: secret_name.clone(),
                reason: format!(
                    "secret `{secret_name}` requires an `https://` destination unless `allow_private_network = true`"
                ),
            });
        }

        if meta.allowed_hosts.is_empty()
            || !meta
                .allowed_hosts
                .iter()
                .any(|pat| host_matches_pattern(&host, pat))
        {
            return Err(GateError::PolicyDenied {
                secret: secret_name.clone(),
                reason: format!(
                    "destination host `{host}` is not in allowed_hosts {:?}",
                    meta.allowed_hosts
                ),
            });
        }

        if !meta.allowed_methods.is_empty()
            && !meta
                .allowed_methods
                .iter()
                .any(|m| m.eq_ignore_ascii_case(&method))
        {
            return Err(GateError::PolicyDenied {
                secret: secret_name.clone(),
                reason: format!(
                    "HTTP method `{method}` is not in allowed_methods {:?}",
                    meta.allowed_methods
                ),
            });
        }

        if !meta.allowed_agents.is_empty() {
            let caller = req
                .caller_agent
                .as_deref()
                .map(|s| s.trim().to_ascii_lowercase())
                .unwrap_or_default();
            let permitted = meta
                .allowed_agents
                .iter()
                .any(|a| a == "*" || a.eq_ignore_ascii_case(&caller));
            if !permitted {
                return Err(GateError::PolicyDenied {
                    secret: secret_name.clone(),
                    reason: format!(
                        "caller agent `{caller}` is not in allowed_agents {:?}",
                        meta.allowed_agents
                    ),
                });
            }
        }

        if meta.allow_private_network {
            allow_private = true;
        }
        decrypted_map.insert(secret_name.clone(), secret_val);
    }

    verify_host_ssrf(
        &host,
        port,
        allow_private
            && referenced.iter().all(|k| {
                vault
                    .get_metadata(k)
                    .is_some_and(|m| m.allow_private_network)
            }),
    )?;

    let replace_all = |input: &str| -> String {
        let mut out = input.to_string();
        for (k, val) in &decrypted_map {
            let token = format!("{PLACEHOLDER_PREFIX}{k}{PLACEHOLDER_SUFFIX}");
            out = out.replace(&token, val);
        }
        out
    };

    let final_url = replace_all(&req.url);
    let mut final_headers = BTreeMap::new();
    for (hk, hv) in &req.headers {
        final_headers.insert(hk.clone(), replace_all(hv));
    }
    let final_body = req.body.as_ref().map(|b| replace_all(b));
    let injected_secrets: Vec<String> = referenced.into_iter().collect();
    if !injected_secrets.is_empty() {
        vault.append_audit_log(
            req.caller_agent.as_deref(),
            &injected_secrets,
            &method,
            &host,
            "allowed",
        );
    }

    Ok(PreparedProxyRequest {
        method,
        url: final_url,
        host,
        headers: final_headers,
        body: final_body,
        injected_secrets,
    })
}

/// Scrub any known secret values from `vault` (plain, URL-encoded, and base64-encoded)
/// and common credential prefixes from `text`.
pub fn scrub_secrets(vault: &Vault, text: &str) -> (String, Vec<String>) {
    let mut scrubbed = text.to_string();
    let mut matched = Vec::new();

    for (name, secret) in vault.all_decrypted_for_scrubbing() {
        if secret.len() < 4 {
            continue;
        }
        let replacement = format!("[REDACTED:FERROGATE:{name}]");
        let b64 = BASE64.encode(secret.as_bytes());
        let url_enc = simple_percent_encode(&secret);

        let mut hit = false;
        if scrubbed.contains(&secret) {
            scrubbed = scrubbed.replace(&secret, &replacement);
            hit = true;
        }
        if b64.len() >= 8 && scrubbed.contains(&b64) {
            scrubbed = scrubbed.replace(&b64, &replacement);
            hit = true;
        }
        if url_enc != secret && scrubbed.contains(&url_enc) {
            scrubbed = scrubbed.replace(&url_enc, &replacement);
            hit = true;
        }
        if hit {
            matched.push(name);
        }
    }

    for (prefix, label) in [
        ("sk-ant-", "ANTHROPIC_TOKEN"),
        ("sk-proj-", "OPENAI_TOKEN"),
        ("ghp_", "GITHUB_PAT"),
        ("github_pat_", "GITHUB_FINE_PAT"),
        ("xoxb-", "SLACK_BOT_TOKEN"),
    ] {
        let (next, redacted) = redact_prefixed_tokens(&scrubbed, prefix, label);
        if redacted {
            scrubbed = next;
            if !matched.iter().any(|m| m == label) {
                matched.push(label.to_string());
            }
        }
    }

    (scrubbed, matched)
}

fn redact_prefixed_tokens(text: &str, prefix: &str, label: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut any_redacted = false;

    while let Some(idx) = rest.find(prefix) {
        out.push_str(&rest[..idx]);
        let after_prefix = &rest[idx + prefix.len()..];
        let token_tail_len = after_prefix
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .map(char::len_utf8)
            .sum::<usize>();
        if token_tail_len >= 6 {
            out.push_str(&format!("[REDACTED:FERROGATE:{label}]"));
            any_redacted = true;
            rest = &after_prefix[token_tail_len..];
        } else {
            out.push_str(prefix);
            rest = after_prefix;
        }
    }
    out.push_str(rest);
    (out, any_redacted)
}

fn simple_percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            use std::fmt::Write as _;
            let _ = write!(&mut out, "%{b:02X}");
        }
    }
    out
}

/// Execute `req` through the zero-knowledge credential proxy (`prepare_proxied_request`),
/// disabling automatic redirect following so credentials are never forwarded cross-origin,
/// and scrubbing any secret values from the response body and headers.
pub fn execute_proxied_request(
    vault: &Vault,
    req: &ProxyRequest,
) -> Result<ProxyResponse, GateError> {
    let prepared = prepare_proxied_request(vault, req)?;
    let timeout = Duration::from_secs(req.timeout_secs.unwrap_or(30).clamp(1, 300));

    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .http_status_as_error(false)
        .build();
    let agent: ureq::Agent = config.into();

    let send_result = match prepared.method.as_str() {
        "GET" => {
            let mut b = agent.get(&prepared.url);
            for (k, v) in &prepared.headers {
                b = b.header(k, v);
            }
            b.call()
        }
        "DELETE" => {
            let mut b = agent.delete(&prepared.url);
            for (k, v) in &prepared.headers {
                b = b.header(k, v);
            }
            b.call()
        }
        "HEAD" => {
            let mut b = agent.head(&prepared.url);
            for (k, v) in &prepared.headers {
                b = b.header(k, v);
            }
            b.call()
        }
        "POST" => {
            let mut b = agent.post(&prepared.url);
            for (k, v) in &prepared.headers {
                b = b.header(k, v);
            }
            b.send(prepared.body.as_deref().unwrap_or("").as_bytes())
        }
        "PUT" => {
            let mut b = agent.put(&prepared.url);
            for (k, v) in &prepared.headers {
                b = b.header(k, v);
            }
            b.send(prepared.body.as_deref().unwrap_or("").as_bytes())
        }
        "PATCH" => {
            let mut b = agent.patch(&prepared.url);
            for (k, v) in &prepared.headers {
                b = b.header(k, v);
            }
            b.send(prepared.body.as_deref().unwrap_or("").as_bytes())
        }
        other => {
            return Err(GateError::InvalidUrl {
                url: prepared.url,
                reason: format!("unsupported HTTP method `{other}`"),
            });
        }
    };

    let mut resp = send_result.map_err(|e| {
        let (scrubbed_err, _) = scrub_secrets(vault, &e.to_string());
        GateError::UpstreamHttp {
            cause: scrubbed_err,
        }
    })?;

    let status = resp.status().as_u16();
    let mut resp_headers = BTreeMap::new();
    let mut scrubbed_names = BTreeSet::new();
    for (hk, hv) in resp.headers() {
        if let Ok(val_str) = hv.to_str() {
            let (clean_v, hits) = scrub_secrets(vault, val_str);
            for h in hits {
                scrubbed_names.insert(h);
            }
            resp_headers.insert(hk.as_str().to_ascii_lowercase(), clean_v);
        }
    }

    let raw_body = resp.body_mut().read_to_string().unwrap_or_default();
    let (clean_body, body_hits) = scrub_secrets(vault, &raw_body);
    for h in body_hits {
        scrubbed_names.insert(h);
    }

    Ok(ProxyResponse {
        status,
        headers: resp_headers,
        body: clean_body,
        scrubbed_secrets: scrubbed_names.into_iter().collect(),
    })
}
