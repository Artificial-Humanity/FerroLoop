//! Cloud sync and edge gate adapters (Records 52 and 53): the saved configuration, the endpoint
//! URL rules, and the concrete HTTP adapter types.
//!
//! The configuration lives at `<home>/.config/fl/cloud-adapters.json` (mode 0600). Unlike the
//! unified settings it never falls back silently: a file that cannot be read, parsed or
//! validated is an error, and the file stays untouched.
//!
//! The adapters are types only. Nothing builds one from the saved configuration yet, and none
//! is installed on a store, a request handler or the registry route.
//!
//! # URL rules
//!
//! An endpoint URL is empty (adapter off) or `scheme://host[:port][/path]`: ASCII only, at most
//! [`MAX_URL_LEN`] bytes, no userinfo, query or fragment. The scheme is `https`, or `http` for
//! exactly `127.0.0.1`, `localhost` and `[::1]`. An IP literal must be a canonical dotted quad
//! or a bracketed IPv6 address and must be public ([`is_forbidden_ip`]); an `https` literal may
//! not be loopback either. Any other numeric host (`127.1`, `2130706433`, `0x7f000001`,
//! `0177.0.0.1`) is refused outright. `https://localhost` is allowed. A hostname cannot be
//! checked for private resolution when the URL is saved, so [`CloudHttp`] checks it at connect
//! time instead.
//!
//! # Connect-time SSRF guard
//!
//! [`CloudHttp`] hands its `ureq` agent a [`GuardedResolver`]. ureq resolves the host through
//! that resolver and connects only to the addresses it returns, so the check and the connect see
//! the same answer (no second lookup to race, TLS still uses the URL host for SNI and
//! certificate checks). The resolver refuses the request when ANY resolved address is forbidden.
//! A loopback endpoint (`localhost`, `127.0.0.1`, `[::1]`) is the one exception, and then EVERY
//! address must be loopback.
//!
//! # Auth
//!
//! None in this version. A bearer-token hook is future work; no header or secret from the
//! configuration appears in an error or a log line.

use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use fl_core::StoreError;
use fl_gate::{GateError, McpRegistryAdapter, McpRelayAdapter, RegistryTier};
use fl_store::{RecordEnvelope, SyncAdapter, SyncBatch};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

/// Cloud providers a config may name. `none` is the off position.
pub const PROVIDERS: [&str; 4] = ["none", "gcp", "aws", "azure"];
/// Store sync modes, mirroring `fl_store::SyncMode`.
pub const SYNC_MODES: [&str; 4] = [
    "write_through",
    "write_behind",
    "local_cache",
    "cloud_pre_cache",
];
/// Longest endpoint URL accepted.
pub const MAX_URL_LEN: usize = 2048;
/// Largest config file the hub reads back.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// What GET and PUT answer when the saved file cannot be used. Fixed on purpose: no path, no
/// parser detail.
pub const UNREADABLE_MESSAGE: &str =
    "the saved cloud adapter settings are unreadable or invalid; fix or remove the file";

/// Serialises every config read-modify-write.
static CONFIG_LOCK: Mutex<()> = Mutex::new(());
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `<home>/.config/fl/cloud-adapters.json`.
pub fn config_path(home: &Path) -> PathBuf {
    home.join(".config/fl/cloud-adapters.json")
}

// --- configuration ---------------------------------------------------------------------------

/// Record 52: where the store replicates to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StoreSyncConfig {
    pub provider: String,
    pub sync_mode: String,
    pub endpoint_url: String,
    pub enabled: bool,
}

impl Default for StoreSyncConfig {
    fn default() -> Self {
        Self {
            provider: "none".into(),
            sync_mode: "local_cache".into(),
            endpoint_url: String::new(),
            enabled: false,
        }
    }
}

/// Record 53: the cloud side of the gate. `endpoint_url` is the registry endpoint;
/// `relay_endpoint_url` is the relay's. `endpoint_url` and `enabled` are optional on the wire
/// (the UI type has them) and always written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateCloudConfig {
    pub registry_provider: String,
    pub relay_endpoint_url: String,
    pub secret_manager_enabled: bool,
    pub model_armor_enabled: bool,
    pub endpoint_url: String,
    pub enabled: bool,
}

impl Default for GateCloudConfig {
    fn default() -> Self {
        Self {
            registry_provider: "none".into(),
            relay_endpoint_url: String::new(),
            secret_manager_enabled: false,
            model_armor_enabled: false,
            endpoint_url: String::new(),
            enabled: false,
        }
    }
}

/// The whole file. Read accepts `gate_registry` for `gate_cloud`; write always says `gate_cloud`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudAdaptersConfig {
    pub store_sync: StoreSyncConfig,
    #[serde(alias = "gate_registry")]
    pub gate_cloud: GateCloudConfig,
}

/// A `PUT /api/v1/cloud/adapters` body: any subset of the groups and of their fields. Unknown
/// fields are rejected at every level.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudAdaptersPatch {
    pub store_sync: Option<StoreSyncPatch>,
    #[serde(alias = "gate_registry")]
    pub gate_cloud: Option<GateCloudPatch>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreSyncPatch {
    pub provider: Option<String>,
    pub sync_mode: Option<String>,
    pub endpoint_url: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateCloudPatch {
    pub registry_provider: Option<String>,
    pub relay_endpoint_url: Option<String>,
    pub secret_manager_enabled: Option<bool>,
    pub model_armor_enabled: Option<bool>,
    pub endpoint_url: Option<String>,
    pub enabled: Option<bool>,
}

fn set<T>(slot: &mut T, value: Option<T>) {
    if let Some(v) = value {
        *slot = v;
    }
}

fn check_choice(field: &str, value: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(format!("{field} must be one of: {}", allowed.join(", ")))
}

fn check_url(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Ok(());
    }
    validate_endpoint_url(value)
        .map(|_| ())
        .map_err(|why| format!("{field}: {why}"))
}

impl CloudAdaptersConfig {
    /// Every enum and URL in the config is acceptable. The first failure wins.
    pub fn validate(&self) -> Result<(), String> {
        let s = &self.store_sync;
        check_choice("store_sync.provider", &s.provider, &PROVIDERS)?;
        check_choice("store_sync.sync_mode", &s.sync_mode, &SYNC_MODES)?;
        check_url("store_sync.endpoint_url", &s.endpoint_url)?;
        let g = &self.gate_cloud;
        check_choice(
            "gate_cloud.registry_provider",
            &g.registry_provider,
            &PROVIDERS,
        )?;
        check_url("gate_cloud.relay_endpoint_url", &g.relay_endpoint_url)?;
        check_url("gate_cloud.endpoint_url", &g.endpoint_url)?;
        Ok(())
    }

    /// Apply `patch` on top of `self` and validate the MERGED result. Fails, changing nothing.
    pub fn merged(&self, patch: CloudAdaptersPatch) -> Result<Self, String> {
        let mut next = self.clone();
        if let Some(p) = patch.store_sync {
            let s = &mut next.store_sync;
            set(&mut s.provider, p.provider);
            set(&mut s.sync_mode, p.sync_mode);
            set(&mut s.endpoint_url, p.endpoint_url);
            set(&mut s.enabled, p.enabled);
        }
        if let Some(p) = patch.gate_cloud {
            let g = &mut next.gate_cloud;
            set(&mut g.registry_provider, p.registry_provider);
            set(&mut g.relay_endpoint_url, p.relay_endpoint_url);
            set(&mut g.secret_manager_enabled, p.secret_manager_enabled);
            set(&mut g.model_armor_enabled, p.model_armor_enabled);
            set(&mut g.endpoint_url, p.endpoint_url);
            set(&mut g.enabled, p.enabled);
        }
        next.validate()?;
        Ok(next)
    }
}

/// Why the saved file cannot be used: it exists but cannot be read, parsed or validated.
#[derive(Debug, PartialEq, Eq)]
pub struct Unreadable;

/// The saved config, the defaults when there is no file, or [`Unreadable`]. A file with a bad
/// value is as unusable as one with bad JSON: nothing served from here is ever unvalidated.
pub fn load(home: &Path) -> Result<CloudAdaptersConfig, Unreadable> {
    let path = config_path(home);
    match fs::metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(CloudAdaptersConfig::default()),
        Err(_) => return Err(Unreadable),
        Ok(meta) if !meta.is_file() || meta.len() > MAX_FILE_BYTES => return Err(Unreadable),
        Ok(_) => {}
    }
    let bytes = fs::read(&path).map_err(|_| Unreadable)?;
    let config: CloudAdaptersConfig = serde_json::from_slice(&bytes).map_err(|_| Unreadable)?;
    config.validate().map_err(|_| Unreadable)?;
    Ok(config)
}

/// Write through a 0600 temp file in the same directory, then rename over the old file.
fn save(home: &Path, config: &CloudAdaptersConfig) -> io::Result<()> {
    let path = config_path(home);
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("config path has no parent"))?;
    fs::create_dir_all(dir)?;
    let bytes = serde_json::to_vec_pretty(config).map_err(io::Error::other)?;
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".cloud-adapters.json.tmp-{}-{n}",
        std::process::id()
    ));
    let result = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(&bytes).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&tmp, &path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Why `update` did not apply a patch.
#[derive(Debug)]
pub enum UpdateError {
    /// The merged config is invalid (client error).
    Invalid(String),
    /// The saved file cannot be read; nothing was changed.
    Unreadable,
    /// The merged config could not be written.
    Io(io::Error),
}

/// Merge `patch` into the saved config and persist the result, under one lock. Nothing is
/// written unless the merged config validates.
pub fn update(home: &Path, patch: CloudAdaptersPatch) -> Result<CloudAdaptersConfig, UpdateError> {
    let _guard = CONFIG_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let current = load(home).map_err(|Unreadable| UpdateError::Unreadable)?;
    let next = current.merged(patch).map_err(UpdateError::Invalid)?;
    save(home, &next).map_err(UpdateError::Io)?;
    Ok(next)
}

// --- endpoint URLs ---------------------------------------------------------------------------

/// A URL that passed [`validate_endpoint_url`], split into the parts the client needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointUrl {
    https: bool,
    /// Lower-cased. An IPv6 literal keeps its brackets.
    host: String,
    port: Option<u16>,
    /// Empty, or starts with `/` and has no trailing `/`.
    path: String,
    loopback_ok: bool,
}

impl EndpointUrl {
    pub fn is_https(&self) -> bool {
        self.https
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// True when the connect-time check admits loopback addresses (and then admits ONLY those).
    pub fn loopback_ok(&self) -> bool {
        self.loopback_ok
    }

    /// `scheme://host[:port][/path]` with no trailing slash: what a request path is appended to.
    pub fn base(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        format!("{scheme}://{}{port}{}", self.host, self.path)
    }

    /// `host[:port]`, for messages. Never the path.
    pub fn authority(&self) -> String {
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        format!("{}{port}", self.host)
    }
}

/// Check an endpoint URL against the rules in the module docs. Pure: no DNS.
pub fn validate_endpoint_url(raw: &str) -> Result<EndpointUrl, String> {
    if raw.len() > MAX_URL_LEN {
        return Err(format!("URL is longer than {MAX_URL_LEN} bytes"));
    }
    if !raw.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("URL must be ASCII with no whitespace or control characters".into());
    }
    if raw.contains('\\') {
        return Err("URL must not contain a backslash".into());
    }
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or("URL must look like scheme://host[:port][/path]")?;
    let https = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return Err("scheme must be https (http only for loopback)".into()),
    };
    if rest.contains('#') {
        return Err("URL must not have a fragment".into());
    }
    if rest.contains('?') {
        return Err("URL must not have a query string".into());
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    if authority.contains('@') {
        return Err("URL must not carry userinfo".into());
    }
    let (host, port) = split_host_port(authority)?;
    let host = host.to_ascii_lowercase();
    let path = check_path(path)?;

    let literal = host_ip_literal(&host)?;
    if let Some(ip) = literal {
        if !https && !matches!(host.as_str(), "127.0.0.1" | "[::1]") {
            return Err("http is only allowed for 127.0.0.1, localhost and [::1]".into());
        }
        if https && is_forbidden_ip(ip) {
            return Err(
                "host is a private, loopback, link-local or otherwise non-public address".into(),
            );
        }
    } else {
        check_hostname(&host)?;
        if !https && host != "localhost" {
            return Err("http is only allowed for 127.0.0.1, localhost and [::1]".into());
        }
    }
    let loopback_ok = matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]");
    Ok(EndpointUrl {
        https,
        host,
        port,
        path,
        loopback_ok,
    })
}

