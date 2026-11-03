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
/// Legacy (uncontracted) response-body cap: 8 MiB.
pub const LEGACY_MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum number of response headers accepted before the body is read.
pub const MAX_HEADERS: usize = 100;
/// Maximum total response-header bytes (names + values) accepted.
pub const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Read at most `cap` bytes from `reader`; one byte over refuses without
/// buffering the full stream.
#[cfg(test)]
pub(crate) fn read_bounded(mut reader: impl std::io::Read, cap: u64) -> Result<Vec<u8>, GateError> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    reader
        .by_ref()
        .take(cap.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| GateError::UpstreamHttp {
            cause: e.to_string(),
        })?;
    if buf.len() as u64 > cap {
        return Err(GateError::UpstreamHttp {
            cause: format!("response body exceeds {cap} bytes"),
        });
    }
    Ok(buf)
}

/// Enforce header count/total-size caps before any body read.
pub(crate) fn validate_headers(
    headers: &[(String, String)],
    max_count: usize,
    max_bytes: usize,
) -> Result<(), GateError> {
    if headers.len() > max_count {
        return Err(GateError::UpstreamHttp {
            cause: format!(
                "response headers exceed {max_count} headers (got {})",
                headers.len()
            ),
        });
    }
    let total: usize = headers.iter().map(|(k, v)| k.len() + v.len()).sum();
    if total > max_bytes {
        return Err(GateError::UpstreamHttp {
            cause: format!("response headers exceed {max_bytes} bytes (got {total})"),
        });
    }
    Ok(())
}

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

/// What a refused request is audited with: filled in as the request is understood, so a denial
/// at any step names what was known by then. Names, methods and hosts only, never a value, a
/// path or a query.
#[derive(Default)]
struct DenialTrace {
    /// The method, once it is a plausible HTTP method.
    method: Option<String>,
    /// The destination host, once the URL parsed.
    host: Option<String>,
    /// Valid secret names referenced by the request (a malformed name is never logged).
    secrets: Vec<String>,
    /// The rule that refused, when the refusing code knows it better than the error variant.
    reason: Option<&'static str>,
}

impl DenialTrace {
    fn refuse<T>(&mut self, reason: &'static str, err: GateError) -> Result<T, GateError> {
        self.reason = Some(reason);
        Err(err)
    }
}

/// A method is audited only when it is 1 to 16 ASCII capitals.
fn audit_method(method: &str) -> String {
    if (1..=16).contains(&method.len()) && method.bytes().all(|b| b.is_ascii_uppercase()) {
        method.to_string()
    } else {
        "INVALID".to_string()
    }
}

/// The `denied:<reason>` outcome for `err`, preferring the rule the code named.
fn denial_reason(trace: &DenialTrace, err: &GateError) -> String {
    let reason = trace.reason.unwrap_or(match err {
        GateError::NoSuchSecret { .. } => "no_such_secret",
        GateError::InvalidSecret { .. } => "invalid_secret_name",
        GateError::PolicyDenied { reason, .. }
            if reason.starts_with(crate::vault::LEASE_EXPIRED_REASON) =>
        {
            "expired"
        }
        GateError::PolicyDenied { .. } => "policy",
        GateError::SsrfBlocked { .. } => "ssrf",
        GateError::InvalidPlaceholder { .. } => "placeholder",
        GateError::InvalidUrl { .. } => "invalid_url",
        GateError::Adapter { .. } => "provider_error",
        _ => "vault_error",
    });
    format!("denied:{reason}")
}

/// Every exit of a proxied request is audited: the success path writes `allowed` (below), and a
/// refusal writes `denied:<reason>` here before the error is returned. The audit write is best
/// effort and never changes the outcome: a denial stays a denial when the log cannot be written.
fn prepare_proxied_request_inner(
    vault: &Vault,
    req: &ProxyRequest,
    provider: Option<&dyn SecretProviderAdapter>,
) -> Result<PreparedSecrets, GateError> {
    let mut trace = DenialTrace::default();
    let result = prepare_checked(vault, req, provider, &mut trace);
    if let Err(err) = &result {
        let host: String = trace
            .host
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(255)
            .collect();
        let _ = vault.try_append_audit_log(
            req.caller_agent.as_deref(),
            &trace.secrets,
            trace.method.as_deref().unwrap_or("INVALID"),
            &host,
            &denial_reason(&trace, err),
        );
    }
    result
}

