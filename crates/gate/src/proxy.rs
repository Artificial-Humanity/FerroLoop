//! Zero-knowledge credential proxy (`{{FERROGATE:KEY}}` placeholder substitution),
//! domain/method/agent ACL enforcement, SSRF blocking, and response secret scrubbing (spec §3).

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::GateError;
use crate::adapter::{SecretProviderAdapter, resolve_vault_secret};
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
///
/// The URL query, header values, and body carry injected plaintext, so `Debug` prints none of
/// them: a `{:?}` of this type, or of a `Result` or log record holding one, must not leak a secret.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedProxyRequest {
    pub method: String,
    pub url: String,
    pub host: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
    pub injected_secrets: Vec<String>,
}

impl std::fmt::Debug for PreparedProxyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let url_without_query = self
            .url
            .split_once('?')
            .map_or(self.url.as_str(), |(u, _)| u);
        f.debug_struct("PreparedProxyRequest")
            .field("method", &self.method)
            .field("url", &url_without_query)
            .field("host", &self.host)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("body_len", &self.body.as_ref().map(String::len))
            .field("injected_secrets", &self.injected_secrets)
            .finish()
    }
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

    let default_port = if scheme_lower == "https" { 443 } else { 80 };
    let bad_url = |reason: &str| GateError::InvalidUrl {
        url: raw_url.to_string(),
        reason: reason.to_string(),
    };
    let (host, port) = if authority.starts_with('[') {
        let Some(close_bracket) = authority.find(']') else {
            return Err(bad_url("unclosed IPv6 bracket in host"));
        };
        let h = authority[1..close_bracket].to_ascii_lowercase();
        if h.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(bad_url("bracketed host must be a plain IPv6 address"));
        }
        let rest_port = &authority[close_bracket + 1..];
        let p = if rest_port.is_empty() {
            default_port
        } else if let Some(p_str) = rest_port.strip_prefix(':') {
            parse_port(p_str).ok_or_else(|| bad_url("invalid port number"))?
        } else {
            return Err(bad_url("unexpected characters after the IPv6 host"));
        };
        (h, p)
    } else if let Some((h, p_str)) = authority.rsplit_once(':') {
        let p = parse_port(p_str).ok_or_else(|| bad_url("invalid port number"))?;
        (h.to_ascii_lowercase(), p)
    } else {
        (authority.to_ascii_lowercase(), default_port)
    };

    // Only the characters of a DNS name or IPv4 literal may reach the allow-list match. A
    // backslash, tab, or other byte would let `evil.com\.allowed.com` satisfy `*.allowed.com`
    // here while a WHATWG-style client reads `\` as the end of the authority and dials `evil.com`.
    if !authority.starts_with('[')
        && !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return Err(bad_url(
            "host contains characters that are not valid in a hostname",
        ));
    }

    if host.is_empty() {
        return Err(GateError::InvalidUrl {
            url: raw_url.to_string(),
            reason: "empty host in URL".to_string(),
        });
    }
    Ok((scheme_lower, host, port))
}

/// A port is one to five ASCII digits. `u16::from_str` alone also accepts a leading `+`.
fn parse_port(raw: &str) -> Option<u16> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
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

/// Return `true` if `ip` is loopback, link-local, unspecified, multicast, reserved, or
/// RFC1918/ULA private, including an IPv4 address embedded in an IPv6 one.
///
/// Judged by the embedded IPv4 address (so a wrapper around a public address stays open):
/// mapped `::ffff:a.b.c.d`, compatible `::a.b.c.d`, NAT64 `64:ff9b::/96`, 6to4 `2002::/16`,
/// ISATAP (`<any /64>:0:5efe:a.b.c.d`, or `:200:5efe:` with the universal bit set), and Teredo
/// `2001::/32` (blocked when either the server address or the client address, which Teredo stores
/// XOR `0xffff`, is private).
///
/// Blocked outright, whatever they embed: NAT64 local-use `64:ff9b:1::/48` (RFC 8215, an operator
/// may map any address into it), the IPv4 6to4 relay anycast block `192.88.99.0/24`, site-local
/// `fec0::/10`, and the documentation range `2001:db8::/32`.
pub fn is_private_or_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            let embedded = |hi: u16, lo: u16| {
                let [a, b] = hi.to_be_bytes();
                let [c, d] = lo.to_be_bytes();
                is_private_v4(Ipv4Addr::new(a, b, c, d))
            };
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80
                || (seg[0] & 0xffc0) == 0xfec0
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)
                || v6.to_ipv4().is_some_and(is_private_v4)
                || (seg[0] == 0x0064
                    && seg[1] == 0xff9b
                    && seg[2..6] == [0; 4]
                    && embedded(seg[6], seg[7]))
                || (seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2] == 0x0001)
                || (seg[0] == 0x2002 && embedded(seg[1], seg[2]))
                || (seg[0] == 0x2001
                    && seg[1] == 0
                    && (embedded(seg[2], seg[3]) || embedded(seg[6] ^ 0xffff, seg[7] ^ 0xffff)))
                || ((seg[4] & 0xfdff) == 0 && seg[5] == 0x5efe && embedded(seg[6], seg[7]))
        }
    }
}