fn split_host_port(authority: &str) -> Result<(&str, Option<u16>), String> {
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']').ok_or("unterminated [ in host")?;
        let (host, after) = authority.split_at(end + 1);
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':').ok_or("junk after the IPv6 host")?),
        };
        (host, port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err("URL has no host".into());
    }
    let port = match port {
        None => None,
        Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            match p.parse::<u16>() {
                Ok(n) if n != 0 => Some(n),
                _ => return Err("port must be 1-65535".into()),
            }
        }
        Some(_) => return Err("port must be 1-65535".into()),
    };
    Ok((host, port))
}

/// Path chars are the RFC 3986 `pchar` set plus `/`; a `%` must start a valid escape.
fn check_path(path: &str) -> Result<String, String> {
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let ok = b.is_ascii_alphanumeric()
            || b"-._~!$&'()*+,;=:@/".contains(&b)
            || (b == b'%'
                && bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit));
        if !ok {
            return Err("URL path has a character that is not allowed".into());
        }
        i += 1;
    }
    Ok(path.trim_end_matches('/').to_string())
}

/// `Some(ip)` for a bracketed IPv6 literal or a canonical dotted quad. A host that LOOKS
/// numeric but is not canonical is refused: those forms (`127.1`, `0x7f000001`, `0177.0.0.1`,
/// `2130706433`) are how a loopback or private address is smuggled past a string check.
fn host_ip_literal(host: &str) -> Result<Option<IpAddr>, String> {
    if let Some(inner) = host.strip_prefix('[') {
        let inner = inner.strip_suffix(']').ok_or("malformed IPv6 host")?;
        let ip: Ipv6Addr = inner.parse().map_err(|_| "malformed IPv6 host")?;
        return Ok(Some(IpAddr::V6(ip)));
    }
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(Some(IpAddr::V4(ip)));
    }
    let last = host.rsplit('.').next().unwrap_or(host);
    let decimal = !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit());
    let hex = last.len() > 2
        && last[..2].eq_ignore_ascii_case("0x")
        && last[2..].bytes().all(|b| b.is_ascii_hexdigit());
    if decimal || hex {
        return Err("numeric host must be a canonical dotted-quad IPv4 address".into());
    }
    Ok(None)
}

/// LDH hostname: labels of 1-63 letters, digits or hyphens, not starting or ending with a
/// hyphen, at most 253 bytes, no trailing dot. Non-ASCII names must come as punycode.
fn check_hostname(host: &str) -> Result<(), String> {
    let bad = || Err("host is not a valid hostname".to_string());
    if host.len() > 253 {
        return bad();
    }
    for label in host.split('.') {
        let edge_ok = !label.starts_with('-') && !label.ends_with('-');
        if label.is_empty()
            || label.len() > 63
            || !edge_ok
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return bad();
        }
    }
    Ok(())
}

/// True for any address a cloud endpoint must never reach: unspecified, loopback, private,
/// link-local (including the 169.254.169.254 and fd00:ec2::254 metadata endpoints), the Azure
/// WireServer 168.63.129.16, CGNAT,
/// documentation, benchmarking, unique-local, multicast, broadcast and reserved space, plus
/// IPv6 forms that carry an IPv4 address (v4-mapped, v4-translated, NAT64, 6to4), which are judged by the
/// address inside. Loopback is forbidden here; a caller that admits it does so itself.
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_forbidden(v4),
        IpAddr::V6(v6) => v6_forbidden(v6),
    }
}

/// Azure's WireServer: a public-looking address every Azure VM reaches for host services (VM
/// extensions, provisioning, certificates), outside any private range.
const AZURE_WIRESERVER: Ipv4Addr = Ipv4Addr::new(168, 63, 129, 16);

fn v4_forbidden(ip: Ipv4Addr) -> bool {
    if ip == AZURE_WIRESERVER {
        return true;
    }
    let [a, b, c, _] = ip.octets();
    matches!(
        (a, b, c),
        (0 | 10 | 127, ..)
            | (100, 64..=127, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 0, 0 | 2)
            | (192, 88, 99)
            | (192, 168, _)
            | (198, 18 | 19, _)
            | (198, 51, 100)
            | (203, 0, 113)
            | (224..=255, ..)
    )
}

fn v6_forbidden(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_forbidden(v4);
    }
    let s = ip.segments();
    let embedded = |hi: u16, lo: u16| {
        let [a, b] = hi.to_be_bytes();
        let [c, d] = lo.to_be_bytes();
        v4_forbidden(Ipv4Addr::new(a, b, c, d))
    };
    // ::/96 holds `::`, `::1` and the deprecated IPv4-compatible form.
    if s[..6].iter().all(|&x| x == 0) {
        return true;
    }
    // ::ffff:0:0:0/96 is the IPv4-translated (SIIT) range: judged by the address inside.
    if s[..4].iter().all(|&x| x == 0) && s[4] == 0xffff && s[5] == 0 {
        return embedded(s[6], s[7]);
    }
    if ip.is_multicast() {
        return true;
    }
    match s[0] {
        // 64:ff9b::/96 NAT64 carries an IPv4 address; 64:ff9b:1::/48 is local-use.
        0x64 if s[1] == 0xff9b => {
            s[2] == 1 || (s[2..6].iter().all(|&x| x == 0) && embedded(s[6], s[7]))
        }
        // 100::/64 discard-only.
        0x100 => s[1..4].iter().all(|&x| x == 0),
        // 2001::/23 IETF assignments (Teredo, benchmarking, ORCHID) and 2001:db8::/32.
        0x2001 => s[1] < 0x200 || s[1] == 0xdb8,
        // 2002::/16 6to4 carries an IPv4 address.
        0x2002 => embedded(s[1], s[2]),
        // 3fff::/20 documentation.
        0x3fff => s[1] < 0x1000,
        // fc00::/7 unique-local, fe80::/10 link-local, fec0::/10 site-local.
        x => (x & 0xfe00) == 0xfc00 || (x & 0xffc0) == 0xfe80 || (x & 0xffc0) == 0xfec0,
    }
}

/// The connect-time rule. Every address must pass: a loopback endpoint admits loopback ONLY,
/// anything else admits no forbidden address. One bad answer among good ones refuses the lot.
fn check_resolved(addrs: &[IpAddr], loopback_ok: bool) -> Result<(), String> {
    if addrs.is_empty() {
        return Err("host did not resolve".into());
    }
    for ip in addrs {
        let ip = ip.to_canonical();
        if loopback_ok {
            if !ip.is_loopback() {
                return Err("a loopback endpoint resolved to a non-loopback address".into());
            }
        } else if is_forbidden_ip(ip) {
            return Err("host resolved to a non-public address".into());
        }
    }
    Ok(())
}

// --- the shared HTTP client ------------------------------------------------------------------

/// A config problem found when an adapter is built.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CloudConfigError(pub String);

/// Maps a hostname to addresses. The seam tests use instead of real DNS.
pub trait HostLookup: std::fmt::Debug + Send + Sync + 'static {
    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<IpAddr>>;
}

/// The operating system's resolver.
#[derive(Debug)]
struct SystemLookup;

impl HostLookup for SystemLookup {
    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<IpAddr>> {
        Ok((host, port)
            .to_socket_addrs()?
            .map(|addr| addr.ip())
            .collect())
    }
}

/// Timeouts and size caps of one [`CloudHttp`].
#[derive(Debug, Clone, Copy)]
pub struct HttpLimits {
    pub connect: Duration,
    /// Waiting for the response headers, and for each stretch of the body.
    pub read: Duration,
    /// The whole call, resolve to last body byte.
    pub total: Duration,
    pub max_request_bytes: u64,
    pub max_response_bytes: u64,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            read: Duration::from_secs(10),
            total: Duration::from_secs(15),
            max_request_bytes: 4 << 20,
            max_response_bytes: 8 << 20,
        }
    }
}

/// Why a [`CloudHttp`] call failed. Never carries the URL path, headers or a response body.
#[derive(Debug, PartialEq, Eq)]
pub enum CloudHttpError {
    /// The connect-time SSRF guard refused the host.
    Blocked { host: String, reason: String },
    /// Connect, TLS, I/O or timeout.
    Transport(String),
    /// A non-2xx answer.
    Status(u16),
    /// A 3xx answer. Redirects are never followed.
    Redirect(u16),
    /// The answer is larger than the response cap.
    ResponseTooLarge { limit: u64 },
    /// The request is larger than the request cap; nothing was sent.
    RequestTooLarge { limit: u64 },
    /// The answer is not what the protocol says.
    Protocol(String),
}

impl std::fmt::Display for CloudHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blocked { host, reason } => {
                write!(f, "SSRF guard blocked host `{host}`: {reason}")
            }
            Self::Transport(cause) => write!(f, "cloud endpoint call failed: {cause}"),
            Self::Status(code) => write!(f, "cloud endpoint answered HTTP {code}"),
            Self::Redirect(code) => {
                write!(
                    f,
                    "cloud endpoint answered a redirect (HTTP {code}), which is never followed"
                )
            }
            Self::ResponseTooLarge { limit } => {
                write!(f, "cloud endpoint answer is larger than {limit} bytes")
            }
            Self::RequestTooLarge { limit } => {
                write!(f, "request is larger than {limit} bytes; nothing was sent")
            }
            Self::Protocol(why) => write!(f, "cloud endpoint broke the protocol: {why}"),
        }
    }
}

impl CloudHttpError {
    /// Server trouble or no answer at all: the call may be worth repeating.
    fn is_unreachable(&self) -> bool {
        matches!(self, Self::Transport(_)) || matches!(self, Self::Status(code) if *code >= 500)
    }

    fn into_store(self, label: &str) -> StoreError {
        if self.is_unreachable() {
            return StoreError::Unreachable {
                store: label.to_string(),
                cause: self.to_string(),
            };
        }
        StoreError::Backend(format!("{label}: {self}"))
    }

    fn into_gate(self, provider: &str) -> GateError {
        match self {
            Self::Blocked { host, reason } => GateError::SsrfBlocked { host, reason },
            Self::Protocol(reason) => GateError::Adapter {
                provider: provider.to_string(),
                reason,
            },
            other => GateError::UpstreamHttp {
                cause: other.to_string(),
            },
        }
    }
}

/// The error a [`GuardedResolver`] hands to ureq, recognisable on the way back.
#[derive(Debug)]
struct SsrfRefusal {
    host: String,
    reason: String,
}

impl std::fmt::Display for SsrfRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SSRF guard blocked host `{}`: {}",
            self.host, self.reason
        )
    }
}

impl std::error::Error for SsrfRefusal {}

/// Resolves for ureq and refuses a forbidden answer. See the module docs.
#[derive(Debug)]
struct GuardedResolver {
    /// The one host this client talks to.
    host: String,
    loopback_ok: bool,
    lookup: Arc<dyn HostLookup>,
    timeout: Duration,
}