fn prepare_checked(
    vault: &Vault,
    req: &ProxyRequest,
    provider: Option<&dyn SecretProviderAdapter>,
    trace: &mut DenialTrace,
) -> Result<PreparedSecrets, GateError> {
    let method = req.method.trim().to_ascii_uppercase();
    if method.is_empty() {
        return trace.refuse(
            "method",
            GateError::InvalidUrl {
                url: req.url.clone(),
                reason: "HTTP method must not be empty".to_string(),
            },
        );
    }
    trace.method = Some(audit_method(&method));

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
        return trace.refuse(
            "placeholder",
            GateError::InvalidPlaceholder {
                reason: "secret placeholders are not permitted in the URL host or path before `?`"
                    .to_string(),
            },
        );
    }
    trace.secrets = referenced.iter().cloned().collect();

    let (scheme, host, port) = parse_url_scheme_host_port(&req.url)?;
    trace.host = Some(host.clone());

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
            return trace.refuse(
                "https_required",
                GateError::PolicyDenied {
                    secret: secret_name.clone(),
                    reason: format!(
                        "secret `{secret_name}` requires an `https://` destination unless `allow_private_network = true`"
                    ),
                },
            );
        }

        if meta.allowed_hosts.is_empty()
            || !meta
                .allowed_hosts
                .iter()
                .any(|pat| host_matches_pattern(&host, pat))
        {
            return trace.refuse(
                "host",
                GateError::PolicyDenied {
                    secret: secret_name.clone(),
                    reason: format!(
                        "destination host `{host}` is not in allowed_hosts {:?}",
                        meta.allowed_hosts
                    ),
                },
            );
        }

        if !meta.allowed_methods.is_empty()
            && !meta
                .allowed_methods
                .iter()
                .any(|m| m.eq_ignore_ascii_case(&method))
        {
            return trace.refuse(
                "method",
                GateError::PolicyDenied {
                    secret: secret_name.clone(),
                    reason: format!(
                        "HTTP method `{method}` is not in allowed_methods {:?}",
                        meta.allowed_methods
                    ),
                },
            );
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
                return trace.refuse(
                    "agent",
                    GateError::PolicyDenied {
                        secret: secret_name.clone(),
                        reason: format!(
                            "caller agent `{caller}` is not in allowed_agents {:?}",
                            meta.allowed_agents
                        ),
                    },
                );
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

/// The outbound client's settings. No proxy: `ureq` reads `HTTP_PROXY`, `HTTPS_PROXY` and
/// `ALL_PROXY` (and `NO_PROXY`) from the environment by default, which would send the request,
/// injected secret and all, to whatever process the environment names. No redirects either: a
/// credential must not follow one to another origin. An error status is an answer, not an error.
fn agent_config(timeout: Duration) -> ureq::config::Config {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .build()
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

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = String::with_capacity(digest.as_ref().len() * 2);
    for b in digest.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Contracted dispatch: read the wire body with `contract.max_response_bytes`
/// as the cap and decode it strictly as UTF-8. Oversize or malformed UTF-8
/// returns a typed contract error carrying only a digest/byte count, never
/// the body; on oversize that digest/count describe only the read prefix
/// (`cap + 1` bytes), not the full body.
pub fn execute_proxied_request_with_contract(
    vault: &Vault,
    req: &ProxyRequest,
    contract: &crate::contract::ResponseContract,
) -> Result<ProxyResponse, GateError> {
    execute_proxied_request_with_provider_and_contract(vault, req, None, Some(contract))
}

/// [`execute_proxied_request_with_contract`] with an optional provider.
pub fn execute_proxied_request_with_provider_and_contract(
    vault: &Vault,
    req: &ProxyRequest,
    provider: Option<&dyn SecretProviderAdapter>,
    contract: Option<&crate::contract::ResponseContract>,
) -> Result<ProxyResponse, GateError> {
    let (cap, strict) = match contract {
        Some(c) => (c.max_response_bytes, true),
        None => (LEGACY_MAX_RESPONSE_BYTES, false),
    };
    execute_proxied_request_inner(vault, req, provider, cap, strict)
}

/// [`execute_proxied_request`] with an optional external secret provider.
///
/// Response and upstream error text also redact injected external plaintext of length >= 4.
pub fn execute_proxied_request_with_provider(
    vault: &Vault,
    req: &ProxyRequest,
    provider: Option<&dyn SecretProviderAdapter>,
) -> Result<ProxyResponse, GateError> {
    execute_proxied_request_inner(vault, req, provider, LEGACY_MAX_RESPONSE_BYTES, false)
}

fn execute_proxied_request_inner(
    vault: &Vault,
    req: &ProxyRequest,
    provider: Option<&dyn SecretProviderAdapter>,
    cap: u64,
    strict_utf8: bool,
) -> Result<ProxyResponse, GateError> {
    let prepared_secrets = prepare_proxied_request_inner(vault, req, provider)?;
    let prepared = prepared_secrets.request;
    let external = prepared_secrets.external_plaintexts;
    let timeout = Duration::from_secs(req.timeout_secs.unwrap_or(30).clamp(1, 300));
    // After the `allowed` entry: what became of the request. Best effort, like every audit write.
    let audit_after_send = |outcome: &str| {
        if !prepared.injected_secrets.is_empty() {
            vault.append_audit_log(
                req.caller_agent.as_deref(),
                &prepared.injected_secrets,
                &audit_method(&prepared.method),
                &prepared.host,
                outcome,
            );
        }
    };

    let agent: ureq::Agent = agent_config(timeout).into();

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
            audit_after_send("denied:unsupported_method");
            // `prepared.url` holds injected plaintext in its query: report the request's own URL.
            return Err(GateError::InvalidUrl {
                url: req.url.clone(),
                reason: format!("unsupported HTTP method `{other}`"),
            });
        }
    };

    let mut resp = send_result.map_err(|e| {
        audit_after_send("error:upstream");
        let (scrubbed_err, _) = scrub_with_external(vault, &e.to_string(), &external);
        GateError::UpstreamHttp {
            cause: scrubbed_err,
        }
    })?;

    let status = resp.status().as_u16();
    if (300..400).contains(&status) {
        // Redirects are never followed (the credential would go with them); the 3xx is returned
        // to the caller as it is, and the refusal to follow it is on record.
        audit_after_send("redirect_not_followed");
    }
    // Header bounds are enforced before the body is read.
    let raw_header_pairs: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(hk, hv)| {
            (
                hk.as_str().to_string(),
                String::from_utf8_lossy(hv.as_bytes()).into_owned(),
            )
        })
        .collect();
    if let Err(e) = validate_headers(&raw_header_pairs, MAX_HEADERS, MAX_HEADER_BYTES) {
        let GateError::UpstreamHttp { cause } = e else {
            return Err(e);
        };
        let (cause, _) = scrub_with_external(vault, &cause, &external);
        return Err(GateError::UpstreamHttp { cause });
    }
    let mut resp_headers = BTreeMap::new();
    let mut scrubbed_names = BTreeSet::new();
    for (hk, hv) in resp.headers() {
        if let Ok(val_str) = hv.to_str() {
            let (clean_v, hits) = scrub_with_external(vault, val_str, &external);
            scrubbed_names.extend(hits);
            resp_headers.insert(hk.as_str().to_ascii_lowercase(), clean_v);
        }
    }

    // Legacy path caps at LEGACY_MAX and decodes lossily. The contracted
    // path caps at the contract bound and decodes strictly: oversize and
    // malformed UTF-8 become typed contract errors (digest/bytes only).
    let raw_bytes = {
        use std::io::Read as _;
        let mut buf = Vec::new();
        let read = resp
            .body_mut()
            .as_reader()
            .by_ref()
            .take(cap.saturating_add(1))
            .read_to_end(&mut buf);
        match read {
            Ok(_) if buf.len() as u64 > cap => {
                if strict_utf8 {
                    return Err(GateError::ContractResponseTooLarge {
                        prefix_sha256: sha256_hex(&buf),
                        read_bytes: buf.len() as u64,
                        max_bytes: cap,
                        status,
                    });
                }
                let cause = format!("response body exceeds {cap} bytes");
                let (cause, _) = scrub_with_external(vault, &cause, &external);
                return Err(GateError::UpstreamHttp { cause });
            }
            Ok(_) => buf,
            Err(e) => {
                let (cause, _) = scrub_with_external(vault, &e.to_string(), &external);
                return Err(GateError::UpstreamHttp { cause });
            }
        }
    };
    let raw_body = if strict_utf8 {
        match String::from_utf8(raw_bytes) {
            Ok(s) => s,
            Err(e) => {
                let bytes = e.into_bytes();
                return Err(GateError::ContractInvalidUtf8 {
                    body_sha256: sha256_hex(&bytes),
                    body_bytes: bytes.len() as u64,
                    status,
                });
            }
        }
    } else {
        String::from_utf8_lossy(&raw_bytes).into_owned()
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

    #[test]
    fn contracted_wire_cap_and_strict_utf8_are_typed() {
        use crate::contract::{ResponseContract, ResponseSchema};
        let tmp = TempDir::new().unwrap();
        let vault = vault_with(&tmp, "K", "v123456", &["127.0.0.1"]);
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok(req) = server.recv() {
                let _ = req.respond(tiny_http::Response::from_data(vec![b'x'; 2048]));
            }
            if let Ok(req) = server.recv() {
                let _ = req.respond(tiny_http::Response::from_data(vec![0xff, 0xfe, 0xfd]));
            }
        });
        let contract = ResponseContract {
            schema: ResponseSchema::from_value(serde_json::json!({"type":"string"})).unwrap(),
            max_response_bytes: 64,
            max_response_tokens: None,
            max_depth: 32,
            max_array_len: 10,
            max_string_len: 10000,
        };
        let req = request(
            "GET",
            &format!("http://127.0.0.1:{port}/a"),
            &[("Authorization", "Bearer {{FERROGATE:K}}")],
        );
        let err = execute_proxied_request_with_contract(&vault, &req, &contract).unwrap_err();
        match &err {
            GateError::ContractResponseTooLarge {
                prefix_sha256,
                read_bytes,
                max_bytes,
                ..
            } => {
                assert_eq!(*read_bytes, 65);
                assert_eq!(*max_bytes, 64);
                assert_eq!(*prefix_sha256, sha256_hex(&[b'x'; 65]));
                assert_ne!(*prefix_sha256, sha256_hex(&[b'x'; 2048]));
            }
            other => panic!("expected prefix oversize error, got {other}"),
        }
        // Digest/count label the read prefix (cap+1 = 65 bytes), not the
        // full 2048-byte body.
        assert!(err.to_string().contains("read_prefix 65 bytes"), "{err}");
        assert!(err.to_string().contains("prefix_sha256"), "{err}");
        assert!(!err.to_string().contains("xxxx"), "{err}");
        let mut big = contract.clone();
        big.max_response_bytes = 100000;
        let err = execute_proxied_request_with_contract(&vault, &req, &big).unwrap_err();
        assert!(
            matches!(err, GateError::ContractInvalidUtf8 { body_bytes: 3, .. }),
            "{err}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn oversize_body_refused_before_full_buffer() {
        let data = vec![b'x'; 2048];
        let err = read_bounded(std::io::Cursor::new(data), 1024).unwrap_err();
        assert!(err.to_string().contains("response body exceeds"), "{err}");
        let ok = read_bounded(std::io::Cursor::new(vec![b'x'; 1024]), 1024).unwrap();
        assert_eq!(ok.len(), 1024);
    }

    #[test]
    fn header_bounds_enforced() {
        let many: Vec<(String, String)> = (0..101)
            .map(|i| (format!("x-h-{i}"), "v".to_string()))
            .collect();
        assert!(validate_headers(&many, MAX_HEADERS, MAX_HEADER_BYTES).is_err());
        let big = vec![("x-big".to_string(), "v".repeat(MAX_HEADER_BYTES + 1))];
        assert!(validate_headers(&big, MAX_HEADERS, MAX_HEADER_BYTES).is_err());
        let ok = vec![("x-a".to_string(), "v".to_string())];
        assert!(validate_headers(&ok, MAX_HEADERS, MAX_HEADER_BYTES).is_ok());
    }

    #[test]
    fn the_outbound_client_has_no_proxy_and_follows_no_redirect() {
        let config = agent_config(Duration::from_secs(1));
        assert!(
            config.proxy().is_none(),
            "an ambient HTTP(S)_PROXY / ALL_PROXY must not be read"
        );
        assert_eq!(config.max_redirects(), 0);
    }
}