fn is_private_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_documentation()
        || v4.is_multicast()
        || o[0] == 0
        || o[0] >= 240
        || (o[0] == 100 && (o[1] & 0b1100_0000) == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 192 && o[1] == 88 && o[2] == 99)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
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
    // `localhost.` and `x.internal.` name the same hosts as their dotless forms.
    let lower = host.trim().trim_end_matches('.').to_ascii_lowercase();
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

struct PreparedSecrets {
    request: PreparedProxyRequest,
    /// Plaintext from a provider, not the local vault. Scrubbed on the way out.
    external_plaintexts: Vec<(String, String)>,
}

/// Validate all `{{FERROGATE:KEY}}` placeholders in `req` against `vault` policies and SSRF rules,
/// returning a [`PreparedProxyRequest`] ready for transport execution.
pub fn prepare_proxied_request(
    vault: &Vault,
    req: &ProxyRequest,
) -> Result<PreparedProxyRequest, GateError> {
    prepare_proxied_request_with_provider(vault, req, None)
}

/// [`prepare_proxied_request`] with an optional external secret provider.
///
/// A local hit does not call `provider`. Host, method, agent, and https checks use the
/// resolved metadata. SSRF still requires `vault.get_metadata` to allow a private network,
/// so an external `allow_private_network` flag cannot bypass it.
pub fn prepare_proxied_request_with_provider(
    vault: &Vault,
    req: &ProxyRequest,
    provider: Option<&dyn SecretProviderAdapter>,
) -> Result<PreparedProxyRequest, GateError> {
    Ok(prepare_proxied_request_inner(vault, req, provider)?.request)
}