impl GuardedResolver {
    fn refuse(&self, reason: &str) -> ureq::Error {
        ureq::Error::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            SsrfRefusal {
                host: self.host.clone(),
                reason: reason.to_string(),
            },
        ))
    }

    /// The lookup runs on its own thread so a stuck resolver cannot outlast `timeout`.
    fn lookup_within(
        &self,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<Vec<IpAddr>, ureq::Error> {
        let (tx, rx) = mpsc::sync_channel(1);
        let lookup = Arc::clone(&self.lookup);
        let name = host.to_string();
        std::thread::spawn(move || tx.send(lookup.lookup(&name, port)).ok());
        match rx.recv_timeout(timeout) {
            Ok(Ok(addrs)) => Ok(addrs),
            Ok(Err(e)) => Err(ureq::Error::Io(e)),
            Err(_) => Err(ureq::Error::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "host lookup timed out",
            ))),
        }
    }
}

impl Resolver for GuardedResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let authority = uri.authority().ok_or(ureq::Error::HostNotFound)?;
        let host = authority.host().to_ascii_lowercase();
        // Redirects are off, so this only fires if that ever changes: the guard was built for
        // one host and must not vouch for another.
        if host != self.host {
            return Err(self.refuse("request is for a different host than the configured endpoint"));
        }
        let port = match (authority.port_u16(), uri.scheme_str()) {
            (Some(port), _) => port,
            (None, Some("https")) => 443,
            (None, Some("http")) => 80,
            _ => return Err(ureq::Error::HostNotFound),
        };
        let literal = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>();
        let ips = match literal {
            Ok(ip) => vec![ip],
            Err(_) => {
                let budget = if timeout.after.is_not_happening() {
                    self.timeout
                } else {
                    (*timeout.after).min(self.timeout)
                };
                self.lookup_within(&host, port, budget)?
            }
        };
        check_resolved(&ips, self.loopback_ok).map_err(|reason| self.refuse(&reason))?;
        let mut out = self.empty();
        for ip in ips.into_iter().take(16) {
            out.push(SocketAddr::new(ip, port));
        }
        Ok(out)
    }
}

/// One reply the client accepted: a 2xx, with its JSON body when it had one.
#[derive(Debug)]
pub struct HttpReply {
    pub status: u16,
    pub body: Option<Value>,
}

#[derive(Clone, Copy)]
enum Method {
    Get,
    Post,
}

/// The hardened HTTP client the three adapters share: bounded timeouts, no redirects, no proxy,
/// no cookies or ambient credentials, capped request and response bodies, and the connect-time
/// SSRF guard.
#[derive(Debug, Clone)]
pub struct CloudHttp {
    agent: ureq::Agent,
    url: EndpointUrl,
    base: String,
    limits: HttpLimits,
}

impl CloudHttp {
    /// Validate `url` (it must be non-empty) and build a client that resolves through the OS.
    pub fn new(url: &str) -> Result<Self, CloudConfigError> {
        Self::with_lookup(url, Arc::new(SystemLookup), HttpLimits::default())
    }

    /// As [`Self::new`], with the resolver and the limits chosen by the caller.
    pub fn with_lookup(
        url: &str,
        lookup: Arc<dyn HostLookup>,
        limits: HttpLimits,
    ) -> Result<Self, CloudConfigError> {
        let url = validate_endpoint_url(url).map_err(CloudConfigError)?;
        let config = ureq::config::Config::builder()
            .http_status_as_error(false)
            .max_redirects(0)
            // ⚠ A proxy would resolve and connect for us and sidestep the guard.
            .proxy(None)
            .timeout_resolve(Some(limits.connect))
            .timeout_connect(Some(limits.connect))
            .timeout_send_request(Some(limits.read))
            .timeout_send_body(Some(limits.read))
            .timeout_recv_response(Some(limits.read))
            .timeout_recv_body(Some(limits.read))
            .timeout_global(Some(limits.total))
            .user_agent(concat!("fl-hub/", env!("CARGO_PKG_VERSION")))
            .build();
        let resolver = GuardedResolver {
            host: url.host().to_string(),
            loopback_ok: url.loopback_ok(),
            lookup,
            timeout: limits.connect,
        };
        let agent = ureq::Agent::with_parts(config, DefaultConnector::new(), resolver);
        Ok(Self {
            agent,
            base: url.base(),
            url,
            limits,
        })
    }

    /// `host[:port]` of the endpoint, for messages.
    pub fn label(&self) -> String {
        self.url.authority()
    }

    /// `GET {base}{path}`.
    pub fn get(&self, path: &str) -> Result<HttpReply, CloudHttpError> {
        self.request(Method::Get, path, None)
    }

    /// `POST {base}{path}` with a JSON body.
    pub fn post(&self, path: &str, body: &Value) -> Result<HttpReply, CloudHttpError> {
        self.request(Method::Post, path, Some(body))
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<HttpReply, CloudHttpError> {
        let payload = match body {
            Some(value) => {
                let bytes = serde_json::to_vec(value)
                    .map_err(|e| CloudHttpError::Protocol(format!("request is not JSON: {e}")))?;
                if bytes.len() as u64 > self.limits.max_request_bytes {
                    return Err(CloudHttpError::RequestTooLarge {
                        limit: self.limits.max_request_bytes,
                    });
                }
                Some(bytes)
            }
            None => None,
        };
        let url = format!("{}{path}", self.base);
        let sent = match (method, &payload) {
            (Method::Get, _) => self
                .agent
                .get(&url)
                .header("Accept", "application/json")
                .call(),
            (Method::Post, body) => {
                let request = self
                    .agent
                    .post(&url)
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json");
                match body {
                    Some(bytes) => request.send(bytes),
                    None => request.send_empty(),
                }
            }
        };
        let mut resp = sent.map_err(classify)?;
        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            return Err(CloudHttpError::Redirect(status));
        }
        if !(200..300).contains(&status) {
            return Err(CloudHttpError::Status(status));
        }
        let limit = self.limits.max_response_bytes;
        // A declared length over the cap is refused before a single body byte is read.
        if resp.body().content_length().is_some_and(|len| len > limit) {
            return Err(CloudHttpError::ResponseTooLarge { limit });
        }
        if status == 204 {
            return Ok(HttpReply { status, body: None });
        }
        // ureq's own limit counts the wire and trips at `limit + 1` bytes; `take` counts the
        // decoded bytes, so a compressed bomb is stopped too. At most `limit + 1` bytes are
        // ever pulled, and a body of exactly `limit` is accepted.
        let mut bytes = Vec::new();
        let read = resp
            .body_mut()
            .with_config()
            .limit(limit + 1)
            .reader()
            .take(limit + 1)
            .read_to_end(&mut bytes);
        if let Err(e) = read {
            return Err(match ureq::Error::from(e) {
                ureq::Error::BodyExceedsLimit(_) => CloudHttpError::ResponseTooLarge { limit },
                other => classify(other),
            });
        }
        if bytes.len() as u64 > limit {
            return Err(CloudHttpError::ResponseTooLarge { limit });
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(HttpReply { status, body: None });
        }
        let value = serde_json::from_slice(&bytes)
            .map_err(|_| CloudHttpError::Protocol("answer is not JSON".into()))?;
        Ok(HttpReply {
            status,
            body: Some(value),
        })
    }
}

fn classify(err: ureq::Error) -> CloudHttpError {
    match err {
        ureq::Error::Io(e) => match e
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<SsrfRefusal>())
        {
            Some(refusal) => CloudHttpError::Blocked {
                host: refusal.host.clone(),
                reason: refusal.reason.clone(),
            },
            None => CloudHttpError::Transport(format!("io: {e}")),
        },
        // The message of a bad URI would carry the URL path.
        ureq::Error::BadUri(_) => CloudHttpError::Transport("bad request URI".into()),
        other => CloudHttpError::Transport(other.to_string()),
    }
}

/// True when `segment` cannot be one path segment of a request: empty, `.` or `..`. The last
/// two survive percent-encoding unchanged, and a normalising server resolves them to another
/// route, so they are refused before any request is built.
fn bad_segment(segment: &str) -> bool {
    matches!(segment, "" | "." | "..")
}

/// One path segment, percent-encoded: everything but the RFC 3986 unreserved set.
fn pct_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// --- store sync adapter (Record 52) ----------------------------------------------------------

const SYNC_BATCHES: &str = "/v1/sync/batches";

/// `SyncAdapter` over a small JSON-over-HTTP protocol. A batch travels as its `RecordEnvelope`
/// frames, base64 inside JSON; every response is JSON. Search is left to the trait default.
///
/// | call | request | success answer |
/// |---|---|---|
/// | `on_commit` | `POST /v1/sync/batches` `{"seq":N,"records":["<b64 FLEN frame>",..]}` | any 2xx |
/// | `pull_since` | `GET /v1/sync/batches?after_seq=N` | `{"batches":[{"seq":N,"records":[..]},..]}`, `seq` ascending and above N |
/// | `fetch_document` | `GET /v1/sync/documents/{collection}/{id}` (segments percent-encoded) | `{"doc":{..}}`; HTTP 404 or `"doc":null` is `Ok(None)` |
/// | `prewarm` | `POST /v1/sync/prewarm` `{"collections":[..]}` | `{"batches":[..]}` |
#[derive(Debug)]
pub struct HttpCloudSyncAdapter {
    name: String,
    http: CloudHttp,
}

impl HttpCloudSyncAdapter {
    /// An adapter for `url`, which is validated here: it can never be built with a rejected URL.
    pub fn new(url: &str) -> Result<Self, CloudConfigError> {
        Ok(Self::with_http(CloudHttp::new(url)?))
    }

    /// An adapter over an already built client.
    pub fn with_http(http: CloudHttp) -> Self {
        Self {
            name: "http-cloud-sync".into(),
            http,
        }
    }

    /// `Ok(None)` when sync is off (not enabled, or provider `none`). An enabled config with no
    /// URL is an error, not a silent off.
    pub fn from_config(config: &StoreSyncConfig) -> Result<Option<Self>, CloudConfigError> {
        if !config.enabled || config.provider == "none" {
            return Ok(None);
        }
        check_choice("store_sync.provider", &config.provider, &PROVIDERS)
            .map_err(CloudConfigError)?;
        if config.endpoint_url.is_empty() {
            return Err(CloudConfigError(
                "store_sync is enabled but endpoint_url is empty".into(),
            ));
        }
        let mut adapter = Self::new(&config.endpoint_url)?;
        adapter.name = format!("http-cloud-sync:{}", config.provider);
        Ok(Some(adapter))
    }

    fn fail(&self, err: CloudHttpError) -> StoreError {
        err.into_store(&format!("cloud sync ({})", self.http.label()))
    }

    fn bad(&self, why: impl std::fmt::Display) -> StoreError {
        StoreError::Backend(format!("cloud sync ({}): {why}", self.http.label()))
    }
}

fn encode_batch(batch: &SyncBatch) -> Result<Value, StoreError> {
    let mut records = Vec::new();
    for envelope in batch.to_envelopes()? {
        records.push(Value::String(BASE64.encode(envelope.encode()?)));
    }
    Ok(json!({ "seq": batch.seq, "records": records }))
}

fn decode_batch(value: &Value) -> Result<SyncBatch, String> {
    let seq = value
        .get("seq")
        .and_then(Value::as_u64)
        .ok_or("a batch has no numeric `seq`")?;
    let records = value
        .get("records")
        .and_then(Value::as_array)
        .ok_or("a batch has no `records` array")?;
    let mut envelopes = Vec::with_capacity(records.len());
    for record in records {
        let text = record.as_str().ok_or("a record is not a string")?;
        let frame = BASE64.decode(text).map_err(|_| "a record is not base64")?;
        envelopes.push(RecordEnvelope::decode(&frame).map_err(|e| e.to_string())?);
    }
    SyncBatch::from_envelopes(seq, &envelopes).map_err(|e| e.to_string())
}

/// The `batches` array of an answer. With `after_seq`, every `seq` must be above it and the
/// list strictly ascending, so a server cannot replay or reorder what the store already has.
fn decode_batches(body: Option<Value>, after_seq: Option<u64>) -> Result<Vec<SyncBatch>, String> {
    let body = body.ok_or("the answer is empty")?;
    let items = body
        .get("batches")
        .and_then(Value::as_array)
        .ok_or("the answer has no `batches` array")?;
    let mut batches: Vec<SyncBatch> = Vec::with_capacity(items.len());
    for item in items {
        let batch = decode_batch(item)?;
        if let Some(after) = after_seq {
            let floor = batches.last().map_or(after, |last| last.seq);
            if batch.seq <= floor {
                return Err("batches are not strictly ascending above `after_seq`".into());
            }
        }
        batches.push(batch);
    }
    Ok(batches)
}

impl SyncAdapter for HttpCloudSyncAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_commit(&self, batch: &SyncBatch) -> Result<(), StoreError> {
        let body = encode_batch(batch)?;
        self.http
            .post(SYNC_BATCHES, &body)
            .map(|_| ())
            .map_err(|e| self.fail(e))
    }

    fn pull_since(&self, after_seq: u64) -> Result<Vec<SyncBatch>, StoreError> {
        let reply = self
            .http
            .get(&format!("{SYNC_BATCHES}?after_seq={after_seq}"))
            .map_err(|e| self.fail(e))?;
        decode_batches(reply.body, Some(after_seq)).map_err(|why| self.bad(why))
    }

    fn fetch_document(&self, collection: &str, id: &str) -> Result<Option<Value>, StoreError> {
        if bad_segment(collection) || bad_segment(id) {
            return Err(self.bad("collection and id must not be empty, `.` or `..`"));
        }
        let path = format!(
            "/v1/sync/documents/{}/{}",
            pct_segment(collection),
            pct_segment(id)
        );
        match self.http.get(&path) {
            Ok(reply) => Ok(reply
                .body
                .and_then(|mut body| body.get_mut("doc").map(Value::take))
                .filter(|doc| !doc.is_null())),
            Err(CloudHttpError::Status(404)) => Ok(None),
            Err(e) => Err(self.fail(e)),
        }
    }

    fn prewarm(&self, collections: &[&str]) -> Result<Vec<SyncBatch>, StoreError> {
        let reply = self
            .http
            .post("/v1/sync/prewarm", &json!({ "collections": collections }))
            .map_err(|e| self.fail(e))?;
        decode_batches(reply.body, None).map_err(|why| self.bad(why))
    }
}

// --- gate adapters (Record 53) ---------------------------------------------------------------

fn gate_provider(provider: &str) -> Result<(), CloudConfigError> {
    match provider {
        "gcp" | "aws" | "azure" => Ok(()),
        _ => Err(CloudConfigError(
            "gate_cloud.registry_provider must be one of: gcp, aws, azure".into(),
        )),
    }
}

/// Whether the gate's cloud side is on at all: enabled, with a real provider.
fn gate_on(config: &GateCloudConfig) -> bool {
    config.enabled && config.registry_provider != "none"
}

/// `McpRegistryAdapter` for a cloud catalog, in the `Cloud` tier.
///
/// `list_servers` is `GET /v0.1/servers` answering `{"servers":[..]}`. `publish_servers` is
/// `POST /v0.1/servers` with `{"servers":[..]}`; the count published is the answer's
/// `{"published":N}`, else the number sent.
#[derive(Debug)]
pub struct HttpCloudRegistryAdapter {
    provider: String,
    http: CloudHttp,
}

impl HttpCloudRegistryAdapter {
    /// An adapter for `provider` (gcp, aws or azure) and `url`, both validated here.
    pub fn new(provider: &str, url: &str) -> Result<Self, CloudConfigError> {
        gate_provider(provider)?;
        Ok(Self::with_http(provider, CloudHttp::new(url)?))
    }

    /// An adapter over an already built client.
    pub fn with_http(provider: &str, http: CloudHttp) -> Self {
        Self {
            provider: provider.to_string(),
            http,
        }
    }

    /// `Ok(None)` when the gate's cloud side is off, or no registry `endpoint_url` is set (the
    /// relay may be configured without one). The URL comes from `gate_cloud.endpoint_url`.
    pub fn from_config(config: &GateCloudConfig) -> Result<Option<Self>, CloudConfigError> {
        if !gate_on(config) || config.endpoint_url.is_empty() {
            return Ok(None);
        }
        Self::new(&config.registry_provider, &config.endpoint_url).map(Some)
    }

    fn fail(&self, err: CloudHttpError) -> GateError {
        err.into_gate(&self.provider)
    }
}

impl McpRegistryAdapter for HttpCloudRegistryAdapter {
    fn provider_name(&self) -> &str {
        &self.provider
    }

    fn tier(&self) -> RegistryTier {
        RegistryTier::CloudProvider {
            provider: self.provider.clone(),
        }
    }

    fn list_servers(&self) -> Result<Vec<Value>, GateError> {
        let reply = self.http.get("/v0.1/servers").map_err(|e| self.fail(e))?;
        reply
            .body
            .and_then(|mut body| match body.get_mut("servers").map(Value::take) {
                Some(Value::Array(servers)) => Some(servers),
                _ => None,
            })
            .ok_or_else(|| {
                self.fail(CloudHttpError::Protocol(
                    "the answer has no `servers` array".into(),
                ))
            })
    }

    fn publish_servers(&self, entries: &[Value]) -> Result<usize, GateError> {
        let reply = self
            .http
            .post("/v0.1/servers", &json!({ "servers": entries }))
            .map_err(|e| self.fail(e))?;
        let reported = reply
            .body
            .as_ref()
            .and_then(|body| body.get("published"))
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok());
        // The server's count is its claim, not ours: it can never exceed what was sent.
        Ok(reported.map_or(entries.len(), |n| n.min(entries.len())))
    }
}

/// `McpRelayAdapter`: `forward_jsonrpc` is `POST /v1/relay/{connector_id}` (percent-encoded)
/// with the JSON-RPC request as the body. A 204 or empty answer is `Ok(None)`: the request was
/// a notification, or the connector had nothing to say.
#[derive(Debug)]
pub struct HttpCloudRelayAdapter {
    provider: String,
    http: CloudHttp,
}

impl HttpCloudRelayAdapter {
    /// An adapter for `provider` (gcp, aws or azure) and the relay `url`, both validated here.
    pub fn new(provider: &str, url: &str) -> Result<Self, CloudConfigError> {
        gate_provider(provider)?;
        Ok(Self::with_http(provider, CloudHttp::new(url)?))
    }

    /// An adapter over an already built client.
    pub fn with_http(provider: &str, http: CloudHttp) -> Self {
        Self {
            provider: provider.to_string(),
            http,
        }
    }

    /// `Ok(None)` when the gate's cloud side is off, or no `relay_endpoint_url` is set.
    pub fn from_config(config: &GateCloudConfig) -> Result<Option<Self>, CloudConfigError> {
        if !gate_on(config) || config.relay_endpoint_url.is_empty() {
            return Ok(None);
        }
        Self::new(&config.registry_provider, &config.relay_endpoint_url).map(Some)
    }
}

impl McpRelayAdapter for HttpCloudRelayAdapter {
    fn provider_name(&self) -> &str {
        &self.provider
    }