fn prepare_proxied_request_inner(
    vault: &Vault,
    req: &ProxyRequest,
    provider: Option<&dyn SecretProviderAdapter>,
) -> Result<PreparedSecrets, GateError> {
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
    let mut external_plaintexts = Vec::new();
    let mut allow_private = false;

    for secret_name in &referenced {
        let (meta, secret_val) =
            resolve_vault_secret(vault, provider, secret_name, req.caller_agent.as_deref())?;
        if vault.get_metadata(secret_name).is_none() {
            external_plaintexts.push((secret_name.clone(), secret_val.clone()));
        }

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

    let replace_all = |input: &str| substitute_placeholders(input, &decrypted_map);

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

    Ok(PreparedSecrets {
        request: PreparedProxyRequest {
            method,
            url: final_url,
            host,
            headers: final_headers,
            body: final_body,
            injected_secrets,
        },
        external_plaintexts,
    })
}

/// Replace every `{{FERROGATE:KEY}}` in `input` with its value from `values`, in one pass.
///
/// Keys are normalized exactly as [`extract_placeholder_keys`] normalizes them, so a lowercase or
/// padded key is replaced like the policy check resolved it. A value is never rescanned, so a
/// secret whose text holds another placeholder is not expanded a second time.
fn substitute_placeholders(input: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find(PLACEHOLDER_PREFIX) {
        out.push_str(&rest[..start]);
        let after_prefix = &rest[start + PLACEHOLDER_PREFIX.len()..];
        let Some(end) = after_prefix.find(PLACEHOLDER_SUFFIX) else {
            out.push_str(&rest[start..]);
            return out;
        };
        let whole = start + PLACEHOLDER_PREFIX.len() + end + PLACEHOLDER_SUFFIX.len();
        match normalize_secret_name(&after_prefix[..end])
            .ok()
            .and_then(|key| values.get(&key))
        {
            Some(value) => out.push_str(value),
            None => out.push_str(&rest[start..whole]),
        }
        rest = &rest[whole..];
    }
    out.push_str(rest);
    out
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

/// Redact injected external plaintext of length >= 4. Vault values stay in [`scrub_secrets`].
fn redact_external_plaintexts(text: &str, external: &[(String, String)]) -> (String, Vec<String>) {
    let mut scrubbed = text.to_string();
    let mut matched = Vec::new();
    for (name, secret) in external {
        if secret.len() < 4 || !scrubbed.contains(secret) {
            continue;
        }
        let replacement = format!("[REDACTED:FERROGATE:{name}]");
        scrubbed = scrubbed.replace(secret, &replacement);
        if !matched.iter().any(|existing: &String| existing == name) {
            matched.push(name.clone());
        }
    }
    (scrubbed, matched)
}

fn scrub_with_external(
    vault: &Vault,
    text: &str,
    external: &[(String, String)],
) -> (String, BTreeSet<String>) {
    let (scrubbed, hits) = scrub_secrets(vault, text);
    let mut names: BTreeSet<String> = hits.into_iter().collect();
    let (scrubbed, extra) = redact_external_plaintexts(&scrubbed, external);
    names.extend(extra);
    (scrubbed, names)
}

/// Execute `req` through the zero-knowledge credential proxy (`prepare_proxied_request`),
/// disabling automatic redirect following so credentials are never forwarded cross-origin,
/// and scrubbing any secret values from the response body and headers.
pub fn execute_proxied_request(
    vault: &Vault,
    req: &ProxyRequest,
) -> Result<ProxyResponse, GateError> {
    execute_proxied_request_with_provider(vault, req, None)
}

/// [`execute_proxied_request`] with an optional external secret provider.
///
/// Response and upstream error text also redact injected external plaintext of length >= 4.
pub fn execute_proxied_request_with_provider(
    vault: &Vault,
    req: &ProxyRequest,
    provider: Option<&dyn SecretProviderAdapter>,
) -> Result<ProxyResponse, GateError> {
    let prepared_secrets = prepare_proxied_request_inner(vault, req, provider)?;
    let prepared = prepared_secrets.request;
    let external = prepared_secrets.external_plaintexts;
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
            // `prepared.url` holds injected plaintext in its query: report the request's own URL.
            return Err(GateError::InvalidUrl {
                url: req.url.clone(),
                reason: format!("unsupported HTTP method `{other}`"),
            });
        }
    };

    let mut resp = send_result.map_err(|e| {
        let (scrubbed_err, _) = scrub_with_external(vault, &e.to_string(), &external);
        GateError::UpstreamHttp {
            cause: scrubbed_err,
        }
    })?;

    let status = resp.status().as_u16();
    let mut resp_headers = BTreeMap::new();
    let mut scrubbed_names = BTreeSet::new();
    for (hk, hv) in resp.headers() {
        if let Ok(val_str) = hv.to_str() {
            let (clean_v, hits) = scrub_with_external(vault, val_str, &external);
            scrubbed_names.extend(hits);
            resp_headers.insert(hk.as_str().to_ascii_lowercase(), clean_v);
        }
    }

    // A body that is not UTF-8 is decoded lossily. A failed or over-limit read is an error, not
    // an empty `200` body.
    let raw_body = match resp.body_mut().read_to_vec() {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => {
            let (cause, _) = scrub_with_external(vault, &e.to_string(), &external);
            return Err(GateError::UpstreamHttp { cause });
        }
    };
    let (clean_body, body_hits) = scrub_with_external(vault, &raw_body, &external);
    scrubbed_names.extend(body_hits);

    Ok(ProxyResponse {
        status,
        headers: resp_headers,
        body: clean_body,
        scrubbed_secrets: scrubbed_names.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::SetSecretArgs;
    use tempfile::TempDir;

    fn vault_with(tmp: &TempDir, name: &str, value: &str, hosts: &[&str]) -> Vault {
        let mut vault = Vault::open(tmp.path().join("vault.enc"), [7u8; 32]).unwrap();
        vault
            .set(SetSecretArgs {
                name: name.into(),
                value: value.into(),
                allowed_hosts: hosts.iter().map(|h| (*h).to_string()).collect(),
                allow_private_network: true,
                ..Default::default()
            })
            .unwrap();
        vault
    }

    fn request(method: &str, url: &str, headers: &[(&str, &str)]) -> ProxyRequest {
        ProxyRequest {
            method: method.into(),
            url: url.into(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            body: None,
            caller_agent: None,
            timeout_secs: Some(5),
        }
    }

    #[test]
    fn authorities_that_clients_read_differently_are_refused() {
        for url in [
            "https://evil.com\\.allowed.com/x",
            "https://evil.com\t.allowed.com/x",
            "https://a b.com/",
            "https://[::1]evil/",
            "https://[::1%25eth0]/",
            "https://host:+80/",
            "https://host:80x/",
            "https://host:/",
        ] {
            assert!(
                matches!(
                    parse_url_scheme_host_port(url),
                    Err(GateError::InvalidUrl { .. })
                ),
                "{url} must be refused"
            );
        }
        assert_eq!(
            parse_url_scheme_host_port("https://[::1]:8443/x").unwrap(),
            ("https".into(), "::1".into(), 8443)
        );
        assert_eq!(
            parse_url_scheme_host_port("HTTP://Example.COM?q=1").unwrap(),
            ("http".into(), "example.com".into(), 80)
        );

        // The wildcard allow-list entry must not be satisfied by the backslash trick.
        let tmp = TempDir::new().unwrap();
        let vault = vault_with(&tmp, "API_KEY", "k-1234567890", &["*.allowed.com"]);
        let req = request(
            "GET",
            "https://evil.com\\.allowed.com/x",
            &[("X-Key", "{{FERROGATE:API_KEY}}")],
        );
        assert!(matches!(
            prepare_proxied_request(&vault, &req),
            Err(GateError::InvalidUrl { .. })
        ));
    }

    #[test]
    fn reserved_and_embedded_addresses_count_as_private() {
        for blocked in [
            "0.1.2.3",
            "240.0.0.1",
            "255.255.255.255",
            "192.0.0.8",
            "198.18.0.1",
            "198.19.255.255",
            "169.254.169.254",
            "::127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "2001:db8::1",
            "fec0::1",
            // 192.88.99.0/24 (6to4 relay anycast), bare and embedded.
            "192.88.99.1",
            "::ffff:192.88.99.1",
            "2002:c058:6301::",
            // NAT64 local-use /48: blocked outright, whatever it embeds.
            "64:ff9b:1::",
            "64:ff9b:1::808:808",
            "64:ff9b:1:ffff:ffff:ffff:ffff:ffff",
            // Teredo: private server address, or private client address (stored XOR 0xffff).
            "2001:0:7f00:1::1",
            "2001:0:a00:1::1",
            "2001:0:808:808:0:0:80ff:fffe",
            "2001:0:808:808:0:fdff:f5ff:fffe",
            // ISATAP, with and without the universal bit, under a private and a public prefix.
            "::5efe:127.0.0.1",
            "::5efe:10.1.2.3",
            "fe80::5efe:192.168.0.1",
            "2606:4700::5efe:7f00:1",
            "2606:4700::200:5efe:a9fe:a9fe",
        ] {
            let ip: IpAddr = blocked.parse().unwrap();
            assert!(is_private_or_loopback_ip(ip), "{blocked} must be blocked");
        }
        for open in [
            "8.8.8.8",
            "198.20.0.1",
            "2606:4700:4700::1111",
            "64:ff9b::808:808",
            "2002:808:808::",
            "192.88.98.1",
            "192.88.100.1",
            "64:ff9b:2::1",
            "2001:0:808:808:0:0:f7f7:f7f7",
            "2001:1::1",
            "::5efe:8.8.8.8",
            "2606:4700::200:5efe:808:808",
            "2606:4700::5efd:7f00:1",
        ] {
            let ip: IpAddr = open.parse().unwrap();
            assert!(!is_private_or_loopback_ip(ip), "{open} must stay open");
        }
    }

    #[test]
    fn a_trailing_dot_does_not_dodge_the_local_name_filter() {
        for host in [
            "localhost.",
            "app.localhost.",
            "metadata.google.internal.",
            "printer.local.",
        ] {
            assert!(
                matches!(
                    verify_host_ssrf(host, 443, false),
                    Err(GateError::SsrfBlocked { .. })
                ),
                "{host}"
            );
        }
    }

    #[test]
    fn placeholders_are_replaced_as_the_policy_check_read_them() {
        let tmp = TempDir::new().unwrap();
        let mut vault = vault_with(&tmp, "A_KEY", "x{{FERROGATE:B_KEY}}y", &["127.0.0.1"]);
        vault
            .set(SetSecretArgs {
                name: "B_KEY".into(),
                value: "BBBB-value".into(),
                allowed_hosts: vec!["127.0.0.1".into()],
                allow_private_network: true,
                ..Default::default()
            })
            .unwrap();

        // Lowercase and padded names resolve in the policy check, so they must be replaced too.
        let req = request(
            "GET",
            "http://127.0.0.1:9/p",
            &[
                ("X-One", "{{FERROGATE:b_key}}"),
                ("X-Two", "{{FERROGATE: B_KEY }}"),
            ],
        );
        let prepared = prepare_proxied_request(&vault, &req).unwrap();
        assert_eq!(prepared.headers["X-One"], "BBBB-value");
        assert_eq!(prepared.headers["X-Two"], "BBBB-value");

        // A secret's own text is data: it is not expanded a second time.
        let req = request(
            "GET",
            "http://127.0.0.1:9/p",
            &[("X-Both", "{{FERROGATE:A_KEY}}|{{FERROGATE:B_KEY}}")],
        );
        let prepared = prepare_proxied_request(&vault, &req).unwrap();
        assert_eq!(
            prepared.headers["X-Both"],
            "x{{FERROGATE:B_KEY}}y|BBBB-value"
        );
    }

    #[test]
    fn an_unsupported_method_error_never_prints_the_injected_secret() {
        let tmp = TempDir::new().unwrap();
        let vault = vault_with(&tmp, "API_KEY", "s3cr3t-value-123", &["127.0.0.1"]);
        let req = request(
            "OPTIONS",
            "http://127.0.0.1:9/p?key={{FERROGATE:API_KEY}}",
            &[],
        );
        let err = execute_proxied_request(&vault, &req).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, GateError::InvalidUrl { .. }), "{text}");
        assert!(!text.contains("s3cr3t-value-123"), "{text}");
    }

    #[test]
    fn a_non_utf8_response_body_is_decoded_not_dropped() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            let _ = req.respond(tiny_http::Response::from_data(b"ok \xff\xfe tail".to_vec()));
        });
        let tmp = TempDir::new().unwrap();
        let vault = vault_with(&tmp, "API_KEY", "s3cr3t-value-123", &["127.0.0.1"]);
        let req = request(
            "GET",
            &format!("http://127.0.0.1:{port}/"),
            &[("X-Key", "{{FERROGATE:API_KEY}}")],
        );
        let resp = execute_proxied_request(&vault, &req).unwrap();
        handle.join().unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.starts_with("ok "), "{:?}", resp.body);
        assert!(resp.body.ends_with(" tail"), "{:?}", resp.body);
    }

    #[test]
    fn debug_of_a_prepared_request_holds_no_plaintext() {
        let tmp = TempDir::new().unwrap();
        let vault = vault_with(&tmp, "API_KEY", "s3cr3t-value-123", &["127.0.0.1"]);
        let mut req = request(
            "POST",
            "http://127.0.0.1:9/p?key={{FERROGATE:API_KEY}}",
            &[("Authorization", "Bearer {{FERROGATE:API_KEY}}")],
        );
        req.body = Some("{\"k\":\"{{FERROGATE:API_KEY}}\"}".into());
        let prepared = prepare_proxied_request(&vault, &req).unwrap();
        assert!(prepared.url.contains("s3cr3t-value-123"));
        let shown = format!("{prepared:?}");
        assert!(!shown.contains("s3cr3t-value-123"), "{shown}");
        assert!(shown.contains("API_KEY"), "{shown}");
    }
}