    fn forward_jsonrpc(
        &self,
        connector_id: &str,
        request: &Value,
    ) -> Result<Option<Value>, GateError> {
        if bad_segment(connector_id) {
            return Err(GateError::Adapter {
                provider: self.provider.clone(),
                reason: "connector id must not be empty, `.` or `..`".into(),
            });
        }
        let path = format!("/v1/relay/{}", pct_segment(connector_id));
        self.http
            .post(&path, request)
            .map(|reply| reply.body)
            .map_err(|e| e.into_gate(&self.provider))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_store::SyncOp;
    use std::collections::HashMap;
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;
    use tempfile::TempDir;

    // --- URL validation ----------------------------------------------------------------------

    #[test]
    fn endpoint_urls_that_are_accepted() {
        for ok in [
            "https://example.com",
            "https://example.com/",
            "HTTPS://EXAMPLE.com",
            "https://api.example.com:8443/v1/base/",
            "https://8.8.8.8",
            "https://[2606:4700::1111]",
            "https://[::ffff:8.8.8.8]",
            "https://[::ffff:0:808:808]",
            "https://localhost",
            "https://127.0.0.1.evil.com",
            "http://127.0.0.1",
            "http://127.0.0.1:8080",
            "http://localhost",
            "http://localhost:3000/x",
            "http://[::1]:9000",
        ] {
            assert!(validate_endpoint_url(ok).is_ok(), "{ok}");
        }
        let url = validate_endpoint_url("HTTPS://Api.Example.com:8443/v1/base//").unwrap();
        assert_eq!(url.base(), "https://api.example.com:8443/v1/base");
        assert_eq!(url.authority(), "api.example.com:8443");
        assert!(!url.loopback_ok());
        assert!(
            validate_endpoint_url("http://127.0.0.1:1")
                .unwrap()
                .loopback_ok()
        );
        assert!(
            validate_endpoint_url("https://localhost")
                .unwrap()
                .loopback_ok()
        );
    }

    #[test]
    fn endpoint_urls_that_are_rejected() {
        let overlong = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        let at_limit = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN - 20));
        assert!(validate_endpoint_url(&at_limit).is_ok());
        for bad in [
            "",
            "example.com",
            "//example.com",
            "http://example.com",
            "http://10.0.0.1",
            "http://8.8.8.8",
            "http://[::2]",
            "http://localhost.evil.com",
            "http://127.0.0.1.evil.com",
            "ftp://example.com",
            "file:///etc/passwd",
            "https://user:pass@example.com",
            "https://user@example.com",
            "https://example.com@evil.com",
            "https://169.254.169.254",
            "https://169.254.169.254/latest/meta-data",
            "https://10.0.0.5",
            "https://192.168.1.1",
            "https://172.16.0.1",
            "https://100.64.0.1",
            "https://0.0.0.0",
            "https://224.0.0.1",
            "https://255.255.255.255",
            "https://198.18.0.1",
            "https://203.0.113.9",
            "https://127.0.0.1",
            "https://[::1]",
            "https://[::]",
            "https://[fd00::1]",
            "https://[fd00:ec2::254]",
            "https://[fe80::1]",
            "https://[::ffff:10.0.0.1]",
            "https://[::ffff:a00:1]",
            "https://[::ffff:0:7f00:1]",
            "https://[::ffff:0:a00:1]",
            "https://[::ffff:127.0.0.1]",
            "https://[2001:db8::1]",
            "https://[ff02::1]",
            "https://0x7f000001",
            "https://2130706433",
            "https://127.1",
            "https://0177.0.0.1",
            "https://0x7f.0.0.1",
            "https://1.2.3",
            "https://10.0.0.5.",
            "https:///path",
            "https://",
            "https://:443",
            "https://[::1",
            "https://[::1]x",
            "https://example.com#frag",
            "https://example.com/a#x",
            "https://example.com/?q=1",
            "https://example.com?q=1",
            "https://exa mple.com",
            "https://example.com/a b",
            "https://example.com/\n",
            "https://example.com\t/",
            "https://example.com/\\evil",
            "https://ex\u{e9}mple.com",
            "https://example.com/%zz",
            "https://example.com/%4",
            "https://example.com:0",
            "https://example.com:99999",
            "https://example.com:",
            "https://example.com:80x",
            "https://example.com./",
            "https://exa_mple.com",
            "https://-bad.example.com",
            "https://a..example.com",
            overlong.as_str(),
        ] {
            assert!(
                validate_endpoint_url(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn forbidden_ip_table() {
        let forbidden = [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "10.255.255.255",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.169.254",
            "169.254.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "100.64.0.1",
            "100.127.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.0.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::2",
            "::a00:1",
            "ff02::1",
            "fe80::1",
            "fec0::1",
            "fc00::1",
            "fd00::1",
            "fd00:ec2::254",
            "2001:db8::1",
            "2001::1",
            "2001:2::1",
            "3fff::1",
            "100::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:0:7f00:1",
            "::ffff:0:a00:1",
            "::ffff:0:a9fe:a9fe",
            "64:ff9b::a00:1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::1",
            "2002:a00:1::",
            "2002:7f00:1::1",
        ];
        for text in forbidden {
            let ip: IpAddr = text.parse().unwrap();
            assert!(is_forbidden_ip(ip), "{text} must be forbidden");
        }
        let public = [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "11.0.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.0.1.1",
            "198.17.255.255",
            "198.20.0.1",
            "223.255.255.255",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "2a00:1450:4001::1",
            "::ffff:8.8.8.8",
            "::ffff:0:808:808",
            "64:ff9b::808:808",
            "2002:808:808::",
        ];
        for text in public {
            let ip: IpAddr = text.parse().unwrap();
            assert!(!is_forbidden_ip(ip), "{text} must be allowed");
        }
    }

    #[test]
    fn the_connect_time_rule_judges_every_resolved_address() {
        let ips =
            |list: &[&str]| -> Vec<IpAddr> { list.iter().map(|t| t.parse().unwrap()).collect() };
        for (list, loopback_ok, ok) in [
            (&["8.8.8.8"][..], false, true),
            (&["8.8.8.8", "2606:4700::1111"], false, true),
            (&["8.8.8.8", "10.0.0.1"], false, false),
            (&["10.0.0.1", "8.8.8.8"], false, false),
            (&["127.0.0.1"], false, false),
            (&["::1"], false, false),
            (&["::ffff:127.0.0.1"], false, false),
            (&["169.254.169.254"], false, false),
            (&[], false, false),
            (&["127.0.0.1"], true, true),
            (&["127.0.0.1", "::1"], true, true),
            (&["::ffff:127.0.0.1"], true, true),
            (&["127.0.0.1", "8.8.8.8"], true, false),
            (&["10.0.0.1"], true, false),
            (&["8.8.8.8"], true, false),
            (&[], true, false),
        ] {
            assert_eq!(
                check_resolved(&ips(list), loopback_ok).is_ok(),
                ok,
                "{list:?} loopback_ok={loopback_ok}"
            );
        }
    }

    // --- configuration -----------------------------------------------------------------------

    fn default_config_json() -> Value {
        json!({
            "store_sync": {
                "provider": "none",
                "sync_mode": "local_cache",
                "endpoint_url": "",
                "enabled": false,
            },
            "gate_cloud": {
                "registry_provider": "none",
                "relay_endpoint_url": "",
                "secret_manager_enabled": false,
                "model_armor_enabled": false,
                "endpoint_url": "",
                "enabled": false,
            },
        })
    }

    fn patch(v: Value) -> CloudAdaptersPatch {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn defaults_match_the_contract_and_a_missing_file_loads_them() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            serde_json::to_value(CloudAdaptersConfig::default()).unwrap(),
            default_config_json()
        );
        assert_eq!(load(tmp.path()).unwrap(), CloudAdaptersConfig::default());
        assert!(!config_path(tmp.path()).exists(), "reading never writes");
    }

    #[test]
    fn missing_keys_fill_from_defaults_and_the_alias_reads_but_never_writes() {
        let tmp = TempDir::new().unwrap();
        let path = config_path(tmp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"store_sync":{"enabled":true},"gate_registry":{"registry_provider":"gcp"}}"#,
        )
        .unwrap();
        let loaded = load(tmp.path()).unwrap();
        assert!(loaded.store_sync.enabled);
        assert_eq!(loaded.store_sync.sync_mode, "local_cache");
        assert_eq!(loaded.gate_cloud.registry_provider, "gcp");
        assert!(!loaded.gate_cloud.model_armor_enabled);

        let saved = update(
            tmp.path(),
            patch(json!({ "gate_registry": { "enabled": true } })),
        )
        .unwrap();
        assert!(saved.gate_cloud.enabled);
        let on_disk: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(on_disk.get("gate_registry").is_none());
        assert_eq!(on_disk["gate_cloud"]["registry_provider"], "gcp");
        assert_eq!(on_disk["gate_cloud"]["enabled"], true);
        assert_eq!(on_disk["gate_cloud"]["endpoint_url"], "");
        // Both spellings in one body is an ambiguity, not a merge.
        assert!(
            serde_json::from_value::<CloudAdaptersPatch>(
                json!({ "gate_cloud": {}, "gate_registry": {} })
            )
            .is_err()
        );
    }

    #[test]
    fn update_merges_persists_with_mode_0600_and_survives_a_reload() {
        let tmp = TempDir::new().unwrap();
        let first = update(
            tmp.path(),
            patch(json!({ "store_sync": { "provider": "gcp", "endpoint_url": "https://sync.example.com/api" } })),
        )
        .unwrap();
        assert_eq!(first.store_sync.provider, "gcp");
        assert_eq!(first.store_sync.sync_mode, "local_cache");
        let second = update(
            tmp.path(),
            patch(json!({
                "store_sync": { "sync_mode": "write_behind", "enabled": true },
                "gate_cloud": { "relay_endpoint_url": "http://127.0.0.1:9", "model_armor_enabled": true },
            })),
        )
        .unwrap();
        assert_eq!(
            second.store_sync.endpoint_url,
            "https://sync.example.com/api"
        );
        assert_eq!(second.store_sync.sync_mode, "write_behind");
        assert!(second.gate_cloud.model_armor_enabled);
        assert_eq!(load(tmp.path()).unwrap(), second);
        let mode = fs::metadata(config_path(tmp.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let strays: Vec<_> = fs::read_dir(config_path(tmp.path()).parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(strays.len(), 1, "no temp file is left behind: {strays:?}");
        // Provider none with the switches on is stored as asked.
        let stored = update(
            tmp.path(),
            patch(
                json!({ "store_sync": { "provider": "none", "enabled": true },
                          "gate_cloud": { "secret_manager_enabled": true } }),
            ),
        )
        .unwrap();
        assert!(stored.store_sync.enabled && stored.gate_cloud.secret_manager_enabled);
    }

    #[test]
    fn a_bad_patch_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let ok = update(
            tmp.path(),
            patch(json!({ "store_sync": { "provider": "aws" } })),
        )
        .unwrap();
        let before = fs::read(config_path(tmp.path())).unwrap();
        for bad in [
            json!({ "store_sync": { "provider": "ibm" } }),
            json!({ "store_sync": { "sync_mode": "write_sometimes" } }),
            json!({ "gate_cloud": { "registry_provider": "GCP" } }),
            json!({ "store_sync": { "endpoint_url": "http://example.com" } }),
            json!({ "store_sync": { "endpoint_url": "https://10.0.0.5" } }),
            json!({ "gate_cloud": { "relay_endpoint_url": "https://169.254.169.254/x" } }),
            json!({ "gate_cloud": { "endpoint_url": "https://user:pw@example.com" } }),
            // A valid field beside an invalid one is not applied.
            json!({ "store_sync": { "enabled": true }, "gate_cloud": { "endpoint_url": "ftp://x" } }),
        ] {
            let res = update(tmp.path(), patch(bad.clone()));
            assert!(matches!(res, Err(UpdateError::Invalid(_))), "{bad}");
            assert_eq!(fs::read(config_path(tmp.path())).unwrap(), before, "{bad}");
        }
        assert_eq!(load(tmp.path()).unwrap(), ok);
        for bad in [
            json!({ "colour": 1 }),
            json!({ "store_sync": { "bogus": 1 } }),
            json!({ "gate_cloud": { "enabled": "yes" } }),
            json!({ "store_sync": { "provider": 3 } }),
        ] {
            assert!(
                serde_json::from_value::<CloudAdaptersPatch>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn an_unusable_file_is_never_reset_or_overwritten() {
        let tmp = TempDir::new().unwrap();
        let path = config_path(tmp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for content in [
            &b"{ not json"[..],
            b"[1,2]",
            b"\0garbage",
            br#"{"store_sync":{"enabled":"yes"}}"#,
            // Parses, but holds a value the PUT would have refused.
            br#"{"store_sync":{"endpoint_url":"https://10.0.0.5"}}"#,
            br#"{"gate_cloud":{"registry_provider":"ibm"}}"#,
        ] {
            fs::write(&path, content).unwrap();
            assert_eq!(load(tmp.path()), Err(Unreadable));
            let res = update(
                tmp.path(),
                patch(json!({ "store_sync": { "enabled": true } })),
            );
            assert!(matches!(res, Err(UpdateError::Unreadable)));
            assert_eq!(fs::read(&path).unwrap(), content);
        }
        // Not a regular file at all.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(load(tmp.path()), Err(Unreadable));
        fs::remove_dir(&path).unwrap();
        fs::write(&path, vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert_eq!(load(tmp.path()), Err(Unreadable));
    }

    // --- fake HTTP server --------------------------------------------------------------------

    #[derive(Clone)]
    struct Req {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Req {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap()
        }
    }

    struct FakeServer {
        port: u16,
        connections: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<Req>>>,
    }

    fn read_request(stream: &mut TcpStream) -> Option<Req> {
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).ok()?;
            buf.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&buf).into_owned();
        let mut lines = head.lines();
        let mut first = lines.next()?.split(' ');
        let method = first.next()?.to_string();
        let path = first.next()?.to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        let len = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).ok()?;
        Some(Req {
            method,
            path,
            headers,
            body,
        })
    }

    fn reply(stream: &mut TcpStream, status: u16, headers: &[(&str, &str)], body: &[u8]) {
        let mut head = format!("HTTP/1.1 {status} X\r\nConnection: close\r\n");
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        if status != 204
            && !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
    }

    fn reply_json(stream: &mut TcpStream, status: u16, body: &Value) {
        reply(
            stream,
            status,
            &[("Content-Type", "application/json")],
            body.to_string().as_bytes(),
        );
    }

    impl FakeServer {
        /// A server on 127.0.0.1 that calls `handler` once per connection. Its thread runs until
        /// the test process exits.
        fn start(handler: impl Fn(&Req, &mut TcpStream) + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let connections = Arc::new(AtomicUsize::new(0));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let handler = Arc::new(handler);
            let (count, log) = (Arc::clone(&connections), Arc::clone(&requests));
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    count.fetch_add(1, Ordering::SeqCst);
                    let (handler, log) = (Arc::clone(&handler), Arc::clone(&log));
                    std::thread::spawn(move || {
                        if let Some(req) = read_request(&mut stream) {
                            // Logged BEFORE the reply is written, so a test that sees the
                            // answer can always see the request.
                            log.lock().unwrap().push(req.clone());
                            handler(&req, &mut stream);
                        }
                    });
                }
            });
            Self {
                port,
                connections,
                requests,
            }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn request(&self, index: usize) -> (String, String, Vec<(String, String)>, Vec<u8>) {
            let log = self.requests.lock().unwrap();
            let r = &log[index];
            (
                r.method.clone(),
                r.path.clone(),
                r.headers.clone(),
                r.body.clone(),
            )
        }
    }

    fn fast_limits() -> HttpLimits {
        HttpLimits {
            connect: Duration::from_secs(1),
            read: Duration::from_millis(400),
            total: Duration::from_secs(2),
            max_request_bytes: 1024,
            max_response_bytes: 16 * 1024,
        }
    }

    // --- sync adapter ------------------------------------------------------------------------

    fn sample_batch(seq: u64) -> SyncBatch {
        SyncBatch {
            seq,
            ops: vec![
                SyncOp::PutDoc {
                    collection: "notes".into(),
                    id: "a/b".into(),
                    doc: json!({ "title": "t", "n": [1, 2] }),
                },
                SyncOp::DeleteDoc {
                    collection: "notes".into(),
                    id: "gone".into(),
                },
                SyncOp::AppendColumnGroup {
                    name: "cols".into(),
                    bytes: vec![0, 1, 2, 255],
                },
            ],
        }
    }

    fn wire_batch(batch: &SyncBatch) -> Value {
        encode_batch(batch).unwrap()
    }

    #[test]
    fn sync_adapter_round_trips_batches_through_record_envelopes() {
        let stored: Arc<Mutex<Vec<Value>>> = Arc::default();
        let held = Arc::clone(&stored);
        let server = FakeServer::start(move |req, stream| {
            match (req.method.as_str(), req.path.split('?').next().unwrap()) {
                ("POST", "/v1/sync/batches") => {
                    held.lock().unwrap().push(req.json());
                    reply_json(stream, 200, &json!({}));
                }
                ("GET", "/v1/sync/batches") => {
                    let after: u64 = req
                        .path
                        .split("after_seq=")
                        .nth(1)
                        .unwrap()
                        .parse()
                        .unwrap();
                    let batches: Vec<Value> = held
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|b| b["seq"].as_u64().unwrap() > after)
                        .cloned()
                        .collect();
                    reply_json(stream, 200, &json!({ "batches": batches }));
                }
                _ => reply_json(stream, 404, &json!({})),
            }
        });
        let adapter = HttpCloudSyncAdapter::new(&server.url()).unwrap();
        assert_eq!(adapter.name(), "http-cloud-sync");
        let (one, two) = (sample_batch(7), sample_batch(9));
        adapter.on_commit(&one).unwrap();
        adapter.on_commit(&two).unwrap();

        // What went over the wire is the batch's own envelope frames.
        let (method, path, headers, body) = server.request(0);
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("POST", "/v1/sync/batches")
        );
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        };
        assert_eq!(header("content-type").as_deref(), Some("application/json"));
        assert!(header("authorization").is_none() && header("cookie").is_none());
        let wire: Value = serde_json::from_slice(&body).unwrap();
        let frames: Vec<Vec<u8>> = wire["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| BASE64.decode(r.as_str().unwrap()).unwrap())
            .collect();
        let sent: Vec<RecordEnvelope> = frames
            .iter()
            .map(|f| RecordEnvelope::decode(f).unwrap())
            .collect();
        assert_eq!(sent, one.to_envelopes().unwrap());

        assert_eq!(
            adapter.pull_since(0).unwrap(),
            vec![one.clone(), two.clone()]
        );
        assert_eq!(adapter.pull_since(7).unwrap(), vec![two]);
        assert!(adapter.pull_since(9).unwrap().is_empty());
        let (_, path, ..) = server.request(2);
        assert_eq!(path, "/v1/sync/batches?after_seq=0");
    }

    #[test]
    fn sync_adapter_fetches_documents_with_encoded_segments_and_maps_misses() {
        let server = FakeServer::start(|req, stream| match req.path.as_str() {
            "/v1/sync/documents/my%20coll/a%2Fb%3F%C3%A9" => {
                reply_json(stream, 200, &json!({ "doc": { "x": 1 } }))
            }
            "/v1/sync/documents/c/null" => reply_json(stream, 200, &json!({ "doc": null })),
            "/v1/sync/documents/c/absent" => reply_json(stream, 200, &json!({})),
            "/v1/sync/documents/c/missing" => reply_json(stream, 404, &json!({})),
            "/v1/sync/documents/c/boom" => reply_json(stream, 503, &json!({})),
            "/v1/sync/documents/c/denied" => reply_json(stream, 403, &json!({})),
            _ => reply_json(stream, 400, &json!({})),
        });
        let adapter = HttpCloudSyncAdapter::new(&server.url()).unwrap();
        assert_eq!(
            adapter.fetch_document("my coll", "a/b?\u{e9}").unwrap(),
            Some(json!({ "x": 1 }))
        );
        for miss in ["null", "absent", "missing"] {
            assert_eq!(adapter.fetch_document("c", miss).unwrap(), None, "{miss}");
        }
        assert!(matches!(
            adapter.fetch_document("c", "boom"),
            Err(StoreError::Unreachable { .. })
        ));
        assert!(matches!(
            adapter.fetch_document("c", "denied"),
            Err(StoreError::Backend(_))
        ));
        let before = server.connections();
        assert!(matches!(
            adapter.fetch_document("", "x"),
            Err(StoreError::Backend(_))
        ));
        assert_eq!(
            server.connections(),
            before,
            "an empty segment is refused before sending"
        );
    }

    #[test]
    fn sync_adapter_refuses_dot_segments_before_sending() {
        let server = FakeServer::start(|_, stream| reply_json(stream, 200, &json!({ "doc": 1 })));
        let adapter = HttpCloudSyncAdapter::new(&server.url()).unwrap();
        for (collection, id) in [
            ("x", ".."),
            ("x", "."),
            ("..", "x"),
            (".", "x"),
            ("", "x"),
            ("x", ""),
            ("..", ".."),
        ] {
            assert!(
                matches!(
                    adapter.fetch_document(collection, id),
                    Err(StoreError::Backend(_))
                ),
                "{collection:?}/{id:?}"
            );
        }
        assert_eq!(server.connections(), 0);
        assert_eq!(server.request_count(), 0);
        // Dots inside a longer segment are ordinary, and still reach the server encoded.
        assert!(adapter.fetch_document("x", "..a").unwrap().is_some());
        assert!(adapter.fetch_document("x.y", "a..").unwrap().is_some());
        assert_eq!(server.request_count(), 2);
    }

    #[test]
    fn sync_adapter_prewarms_and_rejects_malformed_answers() {
        let good = wire_batch(&sample_batch(3));
        let answer = Arc::new(Mutex::new(json!({ "batches": [good.clone()] })));
        let current = Arc::clone(&answer);
        let server =
            FakeServer::start(move |_, stream| reply_json(stream, 200, &current.lock().unwrap()));
        let adapter = HttpCloudSyncAdapter::new(&server.url()).unwrap();
        assert_eq!(
            adapter.prewarm(&["notes", "tasks"]).unwrap(),
            vec![sample_batch(3)]
        );
        let (method, path, _, body) = server.request(0);
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("POST", "/v1/sync/prewarm")
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({ "collections": ["notes", "tasks"] })
        );

        let mut bad_frame = good.clone();
        bad_frame["records"][0] = json!("AAAA");
        let mut seq_mismatch = good.clone();
        seq_mismatch["seq"] = json!(4);
        let older = wire_batch(&sample_batch(5));
        for (what, bad) in [
            ("no batches", json!({})),
            ("batches not an array", json!({ "batches": 1 })),
            (
                "a batch with no seq",
                json!({ "batches": [{ "records": [] }] }),
            ),
            (
                "not base64",
                json!({ "batches": [{ "seq": 1, "records": ["!!"] }] }),
            ),
            ("not an envelope", json!({ "batches": [bad_frame] })),
            (
                "seq disagrees with its frames",
                json!({ "batches": [seq_mismatch] }),
            ),
        ] {
            *answer.lock().unwrap() = bad;
            assert!(adapter.prewarm(&["notes"]).is_err(), "{what}");
            assert!(adapter.pull_since(0).is_err(), "{what}");
        }
        // pull_since refuses batches at or below the cursor and out-of-order lists.
        *answer.lock().unwrap() = json!({ "batches": [good.clone()] });
        assert!(adapter.pull_since(3).is_err());
        assert!(adapter.pull_since(2).is_ok());
        *answer.lock().unwrap() = json!({ "batches": [older, good.clone()] });
        assert!(adapter.pull_since(0).is_err());
        assert!(
            adapter.prewarm(&["notes"]).is_ok(),
            "prewarm is a snapshot, not a cursor"
        );
        *answer.lock().unwrap() = json!({ "batches": [good.clone(), good] });
        assert!(adapter.pull_since(0).is_err());
    }

    #[test]
    fn sync_adapter_errors_name_the_host_and_never_the_path() {
        let server =
            FakeServer::start(|_, stream| reply(stream, 200, &[], b"<html>not json</html>"));
        let url = format!("{}/secret-token-in-path", server.url());
        let adapter = HttpCloudSyncAdapter::new(&url).unwrap();
        let err = adapter.pull_since(0).unwrap_err().to_string();
        assert!(err.contains("127.0.0.1"), "{err}");
        assert!(!err.contains("secret-token-in-path"), "{err}");
        let (_, path, ..) = server.request(0);
        assert_eq!(path, "/secret-token-in-path/v1/sync/batches?after_seq=0");
    }

    // --- registry and relay adapters ---------------------------------------------------------

    #[test]
    fn registry_adapter_lists_and_publishes_in_the_cloud_tier() {
        let server = FakeServer::start(|req, stream| {
            match (req.method.as_str(), req.path.as_str()) {
                ("GET", "/v0.1/servers") => reply_json(
                    stream,
                    200,
                    &json!({ "servers": [{ "name": "a" }, { "server": { "name": "b" } }], "metadata": {} }),
                ),
                ("POST", "/v0.1/servers")
                    if req.json()["servers"].as_array().unwrap().len() == 2 =>
                {
                    reply_json(stream, 200, &json!({ "published": 1 }))
                }
                ("POST", "/v0.1/servers") => reply(stream, 204, &[], b""),
                _ => reply_json(stream, 404, &json!({})),
            }
        });
        let adapter = HttpCloudRegistryAdapter::new("gcp", &server.url()).unwrap();
        assert_eq!(adapter.provider_name(), "gcp");
        assert_eq!(
            adapter.tier(),
            RegistryTier::CloudProvider {
                provider: "gcp".into()
            }
        );
        assert_eq!(
            adapter.list_servers().unwrap(),
            vec![json!({ "name": "a" }), json!({ "server": { "name": "b" } })]
        );
        assert_eq!(
            adapter
                .publish_servers(&[json!({ "name": "a" }), json!({ "name": "b" })])
                .unwrap(),
            1
        );
        assert_eq!(
            adapter.publish_servers(&[json!({ "name": "a" })]).unwrap(),
            1
        );
        let (_, _, headers, _) = server.request(1);
        let names: Vec<String> = headers
            .iter()
            .map(|(k, _)| k.to_ascii_lowercase())
            .collect();
        assert!(
            !names.contains(&"authorization".to_string()) && !names.contains(&"cookie".to_string())
        );
    }

    #[test]
    fn registry_adapter_rejects_a_catalog_without_a_servers_array() {
        let server =
            FakeServer::start(|_, stream| reply_json(stream, 200, &json!({ "servers": "x" })));
        let adapter = HttpCloudRegistryAdapter::new("aws", &server.url()).unwrap();
        assert!(
            matches!(adapter.list_servers(), Err(GateError::Adapter { provider, .. }) if provider == "aws")
        );
        let server = FakeServer::start(|_, stream| reply(stream, 204, &[], b""));
        let adapter = HttpCloudRegistryAdapter::new("aws", &server.url()).unwrap();
        assert!(matches!(
            adapter.list_servers(),
            Err(GateError::Adapter { .. })
        ));
        let server = FakeServer::start(|_, stream| reply_json(stream, 500, &json!({})));
        let adapter = HttpCloudRegistryAdapter::new("aws", &server.url()).unwrap();
        assert!(matches!(
            adapter.list_servers(),
            Err(GateError::UpstreamHttp { .. })
        ));
    }

    #[test]
    fn relay_adapter_forwards_jsonrpc_and_maps_no_content_to_none() {
        let server = FakeServer::start(|req, stream| match req.path.as_str() {
            "/v1/relay/conn%2F1" => {
                let request = req.json();
                reply_json(
                    stream,
                    200,
                    &json!({ "jsonrpc": "2.0", "id": request["id"], "result": { "echo": request["method"] } }),
                );
            }
            "/v1/relay/notify" => reply(stream, 204, &[], b""),
            "/v1/relay/blank" => reply(stream, 202, &[], b""),
            "/v1/relay/boom" => reply_json(stream, 502, &json!({})),
            _ => reply_json(stream, 404, &json!({})),
        });
        let adapter = HttpCloudRelayAdapter::new("azure", &server.url()).unwrap();
        assert_eq!(adapter.provider_name(), "azure");
        let rpc = json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" });
        let answer = adapter.forward_jsonrpc("conn/1", &rpc).unwrap().unwrap();
        assert_eq!(answer["result"]["echo"], "tools/list");
        assert_eq!(answer["id"], 5);
        assert_eq!(adapter.forward_jsonrpc("notify", &rpc).unwrap(), None);
        assert_eq!(adapter.forward_jsonrpc("blank", &rpc).unwrap(), None);
        assert!(matches!(
            adapter.forward_jsonrpc("boom", &rpc),
            Err(GateError::UpstreamHttp { .. })
        ));
        assert!(matches!(
            adapter.forward_jsonrpc("", &rpc),
            Err(GateError::Adapter { .. })
        ));
    }

    #[test]
    fn relay_adapter_refuses_dot_connector_ids_before_sending() {
        let server = FakeServer::start(|_, stream| reply_json(stream, 200, &json!({ "ok": 1 })));
        let adapter = HttpCloudRelayAdapter::new("gcp", &server.url()).unwrap();
        let rpc = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });
        for bad in ["..", ".", ""] {
            assert!(
                matches!(
                    adapter.forward_jsonrpc(bad, &rpc),
                    Err(GateError::Adapter { .. })
                ),
                "{bad:?}"
            );
        }
        assert_eq!(server.connections(), 0);
        assert_eq!(server.request_count(), 0);
        assert!(adapter.forward_jsonrpc("..a", &rpc).unwrap().is_some());
        assert_eq!(server.request(0).1, "/v1/relay/..a");
    }

    // --- hardening ---------------------------------------------------------------------------

    #[test]
    fn a_redirect_is_an_error_and_its_target_is_never_contacted() {
        let target =
            FakeServer::start(|_, stream| reply_json(stream, 200, &json!({ "servers": [] })));
        let location = format!("{}/internal", target.url());
        for status in [301u16, 302, 307, 308] {
            let location = location.clone();
            let server = FakeServer::start(move |_, stream| {
                reply(stream, status, &[("Location", location.as_str())], b"")
            });
            let registry = HttpCloudRegistryAdapter::new("gcp", &server.url()).unwrap();
            let err = registry.list_servers().unwrap_err().to_string();
            assert!(err.contains("redirect"), "{status}: {err}");
            let relay = HttpCloudRelayAdapter::new("gcp", &server.url()).unwrap();
            assert!(
                relay.forward_jsonrpc("c", &json!({ "id": 1 })).is_err(),
                "{status}"
            );
            let sync = HttpCloudSyncAdapter::new(&server.url()).unwrap();
            assert!(sync.on_commit(&sample_batch(1)).is_err(), "{status}");
            assert!(sync.pull_since(0).is_err(), "{status}");
        }
        assert_eq!(
            target.connections(),
            0,
            "a redirect target must never see a connection"
        );
    }

    #[test]
    fn a_declared_oversize_answer_is_refused_without_reading_it() {
        let server = FakeServer::start(|_, stream| {
            reply(
                stream,
                200,
                &[("Content-Length", "1073741824")],
                b"{\"servers\":[",
            );
            std::thread::sleep(Duration::from_secs(3));
        });
        let http =
            CloudHttp::with_lookup(&server.url(), Arc::new(SystemLookup), fast_limits()).unwrap();
        let started = Instant::now();
        let err = http.get("/x").unwrap_err();
        assert_eq!(err, CloudHttpError::ResponseTooLarge { limit: 16 * 1024 });
        // The server holds the connection for 3s; refusing on the header alone is far quicker.
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "refused on the header alone: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_streamed_oversize_answer_is_cut_off_at_the_cap() {
        let sent = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        let (counter, finished) = (Arc::clone(&sent), Arc::clone(&done));
        let server = FakeServer::start(move |_, stream| {
            let _ = stream.write_all(
                b"HTTP/1.1 200 X\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n",
            );
            let chunk = vec![b'a'; 8192];
            for _ in 0..32_768 {
                // 256 MiB on offer.
                let framed =
                    [format!("{:x}\r\n", chunk.len()).as_bytes(), &chunk, b"\r\n"].concat();
                if stream.write_all(&framed).is_err() {
                    break;
                }
                counter.fetch_add(framed.len() as u64, Ordering::SeqCst);
            }
            finished.store(1, Ordering::SeqCst);
        });
        let http =
            CloudHttp::with_lookup(&server.url(), Arc::new(SystemLookup), fast_limits()).unwrap();
        let err = http.get("/x").unwrap_err();
        assert_eq!(err, CloudHttpError::ResponseTooLarge { limit: 16 * 1024 });
        let deadline = Instant::now() + Duration::from_secs(5);
        while done.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            done.load(Ordering::SeqCst),
            1,
            "the server must see the client hang up"
        );
        let written = sent.load(Ordering::SeqCst);
        assert!(
            written < 64 << 20,
            "the client kept reading: {written} bytes were taken"
        );
    }

    /// A gzip stream of `1 + 258 * runs` bytes of `a`, a few hundred bytes on the wire: one
    /// literal, then `runs` fixed-Huffman matches of length 258 at distance 1.
    fn gzip_of_as(runs: usize) -> Vec<u8> {
        let mut bits: Vec<bool> = Vec::new();
        let mut put = |value: u32, count: u32, msb_first: bool| {
            for i in 0..count {
                let shift = if msb_first { count - 1 - i } else { i };
                bits.push((value >> shift) & 1 == 1);
            }
        };
        put(1, 1, false); // BFINAL
        put(1, 2, false); // BTYPE = fixed Huffman
        put(0x30 + u32::from(b'a'), 8, true); // literal `a`
        for _ in 0..runs {
            put(0xC0 + 5, 8, true); // length code 285 = 258
            put(0, 5, true); // distance code 0 = 1
        }
        put(0, 7, true); // end of block
        let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
        out.extend(bits.chunks(8).map(|byte| {
            byte.iter()
                .enumerate()
                .fold(0u8, |acc, (i, bit)| acc | (u8::from(*bit) << i))
        }));
        let decoded = vec![b'a'; 1 + 258 * runs];
        let mut crc = 0xFFFF_FFFFu32;
        for byte in &decoded {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & (!(crc & 1)).wrapping_add(1));
            }
        }
        out.extend((!crc).to_le_bytes());
        out.extend((decoded.len() as u32).to_le_bytes());
        out
    }

    #[test]
    fn a_compressed_answer_is_capped_after_decoding() {
        let server = FakeServer::start(|_, stream| {
            let wire = gzip_of_as(300); // 77,401 bytes decoded, under 400 on the wire
            assert!(wire.len() < 1024);
            reply(stream, 200, &[("Content-Encoding", "gzip")], &wire);
        });
        let http =
            CloudHttp::with_lookup(&server.url(), Arc::new(SystemLookup), fast_limits()).unwrap();
        assert_eq!(
            http.get("/x").unwrap_err(),
            CloudHttpError::ResponseTooLarge { limit: 16 * 1024 }
        );
    }

    #[test]
    fn the_client_is_built_without_proxy_or_redirects() {
        let http = CloudHttp::new("https://example.com").unwrap();
        let config = http.agent.config();
        // `HTTP_PROXY` and friends would otherwise route around the connect-time guard.
        assert!(config.proxy().is_none());
        assert_eq!(config.max_redirects(), 0);
        assert!(!config.http_status_as_error());
    }

    #[test]
    fn the_response_cap_is_exact() {
        let limit = fast_limits().max_response_bytes as usize;
        let server = FakeServer::start(move |req, stream| {
            let size = if req.path == "/exact" {
                limit
            } else {
                limit + 1
            };
            let mut body = b"{\"ok\":true}".to_vec();
            body.resize(size, b' ');
            reply(stream, 200, &[], &body);
        });
        let http =
            CloudHttp::with_lookup(&server.url(), Arc::new(SystemLookup), fast_limits()).unwrap();
        assert_eq!(
            http.get("/exact").unwrap().body,
            Some(json!({ "ok": true }))
        );
        assert_eq!(
            http.get("/over").unwrap_err(),
            CloudHttpError::ResponseTooLarge {
                limit: limit as u64
            }
        );
    }

    #[test]
    fn an_oversize_request_is_refused_before_anything_is_sent() {
        let server = FakeServer::start(|_, stream| reply_json(stream, 200, &json!({})));
        let http =
            CloudHttp::with_lookup(&server.url(), Arc::new(SystemLookup), fast_limits()).unwrap();
        let big = json!({ "pad": "x".repeat(2048) });
        assert_eq!(
            http.post("/x", &big).unwrap_err(),
            CloudHttpError::RequestTooLarge { limit: 1024 }
        );
        assert_eq!(server.connections(), 0);
        assert!(http.post("/x", &json!({ "pad": "x" })).is_ok());
    }

    #[test]
    fn a_slow_server_hits_the_timeout() {
        let server = FakeServer::start(|_, stream| {
            std::thread::sleep(Duration::from_secs(8));
            reply_json(stream, 200, &json!({}));
        });
        let http =
            CloudHttp::with_lookup(&server.url(), Arc::new(SystemLookup), fast_limits()).unwrap();
        let started = Instant::now();
        let err = http.get("/x").unwrap_err();
        assert!(
            matches!(err, CloudHttpError::Transport(ref why) if why.contains("timeout")),
            "{err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the timeout fired well before the server's 8s delay: {:?}",
            started.elapsed()
        );
    }

    #[derive(Debug)]
    struct MapLookup(HashMap<String, Vec<IpAddr>>);

    impl MapLookup {
        fn with(host: &str, ips: &[&str]) -> Arc<dyn HostLookup> {
            let ips = ips.iter().map(|t| t.parse().unwrap()).collect();
            Arc::new(Self(HashMap::from([(host.to_string(), ips)])))
        }
    }

    impl HostLookup for MapLookup {
        fn lookup(&self, host: &str, _port: u16) -> io::Result<Vec<IpAddr>> {
            self.0
                .get(host)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such host"))
        }
    }

    #[test]
    fn a_hostname_that_resolves_to_a_forbidden_address_is_refused_at_connect_time() {
        let server =
            FakeServer::start(|_, stream| reply_json(stream, 200, &json!({ "servers": [] })));
        let port = server.port;
        let blocked = |url: String, ips: &[&str], host: &str| {
            let http =
                CloudHttp::with_lookup(&url, MapLookup::with(host, ips), fast_limits()).unwrap();
            http.get("/v0.1/servers").unwrap_err()
        };
        // DNS rebinding to loopback, private, link-local and metadata space, for an https name.
        for ips in [
            &["127.0.0.1"][..],
            &["::1"],
            &["10.0.0.5"],
            &["169.254.169.254"],
            &["fd00:ec2::254"],
            &["::ffff:127.0.0.1"],
            // One bad answer among good ones refuses the lot.
            &["8.8.8.8", "127.0.0.1"],
        ] {
            let err = blocked(
                format!("https://api.example.test:{port}"),
                ips,
                "api.example.test",
            );
            assert!(
                matches!(err, CloudHttpError::Blocked { .. }),
                "{ips:?}: {err:?}"
            );
        }
        // `localhost` is a loopback endpoint: it may resolve to loopback ONLY.
        for ips in [&["10.0.0.5"][..], &["127.0.0.1", "10.0.0.5"], &["8.8.8.8"]] {
            let err = blocked(format!("http://localhost:{port}"), ips, "localhost");
            assert!(
                matches!(err, CloudHttpError::Blocked { .. }),
                "{ips:?}: {err:?}"
            );
        }
        assert_eq!(
            server.connections(),
            0,
            "the guard refuses before any connect"
        );

        // Through an adapter the refusal is an SSRF error, not a generic upstream failure.
        let http = CloudHttp::with_lookup(
            &format!("https://api.example.test:{port}"),
            MapLookup::with("api.example.test", &["10.0.0.5"]),
            fast_limits(),
        )
        .unwrap();
        let registry = HttpCloudRegistryAdapter::with_http("gcp", http.clone());
        assert!(matches!(
            registry.list_servers(),
            Err(GateError::SsrfBlocked { .. })
        ));
        let relay = HttpCloudRelayAdapter::with_http("gcp", http.clone());
        assert!(matches!(
            relay.forward_jsonrpc("c", &json!({})),
            Err(GateError::SsrfBlocked { .. })
        ));
        let sync = HttpCloudSyncAdapter::with_http(http);
        assert!(
            matches!(sync.pull_since(0), Err(StoreError::Backend(ref m)) if m.contains("SSRF"))
        );
        assert_eq!(server.connections(), 0);
    }

    #[test]
    fn the_connection_goes_to_the_address_the_guard_approved() {
        let server =
            FakeServer::start(|_, stream| reply_json(stream, 200, &json!({ "servers": [] })));
        let url = format!("http://localhost:{}", server.port);
        // The name resolves, through the seam, to loopback: approved, and that is where it connects.
        let http = CloudHttp::with_lookup(
            &url,
            MapLookup::with("localhost", &["127.0.0.1"]),
            fast_limits(),
        )
        .unwrap();
        assert!(http.get("/v0.1/servers").is_ok());
        assert_eq!(server.connections(), 1);
        // A lookup failure is a transport error, not a guess.
        let http = CloudHttp::with_lookup(&url, Arc::new(MapLookup(HashMap::new())), fast_limits())
            .unwrap();
        assert!(matches!(http.get("/x"), Err(CloudHttpError::Transport(_))));
        assert_eq!(server.connections(), 1);
    }

    // --- constructors ------------------------------------------------------------------------

    #[test]
    fn constructors_refuse_a_rejected_url() {
        for bad in [
            "",
            "https://10.0.0.5",
            "https://169.254.169.254",
            "http://example.com",
            "https://user:pw@example.com",
            "https://0x7f000001",
        ] {
            assert!(CloudHttp::new(bad).is_err(), "{bad:?}");
            assert!(HttpCloudSyncAdapter::new(bad).is_err(), "{bad:?}");
            assert!(
                HttpCloudRegistryAdapter::new("gcp", bad).is_err(),
                "{bad:?}"
            );
            assert!(HttpCloudRelayAdapter::new("gcp", bad).is_err(), "{bad:?}");
        }
        for provider in ["none", "", "ibm", "GCP"] {
            assert!(HttpCloudRegistryAdapter::new(provider, "https://example.com").is_err());
            assert!(HttpCloudRelayAdapter::new(provider, "https://example.com").is_err());
        }
        assert!(HttpCloudSyncAdapter::new("https://example.com").is_ok());
        assert!(HttpCloudRegistryAdapter::new("azure", "https://example.com").is_ok());
    }

    #[test]
    fn from_config_builds_only_what_is_switched_on() {
        let mut sync = StoreSyncConfig::default();
        assert!(HttpCloudSyncAdapter::from_config(&sync).unwrap().is_none());
        sync.enabled = true;
        assert!(
            HttpCloudSyncAdapter::from_config(&sync).unwrap().is_none(),
            "provider none"
        );
        sync.provider = "gcp".into();
        assert!(
            HttpCloudSyncAdapter::from_config(&sync).is_err(),
            "enabled with no URL"
        );
        sync.endpoint_url = "https://10.0.0.5".into();
        assert!(
            HttpCloudSyncAdapter::from_config(&sync).is_err(),
            "a rejected URL, even from a raw struct"
        );
        sync.endpoint_url = "https://sync.example.com".into();
        let adapter = HttpCloudSyncAdapter::from_config(&sync).unwrap().unwrap();
        assert_eq!(adapter.name(), "http-cloud-sync:gcp");
        sync.provider = "bogus".into();
        assert!(HttpCloudSyncAdapter::from_config(&sync).is_err());
        sync.provider = "aws".into();
        sync.enabled = false;
        assert!(HttpCloudSyncAdapter::from_config(&sync).unwrap().is_none());

        let mut gate = GateCloudConfig::default();
        assert!(
            HttpCloudRegistryAdapter::from_config(&gate)
                .unwrap()
                .is_none()
        );
        assert!(HttpCloudRelayAdapter::from_config(&gate).unwrap().is_none());
        gate.registry_provider = "azure".into();
        gate.endpoint_url = "https://registry.example.com".into();
        gate.relay_endpoint_url = "https://relay.example.com".into();
        assert!(
            HttpCloudRegistryAdapter::from_config(&gate)
                .unwrap()
                .is_none(),
            "not enabled"
        );
        gate.enabled = true;
        let registry = HttpCloudRegistryAdapter::from_config(&gate)
            .unwrap()
            .unwrap();
        assert_eq!(registry.provider_name(), "azure");
        assert!(HttpCloudRelayAdapter::from_config(&gate).unwrap().is_some());
        gate.relay_endpoint_url.clear();
        assert!(HttpCloudRelayAdapter::from_config(&gate).unwrap().is_none());
        gate.endpoint_url = "http://example.com".into();
        assert!(HttpCloudRegistryAdapter::from_config(&gate).is_err());
        gate.registry_provider = "none".into();
        assert!(
            HttpCloudRegistryAdapter::from_config(&gate)
                .unwrap()
                .is_none()
        );
    }

    // --- Wave C audit regressions (2026-10-30) -----------------------------------------------

    #[test]
    fn the_azure_wireserver_is_forbidden_but_its_neighbours_are_not() {
        for ip in [
            "168.63.129.16",
            "::ffff:168.63.129.16",
            "64:ff9b::a83f:8110",
        ] {
            assert!(is_forbidden_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(validate_endpoint_url("https://168.63.129.16").is_err());
        assert!(validate_endpoint_url("https://[::ffff:168.63.129.16]/x").is_err());
        for ip in [
            "168.63.129.15",
            "168.63.129.17",
            "168.63.128.16",
            "168.62.129.16",
        ] {
            assert!(!is_forbidden_ip(ip.parse().unwrap()), "{ip}");
        }
        // The connect-time guard refuses a name that resolves to it.
        assert!(check_resolved(&["168.63.129.16".parse().unwrap()], false).is_err());
    }

    #[test]
    fn ipv6_forms_that_carry_a_private_ipv4_address_are_refused() {
        for url in [
            "https://[::ffff:127.0.0.1]",
            "https://[::ffff:7f00:1]",
            "https://[::127.0.0.1]",
            "https://[::10.0.0.1]/x",
            "https://[64:ff9b::7f00:1]",
            "https://[64:ff9b::a9fe:a9fe]",
            "https://[2002:7f00:1::1]",
            "https://[2002:a9fe:a9fe::]",
            "https://[fd00:ec2::254]",
            "https://[fe80::1]",
            "https://[::]",
            "https://[0:0:0:0:0:0:0:1]",
            "http://[0:0:0:0:0:0:0:1]",
            "http://[::ffff:127.0.0.1]",
            "http://[fe80::1%25eth0]",
        ] {
            assert!(validate_endpoint_url(url).is_err(), "{url}");
        }
        for url in [
            "https://[2606:4700:4700::1111]",
            "https://[2001:4860:4860::8888]:8443/p",
        ] {
            assert!(validate_endpoint_url(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn malformed_authorities_and_numeric_hosts_are_refused() {
        for url in [
            "https://example.com:0",
            "https://example.com:65536",
            "https://example.com:+80",
            "https://example.com:-1",
            "https://example.com:",
            "https://example.com:80:80",
            "https://exa%6dple.com",
            "https://a..b",
            "https://-a.com",
            "https://a-.com",
            "https://example.com.",
            "https://127.0.0.1.",
            "https://1.1.1.1.1",
            "https://0x7f.1",
            "https://0x7f000001",
            "https://017700000001",
            "https://2130706433",
            "https://[::1]x",
            "https://[::1",
            "https://::1",
            "http://localhost:80@evil.com",
            "http://localhost@evil.com",
            "https://evil.com\\@localhost",
            "https://evil.com\t",
            "https://evil.com\0",
            "https://evil.com#@localhost",
            "https://evil.com?@localhost",
            "http://localhost.evil.com",
            "http://127.0.0.1.evil.com",
            "http://127.0.0.2",
            "http://0.0.0.0",
            "file:///etc/passwd",
            "gopher://localhost",
            "//example.com",
            "https:/example.com",
            "https:example.com",
            "https://",
            "",
        ] {
            let got = validate_endpoint_url(url);
            // The empty string is the "adapter off" spelling, handled before this function.
            assert!(got.is_err(), "{url:?} was accepted as {got:?}");
        }
        // Case and a doubled slash in the path are not a way past the host rules.
        let ok = validate_endpoint_url("HTTPS://Example.COM:443//a//b///").unwrap();
        assert_eq!(ok.base(), "https://example.com:443//a//b");
        assert!(!ok.loopback_ok());
        assert!(
            validate_endpoint_url("http://LOCALHOST:8080")
                .unwrap()
                .loopback_ok()
        );
        // `https://localhost` stays loopback-only at connect time.
        assert!(
            validate_endpoint_url("https://localhost")
                .unwrap()
                .loopback_ok()
        );
    }

    #[test]
    fn a_registry_cannot_inflate_the_published_count() {
        let server =
            FakeServer::start(|_, stream| reply_json(stream, 200, &json!({ "published": 99 })));
        let adapter = HttpCloudRegistryAdapter::new("aws", &server.url()).unwrap();
        let sent = [json!({ "name": "a" }), json!({ "name": "b" })];
        assert_eq!(adapter.publish_servers(&sent).unwrap(), 2);
        assert_eq!(adapter.publish_servers(&[]).unwrap(), 0);
        // A smaller honest count, and a garbage one, still behave.
        let server =
            FakeServer::start(|_, stream| reply_json(stream, 200, &json!({ "published": 1 })));
        let adapter = HttpCloudRegistryAdapter::new("aws", &server.url()).unwrap();
        assert_eq!(adapter.publish_servers(&sent).unwrap(), 1);
        let server =
            FakeServer::start(|_, stream| reply_json(stream, 200, &json!({ "published": -3 })));
        let adapter = HttpCloudRegistryAdapter::new("aws", &server.url()).unwrap();
        assert_eq!(adapter.publish_servers(&sent).unwrap(), 2);
    }

    #[test]
    fn path_segments_cannot_escape_their_route() {
        let server = FakeServer::start(|_, stream| {
            reply_json(stream, 200, &json!({ "doc": { "ok": true } }))
        });
        let adapter = HttpCloudSyncAdapter::new(&server.url()).unwrap();
        for (collection, id, want) in [
            ("a/b", "c", "/v1/sync/documents/a%2Fb/c"),
            ("a", "b?x=1#f", "/v1/sync/documents/a/b%3Fx%3D1%23f"),
            ("a", ".%2e", "/v1/sync/documents/a/.%252e"),
            ("a", "%2e%2e", "/v1/sync/documents/a/%252e%252e"),
            ("a", "b\\c", "/v1/sync/documents/a/b%5Cc"),
            (
                "a",
                "b\r\nHost: evil",
                "/v1/sync/documents/a/b%0D%0AHost%3A%20evil",
            ),
            ("\u{e9}", "x", "/v1/sync/documents/%C3%A9/x"),
            ("a", "...", "/v1/sync/documents/a/..."),
        ] {
            adapter.fetch_document(collection, id).unwrap();
            let (_, path, ..) = server.request(server.request_count() - 1);
            assert_eq!(path, want, "{collection:?}/{id:?}");
        }
    }
}
