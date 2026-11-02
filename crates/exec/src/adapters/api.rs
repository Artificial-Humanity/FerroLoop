//! The direct-API and local-engine adapter: an attempt is one HTTP request to a vendor's API.
//!
//! It serves every `HttpApi` vendor in the registry (`anthropic-api`, `openai-api`, `gemini-api`,
//! `xai-api`, `openrouter-api`, `kimi-api`, `ollama`, `lemonade`, and any vendor registered with
//! [`fl_core::register_vendor`]) through three dialects.
//!
//! It is a *completion* adapter, not an agent loop: it sends the instruction and reports the
//! reply, the tokens and the cost. It runs no tools, so it changes no files by itself; the git
//! snapshot is taken anyway, so `paths_touched` is the truth if anything else moved.
//!
//! Safety, in the order a reader should check it:
//! - The API key goes only to the vendor's base URL, which must be `https://` or loopback `http://`
//!   ([`fl_core::validate_api_base_url`]); redirects are never followed, so a key cannot be
//!   forwarded. The key sits in a header, never in a URL, a log line, an outcome or `Debug`.
//! - A hosted vendor needs a rate card. Without one the spend ceiling cannot be kept, so the
//!   attempt is refused. The reply's length is capped from the ceiling before the request
//!   (`max_tokens`), so a priced attempt cannot overshoot by more than the prompt estimate.
//! - Cached input tokens are counted in `tokens_in`, as the CLI adapters count them.

use crate::adapters::harness::{
    EXCERPT_LIMIT, GitSnapshot, LivenessSink, detect_host, now_at, truncate_utf8, usd_f64_to_micros,
};
use crate::runner::{AttemptError, AttemptOutcome, AttemptSpec, Runner};
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::log::AttemptStatus;
use fl_core::telemetry::{AttemptHeartbeat, LivenessStatus, RateCard};
use fl_core::vendor::{ApiDialect, VendorDescriptor, VendorTransport, validate_api_base_url};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The longest reply asked for when the spend ceiling does not set a lower one.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 4096;
/// An answer larger than this is not read.
const BODY_LIMIT: u64 = 4 << 20;
/// How often a running call reports itself alive.
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Bytes of an instruction that count as one token when the prompt is priced before the call.
/// Three is cautious (English averages four): it over-counts, so the ceiling holds.
const BYTES_PER_PROMPT_TOKEN: u64 = 3;
/// Tokens added to the prompt estimate for the request's own framing.
const PROMPT_OVERHEAD_TOKENS: u64 = 32;

/// Gives the API key for a vendor, or `None` when there is none. It is called once per attempt,
/// so a Vault-backed resolver reads the secret as late as possible.
pub type ApiKeyResolver = Arc<dyn Fn(&VendorDescriptor) -> Option<String> + Send + Sync>;

/// A resolver that reads the vendor's `env_key_name` from the process environment.
pub fn env_key_resolver() -> ApiKeyResolver {
    Arc::new(|d: &VendorDescriptor| match &d.transport {
        VendorTransport::HttpApi {
            env_key_name: Some(name),
            ..
        } => std::env::var(name).ok().filter(|k| !k.trim().is_empty()),
        _ => None,
    })
}

/// Drives an HTTP API vendor as a [`Runner`].
pub struct HttpApiAdapter {
    descriptor: VendorDescriptor,
    dialect: ApiDialect,
    base_url: String,
    model: String,
    rate_card: Option<RateCard>,
    key_resolver: ApiKeyResolver,
    max_output_tokens: u64,
    heartbeat_interval: Duration,
    liveness_sink: Option<LivenessSink>,
    attempt_id: Option<Iri>,
    project: Option<ProjectId>,
    agent: Option<String>,
    host: String,
}

impl std::fmt::Debug for HttpApiAdapter {
    /// ⚠ Nothing here can hold a key: the resolver is opaque and is not printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpApiAdapter")
            .field("vendor", &self.descriptor.id)
            .field("dialect", &self.dialect)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl HttpApiAdapter {
    /// An adapter for `descriptor`, which must be an `HttpApi` vendor. The key comes from the
    /// vendor's environment variable until [`Self::with_key_resolver`] says otherwise.
    pub fn new(descriptor: VendorDescriptor) -> Result<Self, AttemptError> {
        let VendorTransport::HttpApi {
            default_base_url,
            api_dialect,
            ..
        } = descriptor.transport.clone()
        else {
            return Err(AttemptError::UnknownAdapter(format!(
                "{} (not an HTTP API vendor)",
                descriptor.id
            )));
        };
        let model = descriptor.default_model.clone();
        Ok(Self {
            dialect: api_dialect,
            base_url: default_base_url.trim_end_matches('/').to_string(),
            model,
            rate_card: None,
            key_resolver: env_key_resolver(),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            liveness_sink: None,
            attempt_id: None,
            project: None,
            agent: None,
            host: detect_host(),
            descriptor,
        })
    }

    /// Points the adapter at another base URL (a proxy, a test server). Refused unless it is a
    /// place a key may be sent.
    pub fn with_base_url(mut self, url: &str) -> Result<Self, AttemptError> {
        validate_api_base_url(url).map_err(|e| {
            AttemptError::UnknownAdapter(format!("{}: base URL refused: {e}", self.descriptor.id))
        })?;
        self.base_url = url.trim_end_matches('/').to_string();
        Ok(self)
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        let model = model.into();
        if !model.trim().is_empty() {
            self.model = model;
        }
        self
    }

    pub fn with_rate_card(mut self, rate_card: Option<RateCard>) -> Self {
        self.rate_card = rate_card;
        self
    }

    pub fn with_key_resolver(mut self, resolver: ApiKeyResolver) -> Self {
        self.key_resolver = resolver;
        self
    }

    pub fn with_max_output_tokens(mut self, tokens: u64) -> Self {
        self.max_output_tokens = tokens.max(1);
        self
    }

    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    pub fn with_liveness(
        mut self,
        attempt_id: Iri,
        project: ProjectId,
        agent: Option<String>,
        sink: LivenessSink,
    ) -> Self {
        self.attempt_id = Some(attempt_id);
        self.project = Some(project);
        self.agent = agent;
        self.liveness_sink = Some(sink);
        self
    }

    /// The card the attempt is priced with: the caller's, else the vendor's default.
    fn card(&self) -> RateCard {
        self.rate_card.unwrap_or(self.descriptor.default_rate_card)
    }

    async fn run(&self, spec: AttemptSpec) -> AttemptOutcome {
        let attempt_id = self.attempt_id.clone().unwrap_or_else(|| {
            Iri::parse(&format!("urn:uuid:{}", Uuid::now_v7()))
                .expect("uuid v7 forms a valid urn:uuid IRI")
        });
        let project = self.project.clone().unwrap_or_else(|| {
            ProjectId(
                Iri::parse("urn:uuid:00000000-0000-4000-8000-000000000000")
                    .expect("static urn is valid"),
            )
        });
        let emit = |hb: &AttemptHeartbeat| {
            if let Some(sink) = &self.liveness_sink {
                sink(hb);
            }
        };
        let started_wall = now_at();
        let mut hb = AttemptHeartbeat {
            attempt_id,
            project,
            record: spec.record.clone(),
            adapter: self.descriptor.id.clone(),
            agent: self.agent.clone(),
            host: self.host.clone(),
            pid: 0,
            started_at: started_wall.clone(),
            heartbeat_at: started_wall,
            status: LivenessStatus::Live,
            outcome: None,
            exit_at: None,
            duration_ms: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd_micros: 0,
        };
        let refuse = |hb: &mut AttemptHeartbeat, why: String| {
            let now = now_at();
            hb.heartbeat_at = now.clone();
            hb.exit_at = Some(now);
            hb.status = LivenessStatus::Exited;
            hb.outcome = Some(AttemptStatus::Refused);
            emit(hb);
            AttemptOutcome::refused(why)
        };

        if spec.budget_usd_micros == 0 {
            return refuse(
                &mut hb,
                "the attempt's spend ceiling is zero, so nothing was started".into(),
            );
        }
        let needs_key = self.descriptor.transport.needs_api_key();
        let api_key = if needs_key {
            match (self.key_resolver)(&self.descriptor) {
                Some(k) if !k.trim().is_empty() => Some(k.trim().to_string()),
                _ => {
                    return refuse(
                        &mut hb,
                        format!(
                            "no API key for `{}`: {}",
                            self.descriptor.id,
                            key_hint(&self.descriptor)
                        ),
                    );
                }
            }
        } else {
            None
        };
        let card = self.card();
        let priced = card != RateCard::default();
        if needs_key && !priced {
            return refuse(
                &mut hb,
                format!(
                    "`{}` is a paid API and has no rate card, so its spend ceiling cannot be \
                     kept: set `input_usd_micros_per_mtok` and `output_usd_micros_per_mtok` for \
                     it in config.toml",
                    self.descriptor.id
                ),
            );
        }
        let max_tokens = match output_cap(&card, &spec, self.max_output_tokens) {
            Ok(n) => n,
            Err(why) => return refuse(&mut hb, why),
        };

        let (url, headers, body) = build_request(
            self.dialect,
            &self.descriptor.id,
            &self.base_url,
            &self.model,
            &spec.instruction,
            max_tokens,
            api_key.as_deref(),
        );
        let limit = Duration::from_secs(spec.timeout_secs.max(1));
        let snap_before = GitSnapshot::capture(&spec.project_root).await;
        let started = Instant::now();
        hb.heartbeat_at = now_at();
        emit(&hb);

        let call = tokio::task::spawn_blocking(move || call_api(&url, &headers, &body, limit));
        tokio::pin!(call);
        let mut ticker =
            tokio::time::interval(self.heartbeat_interval.max(Duration::from_millis(5)));
        ticker.tick().await; // the first tick is immediate
        // The call carries its own timeout; this one is a backstop for a blocking thread that
        // never returns.
        let deadline = tokio::time::Instant::now() + limit + Duration::from_secs(2);
        let result = loop {
            tokio::select! {
                joined = &mut call => {
                    break joined.unwrap_or_else(|e| Err(CallError::Failed(format!("the request thread failed: {e}"))));
                }
                _ = tokio::time::sleep_until(deadline) => break Err(CallError::Timeout),
                _ = ticker.tick() => {
                    hb.heartbeat_at = now_at();
                    hb.duration_ms = started.elapsed().as_millis() as u64;
                    emit(&hb);
                }
            }
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        let paths_touched = match snap_before {
            Some(before) => before.diff_after(&spec.project_root).await,
            None => Vec::new(),
        };

        let (status, excerpt, parsed) = match result {
            // Nothing was reached: the same as a binary that would not start.
            Err(CallError::Unreachable(why)) => {
                return {
                    let mut out = refuse(&mut hb, why);
                    out.duration_ms = duration_ms;
                    out
                };
            }
            Err(CallError::Timeout) => (
                AttemptStatus::Timeout,
                format!(
                    "`{}` exceeded its {}s timeout",
                    self.descriptor.id, spec.timeout_secs
                ),
                Parsed::default(),
            ),
            Err(CallError::Failed(why)) => (AttemptStatus::Crashed, why, Parsed::default()),
            Ok((status, text)) if (200..300).contains(&status) => {
                match parse_reply(self.dialect, &text) {
                    Some(p) => (AttemptStatus::Completed, p.text.clone(), p),
                    None => (
                        AttemptStatus::Crashed,
                        format!(
                            "`{}` answered {status} with a body fl cannot read as a {:?} reply",
                            self.descriptor.id, self.dialect
                        ),
                        Parsed::default(),
                    ),
                }
            }
            Ok((status, text)) => (
                AttemptStatus::Crashed,
                format!(
                    "`{}` answered {status}: {}",
                    self.descriptor.id,
                    error_message(&text)
                ),
                Parsed::default(),
            ),
        };

        let cost_usd_micros = card.resolve_cost(
            parsed.tokens_in,
            parsed.tokens_out,
            parsed.vendor_cost_usd_micros,
        );
        let now = now_at();
        hb.heartbeat_at = now.clone();
        hb.exit_at = Some(now);
        hb.status = LivenessStatus::Exited;
        hb.outcome = Some(status);
        hb.duration_ms = duration_ms;
        hb.tokens_in = parsed.tokens_in;
        hb.tokens_out = parsed.tokens_out;
        hb.cost_usd_micros = cost_usd_micros;
        emit(&hb);

        AttemptOutcome {
            status,
            output_excerpt: truncate_utf8(excerpt, EXCERPT_LIMIT),
            duration_ms,
            tokens_in: parsed.tokens_in,
            tokens_out: parsed.tokens_out,
            cost_usd_micros,
            paths_touched,
        }
    }
}

impl Runner for HttpApiAdapter {
    fn id(&self) -> &str {
        &self.descriptor.id
    }

    async fn attempt(&self, spec: AttemptSpec) -> Result<AttemptOutcome, AttemptError> {
        Ok(self.run(spec).await)
    }
}

/// Where to get a key, for the refusal.
fn key_hint(d: &VendorDescriptor) -> String {
    match &d.transport {
        VendorTransport::HttpApi {
            env_key_name,
            vault_secret_name,
            ..
        } => match (env_key_name, vault_secret_name) {
            (Some(env), Some(vault)) if env == vault => {
                format!("set ${env}, or store it in the Vault as `{vault}`")
            }
            (Some(env), Some(vault)) => {
                format!("set ${env}, or store it in the Vault as `{vault}`")
            }
            (Some(env), None) => format!("set ${env}"),
            (None, Some(vault)) => format!("store it in the Vault as `{vault}`"),
            (None, None) => "none is configured".into(),
        },
        VendorTransport::SubscriptionCli { .. } => "none is needed".into(),
    }
}

/// The most output tokens the reply may have so the attempt stays under its ceiling, given what
/// the prompt is estimated to cost. A free (local) vendor is limited only by `cap`.
fn output_cap(card: &RateCard, spec: &AttemptSpec, cap: u64) -> Result<u64, String> {
    if *card == RateCard::default() {
        return Ok(cap);
    }
    let est_in =
        (spec.instruction.len() as u64).div_ceil(BYTES_PER_PROMPT_TOKEN) + PROMPT_OVERHEAD_TOKENS;
    let in_cost = ceil_div_u128(
        u128::from(est_in) * u128::from(card.input_usd_micros_per_mtok),
        1_000_000,
    );
    let budget = u128::from(spec.budget_usd_micros);
    if in_cost >= budget {
        return Err(format!(
            "the prompt alone is estimated at {in_cost} micro-USD, which passes the attempt's \
             spend ceiling of {} micro-USD, so nothing was sent",
            spec.budget_usd_micros
        ));
    }
    if card.output_usd_micros_per_mtok == 0 {
        return Ok(cap);
    }
    let room = (budget - in_cost) * 1_000_000 / u128::from(card.output_usd_micros_per_mtok);
    let room = u64::try_from(room).unwrap_or(u64::MAX).min(cap);
    if room == 0 {
        return Err(format!(
            "the spend ceiling of {} micro-USD leaves no room for a reply after the prompt",
            spec.budget_usd_micros
        ));
    }
    Ok(room)
}

fn ceil_div_u128(n: u128, d: u128) -> u128 {
    n.div_ceil(d)
}

type Headers = Vec<(&'static str, String)>;

/// The URL, headers and JSON body for one request. The key is only ever in `headers`.
fn build_request(
    dialect: ApiDialect,
    vendor_id: &str,
    base_url: &str,
    model: &str,
    instruction: &str,
    max_tokens: u64,
    api_key: Option<&str>,
) -> (String, Headers, Value) {
    let mut headers: Headers = vec![("content-type", "application/json".into())];
    match dialect {
        ApiDialect::OpenAiChatCompletions => {
            if let Some(key) = api_key {
                headers.push(("authorization", format!("Bearer {key}")));
            }
            // OpenAI's own newer models refuse `max_tokens`; every other server of this dialect
            // still expects it.
            let limit_field = if vendor_id == "openai-api" {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            let mut body = json!({
                "model": model,
                "messages": [{ "role": "user", "content": instruction }],
            });
            body[limit_field] = json!(max_tokens);
            (format!("{base_url}/chat/completions"), headers, body)
        }
        ApiDialect::AnthropicMessages => {
            if let Some(key) = api_key {
                headers.push(("x-api-key", key.to_string()));
            }
            headers.push(("anthropic-version", "2023-06-01".into()));
            (
                format!("{base_url}/v1/messages"),
                headers,
                json!({
                    "model": model,
                    "max_tokens": max_tokens,
                    "messages": [{ "role": "user", "content": instruction }],
                }),
            )
        }
        ApiDialect::GeminiGenerateContent => {
            if let Some(key) = api_key {
                // A header, not `?key=`: a URL ends up in logs and error text.
                headers.push(("x-goog-api-key", key.to_string()));
            }
            (
                format!("{base_url}/v1beta/models/{model}:generateContent"),
                headers,
                json!({
                    "contents": [{ "role": "user", "parts": [{ "text": instruction }] }],
                    "generationConfig": { "maxOutputTokens": max_tokens },
                }),
            )
        }
    }
}

enum CallError {
    /// The server could not be reached at all: nothing was sent.
    Unreachable(String),
    Timeout,
    Failed(String),
}

/// One blocking request. Redirects are never followed (the key would go with them), and an
/// error status is an answer, not an error.
fn call_api(
    url: &str,
    headers: &Headers,
    body: &Value,
    limit: Duration,
) -> Result<(u16, String), CallError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(limit))
        .build()
        .into();
    let mut request = agent.post(url);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let mut response = request.send_json(body).map_err(|e| classify(&e))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map_err(|e| classify(&e))?;
    Ok((status, text))
}

fn classify(e: &ureq::Error) -> CallError {
    match e {
        ureq::Error::Timeout(_) => CallError::Timeout,
        ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => CallError::Unreachable(
            format!("could not reach the API: {}", scrub(&e.to_string())),
        ),
        ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => {
            CallError::Unreachable(format!(
                "could not reach the API: {}",
                scrub(&e.to_string())
            ))
        }
        other => CallError::Failed(format!("the request failed: {}", scrub(&other.to_string()))),
    }
}

/// Error text for a person: no key can be in it (a key is only ever in a header), but a URL can
/// carry a path worth keeping short.
fn scrub(text: &str) -> String {
    truncate_utf8(text.to_string(), 300)
}

#[derive(Debug, Default)]
struct Parsed {
    text: String,
    tokens_in: u64,
    tokens_out: u64,
    vendor_cost_usd_micros: Option<u64>,
}

fn u64_at(v: &Value, path: &[&str]) -> Option<u64> {
    let mut cur = v;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_u64()
}

/// The reply's text and usage, or `None` when the body is not a reply of this dialect.
/// Cached input tokens are inside `tokens_in`.
fn parse_reply(dialect: ApiDialect, body: &str) -> Option<Parsed> {
    let v: Value = serde_json::from_str(body).ok()?;
    match dialect {
        ApiDialect::OpenAiChatCompletions => {
            let content = v.pointer("/choices/0/message/content")?;
            let text = match content {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(""),
                Value::Null => String::new(),
                _ => return None,
            };
            // OpenRouter reports what it charged, in USD.
            let vendor_cost = v
                .pointer("/usage/cost")
                .and_then(Value::as_f64)
                .and_then(usd_f64_to_micros);
            Some(Parsed {
                text,
                // `prompt_tokens` already includes `prompt_tokens_details.cached_tokens`.
                tokens_in: u64_at(&v, &["usage", "prompt_tokens"]).unwrap_or(0),
                tokens_out: u64_at(&v, &["usage", "completion_tokens"]).unwrap_or(0),
                vendor_cost_usd_micros: vendor_cost,
            })
        }
        ApiDialect::AnthropicMessages => {
            let parts = v.get("content")?.as_array()?;
            let text = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            let uncached = u64_at(&v, &["usage", "input_tokens"]).unwrap_or(0);
            let created = u64_at(&v, &["usage", "cache_creation_input_tokens"]).unwrap_or(0);
            let read = u64_at(&v, &["usage", "cache_read_input_tokens"]).unwrap_or(0);
            Some(Parsed {
                text,
                tokens_in: uncached.saturating_add(created).saturating_add(read),
                tokens_out: u64_at(&v, &["usage", "output_tokens"]).unwrap_or(0),
                vendor_cost_usd_micros: None,
            })
        }
        ApiDialect::GeminiGenerateContent => {
            let parts = v.pointer("/candidates/0/content/parts")?.as_array()?;
            let text = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            // `promptTokenCount` already includes `cachedContentTokenCount`; thinking tokens
            // are billed as output.
            let out = u64_at(&v, &["usageMetadata", "candidatesTokenCount"])
                .unwrap_or(0)
                .saturating_add(u64_at(&v, &["usageMetadata", "thoughtsTokenCount"]).unwrap_or(0));
            Some(Parsed {
                text,
                tokens_in: u64_at(&v, &["usageMetadata", "promptTokenCount"]).unwrap_or(0),
                tokens_out: out,
                vendor_cost_usd_micros: None,
            })
        }
    }
}

/// The vendor's own message from an error body, else a short piece of the body.
fn error_message(body: &str) -> String {
    let message = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        v.pointer("/error/message")
            .or_else(|| v.get("message"))
            .or_else(|| v.get("error").filter(|e| e.is_string()))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    truncate_utf8(message.unwrap_or_else(|| body.trim().to_string()), 500)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::ids::{RecordId, seq_iri};
    use fl_core::vendor::{VendorRegistry, lookup_vendor};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Value,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    struct Reply {
        status: u16,
        body: String,
        delay: Duration,
        location: Option<String>,
    }

    fn ok(body: Value) -> Reply {
        Reply {
            status: 200,
            body: body.to_string(),
            delay: Duration::ZERO,
            location: None,
        }
    }

    /// A one-thread HTTP server on loopback that records each request and answers with `handler`.
    struct Mock {
        url: String,
        seen: Arc<Mutex<Vec<Seen>>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Mock {
        fn start(handler: impl Fn(&Seen) -> Reply + Send + 'static) -> Self {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}", server.server_addr().to_ip().unwrap());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (seen2, stop2) = (Arc::clone(&seen), Arc::clone(&stop));
            let thread = std::thread::spawn(move || {
                while !stop2.load(Ordering::Relaxed) {
                    let Ok(Some(mut req)) = server.recv_timeout(Duration::from_millis(50)) else {
                        continue;
                    };
                    let mut text = String::new();
                    std::io::Read::read_to_string(req.as_reader(), &mut text).unwrap();
                    let record = Seen {
                        method: req.method().to_string(),
                        path: req.url().to_string(),
                        headers: req
                            .headers()
                            .iter()
                            .map(|h| (h.field.to_string(), h.value.to_string()))
                            .collect(),
                        body: serde_json::from_str(&text).unwrap_or(Value::Null),
                    };
                    let reply = handler(&record);
                    seen2.lock().unwrap().push(record);
                    std::thread::sleep(reply.delay);
                    let mut response = tiny_http::Response::from_string(reply.body)
                        .with_status_code(reply.status)
                        .with_header(
                            tiny_http::Header::from_bytes("content-type", "application/json")
                                .unwrap(),
                        );
                    if let Some(loc) = reply.location {
                        response = response
                            .with_header(tiny_http::Header::from_bytes("location", loc).unwrap());
                    }
                    let _ = req.respond(response);
                }
            });
            Self {
                url,
                seen,
                stop,
                thread: Some(thread),
            }
        }

        fn requests(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    const KEY: &str = "test-key-AAAA";

    fn key() -> ApiKeyResolver {
        Arc::new(|_| Some(KEY.to_string()))
    }

    fn card() -> Option<RateCard> {
        Some(RateCard {
            input_usd_micros_per_mtok: 2_000_000,
            output_usd_micros_per_mtok: 10_000_000,
        })
    }

    fn adapter(id: &str, mock: &Mock) -> HttpApiAdapter {
        HttpApiAdapter::new(lookup_vendor(id).unwrap())
            .unwrap()
            .with_base_url(&mock.url)
            .unwrap()
            .with_key_resolver(key())
            .with_rate_card(card())
            .with_heartbeat_interval(Duration::from_millis(20))
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]);
        git(d.path(), &["config", "user.email", "t@example.com"]);
        git(d.path(), &["config", "user.name", "t"]);
        std::fs::write(d.path().join("a.txt"), "a\n").unwrap();
        git(d.path(), &["add", "-A"]);
        git(d.path(), &["commit", "-qm", "init"]);
        d
    }

    fn spec(dir: &std::path::Path, budget: u64) -> AttemptSpec {
        AttemptSpec {
            project_root: dir.to_path_buf(),
            record: RecordId(seq_iri(1)),
            instruction: "say hello".into(),
            timeout_secs: 5,
            budget_usd_micros: budget,
        }
    }

    fn sink() -> (LivenessSink, Arc<Mutex<Vec<AttemptHeartbeat>>>) {
        let beats = Arc::new(Mutex::new(Vec::new()));
        let b = Arc::clone(&beats);
        (
            Arc::new(move |hb| b.lock().unwrap().push(hb.clone())),
            beats,
        )
    }

    fn openai_reply() -> Reply {
        ok(json!({
            "choices": [{ "message": { "role": "assistant", "content": "hello from kimi" } }],
            "usage": { "prompt_tokens": 1000, "completion_tokens": 200,
                       "prompt_tokens_details": { "cached_tokens": 400 } },
        }))
    }

    #[tokio::test]
    async fn the_openai_dialect_serves_kimi_with_tokens_cost_heartbeats_and_touched_paths() {
        let dir = repo();
        let root = dir.path().to_path_buf();
        // The server writes a file while the request is in flight, as anything else on the
        // machine could: the snapshot delta must see it.
        let mock = Mock::start(move |_| {
            std::fs::write(root.join("new.txt"), "x").unwrap();
            let mut r = openai_reply();
            r.delay = Duration::from_millis(120);
            r
        });
        let (sink, beats) = sink();
        let a = adapter("kimi-api", &mock).with_liveness(
            seq_iri(10),
            ProjectId(seq_iri(20)),
            Some("feira".into()),
            sink,
        );
        let out = a.attempt(spec(dir.path(), 1_000_000)).await.unwrap();
        assert_eq!(
            out.status,
            AttemptStatus::Completed,
            "{}",
            out.output_excerpt
        );
        assert_eq!(out.output_excerpt, "hello from kimi");
        // 1000 in * 2.0 + 200 out * 10.0 = 4000 micro-USD. Cached tokens are inside `tokens_in`.
        assert_eq!((out.tokens_in, out.tokens_out), (1000, 200));
        assert_eq!(out.cost_usd_micros, 4_000);
        assert_eq!(out.paths_touched, vec!["new.txt".to_string()]);

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            (reqs[0].method.as_str(), reqs[0].path.as_str()),
            ("POST", "/chat/completions")
        );
        assert_eq!(
            reqs[0].header("authorization"),
            Some("Bearer test-key-AAAA")
        );
        assert_eq!(reqs[0].body["model"], "kimi-k2.5");
        assert_eq!(reqs[0].body["messages"][0]["content"], "say hello");
        assert!(reqs[0].body["max_tokens"].as_u64().unwrap() <= DEFAULT_MAX_OUTPUT_TOKENS);

        let beats = beats.lock().unwrap();
        assert!(
            beats.len() >= 3,
            "start, at least one live beat, exit: {}",
            beats.len()
        );
        let last = beats.last().unwrap();
        assert_eq!(last.adapter, "kimi-api");
        assert_eq!(last.agent.as_deref(), Some("feira"));
        assert_eq!(last.status, LivenessStatus::Exited);
        assert_eq!(last.outcome, Some(AttemptStatus::Completed));
        assert_eq!(
            (last.tokens_in, last.tokens_out, last.cost_usd_micros),
            (1000, 200, 4_000)
        );
        assert!(
            beats
                .iter()
                .any(|b| b.status == LivenessStatus::Live && b.duration_ms > 0)
        );
    }

    #[tokio::test]
    async fn a_local_engine_needs_no_key_and_costs_nothing() {
        let dir = repo();
        let mock = Mock::start(|_| openai_reply());
        for id in ["ollama", "lemonade"] {
            let a = HttpApiAdapter::new(lookup_vendor(id).unwrap())
                .unwrap()
                .with_base_url(&mock.url)
                .unwrap()
                .with_key_resolver(Arc::new(|_| panic!("a local engine asks for no key")));
            let out = a.attempt(spec(dir.path(), 1)).await.unwrap();
            assert_eq!(
                out.status,
                AttemptStatus::Completed,
                "{id}: {}",
                out.output_excerpt
            );
            assert_eq!(out.cost_usd_micros, 0);
            assert_eq!((out.tokens_in, out.tokens_out), (1000, 200));
        }
        let reqs = mock.requests();
        assert!(reqs.iter().all(|r| r.header("authorization").is_none()));
        assert_eq!(reqs[0].body["model"], "llama3.2");
        assert_eq!(reqs[1].body["model"], "Llama-3.2-3B-Instruct-Hybrid");
    }

    #[tokio::test]
    async fn the_anthropic_dialect_counts_cache_tokens_as_input() {
        let dir = repo();
        let mock = Mock::start(|_| {
            ok(json!({
                "content": [{ "type": "text", "text": "hi " }, { "type": "text", "text": "there" }],
                "usage": { "input_tokens": 100, "cache_creation_input_tokens": 50,
                           "cache_read_input_tokens": 850, "output_tokens": 30 },
            }))
        });
        let out = adapter("anthropic-api", &mock)
            .attempt(spec(dir.path(), 1_000_000))
            .await
            .unwrap();
        assert_eq!(
            out.status,
            AttemptStatus::Completed,
            "{}",
            out.output_excerpt
        );
        assert_eq!(out.output_excerpt, "hi there");
        assert_eq!((out.tokens_in, out.tokens_out), (1000, 30));
        assert_eq!(out.cost_usd_micros, 2_000 + 300);
        let r = &mock.requests()[0];
        assert_eq!(r.path, "/v1/messages");
        assert_eq!(r.header("x-api-key"), Some(KEY));
        assert_eq!(r.header("anthropic-version"), Some("2023-06-01"));
        assert!(r.header("authorization").is_none());
        assert_eq!(r.body["model"], "claude-sonnet-5-5");
        assert!(r.body["max_tokens"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn the_gemini_dialect_puts_the_model_in_the_path_and_the_key_in_a_header() {
        let dir = repo();
        let mock = Mock::start(|_| {
            ok(json!({
                "candidates": [{ "content": { "parts": [{ "text": "gem" }] } }],
                "usageMetadata": { "promptTokenCount": 500, "candidatesTokenCount": 40,
                                   "thoughtsTokenCount": 60, "cachedContentTokenCount": 100 },
            }))
        });
        let out = adapter("gemini-api", &mock)
            .attempt(spec(dir.path(), 1_000_000))
            .await
            .unwrap();
        assert_eq!(
            out.status,
            AttemptStatus::Completed,
            "{}",
            out.output_excerpt
        );
        assert_eq!(out.output_excerpt, "gem");
        // Thinking tokens are billed as output; cached tokens are inside the prompt count.
        assert_eq!((out.tokens_in, out.tokens_out), (500, 100));
        let r = &mock.requests()[0];
        assert_eq!(r.path, "/v1beta/models/gemini-2.5-pro:generateContent");
        assert_eq!(r.header("x-goog-api-key"), Some(KEY));
        assert!(!r.path.contains("key="), "the key is never in the URL");
        assert_eq!(r.body["contents"][0]["parts"][0]["text"], "say hello");
        assert!(
            r.body["generationConfig"]["maxOutputTokens"]
                .as_u64()
                .unwrap()
                > 0
        );
    }

    #[tokio::test]
    async fn openrouter_reports_its_own_charge_and_openai_uses_the_newer_limit_field() {
        let dir = repo();
        let mock = Mock::start(|_| {
            ok(json!({
                "choices": [{ "message": { "content": [{ "type": "text", "text": "routed" }] } }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 5, "cost": 0.000123 },
            }))
        });
        let out = adapter("openrouter-api", &mock)
            .attempt(spec(dir.path(), 1_000_000))
            .await
            .unwrap();
        assert_eq!(out.output_excerpt, "routed");
        assert_eq!(
            out.cost_usd_micros, 123,
            "the vendor's charge wins over the card"
        );
        adapter("openai-api", &mock)
            .attempt(spec(dir.path(), 1_000_000))
            .await
            .unwrap();
        let reqs = mock.requests();
        assert!(reqs[0].body.get("max_tokens").is_some());
        assert!(reqs[1].body.get("max_completion_tokens").is_some());
        assert!(reqs[1].body.get("max_tokens").is_none());
    }

    #[tokio::test]
    async fn the_spend_ceiling_caps_the_reply_before_the_request_is_sent() {
        let dir = repo();
        let mock = Mock::start(|_| openai_reply());
        // Output is 10 micro-USD per token. 500 micro-USD, less the prompt, allows < 50 tokens.
        adapter("kimi-api", &mock)
            .attempt(spec(dir.path(), 500))
            .await
            .unwrap();
        let cap = mock.requests()[0].body["max_tokens"].as_u64().unwrap();
        assert!((1..50).contains(&cap), "cap {cap}");
        // A prompt that alone passes the ceiling is never sent.
        let mut big = spec(dir.path(), 5);
        big.instruction = "x".repeat(10_000);
        let out = adapter("kimi-api", &mock).attempt(big).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Refused);
        assert!(
            out.output_excerpt.contains("spend ceiling"),
            "{}",
            out.output_excerpt
        );
        assert_eq!(
            mock.requests().len(),
            1,
            "nothing was sent for the refused attempt"
        );
    }

    #[tokio::test]
    async fn an_attempt_that_cannot_be_priced_or_authorised_sends_nothing() {
        let dir = repo();
        let mock = Mock::start(|_| openai_reply());
        let refused = |out: &AttemptOutcome, needle: &str| {
            assert_eq!(out.status, AttemptStatus::Refused, "{}", out.output_excerpt);
            assert!(
                out.output_excerpt.contains(needle),
                "{}",
                out.output_excerpt
            );
        };
        // No key.
        let a = adapter("kimi-api", &mock).with_key_resolver(Arc::new(|_| None));
        let out = a.attempt(spec(dir.path(), 1_000_000)).await.unwrap();
        refused(&out, "MOONSHOT_API_KEY");
        // A blank key is no key.
        let a = adapter("kimi-api", &mock).with_key_resolver(Arc::new(|_| Some("  ".into())));
        refused(
            &a.attempt(spec(dir.path(), 1_000_000)).await.unwrap(),
            "no API key",
        );
        // A paid API with no rate card cannot keep a ceiling.
        let a = adapter("kimi-api", &mock).with_rate_card(None);
        refused(
            &a.attempt(spec(dir.path(), 1_000_000)).await.unwrap(),
            "no rate card",
        );
        // A zero ceiling starts nothing.
        let a = adapter("kimi-api", &mock);
        refused(
            &a.attempt(spec(dir.path(), 0)).await.unwrap(),
            "ceiling is zero",
        );
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn a_base_url_that_could_leak_the_key_is_refused_and_the_key_is_not_in_debug() {
        let kimi = lookup_vendor("kimi-api").unwrap();
        for url in [
            "http://api.moonshot.ai/v1",
            "http://127.0.0.1:1@evil.example/v1",
            "https://user:pw@api.example.com",
        ] {
            let err = HttpApiAdapter::new(kimi.clone())
                .unwrap()
                .with_base_url(url);
            assert!(matches!(err, Err(AttemptError::UnknownAdapter(_))), "{url}");
        }
        let a = HttpApiAdapter::new(kimi)
            .unwrap()
            .with_key_resolver(Arc::new(|_| Some("super-secret-key".into())));
        assert!(!format!("{a:?}").contains("super-secret"));
        // A CLI vendor is not an API vendor.
        let cli = lookup_vendor("claude").unwrap();
        assert!(matches!(
            HttpApiAdapter::new(cli),
            Err(AttemptError::UnknownAdapter(_))
        ));
    }

    #[tokio::test]
    async fn an_error_status_is_a_crash_with_the_vendors_message_and_no_key() {
        let dir = repo();
        let mock = Mock::start(|_| Reply {
            status: 429,
            body: json!({ "error": { "message": "rate limit reached" } }).to_string(),
            delay: Duration::ZERO,
            location: None,
        });
        let out = adapter("kimi-api", &mock)
            .attempt(spec(dir.path(), 1_000_000))
            .await
            .unwrap();
        assert_eq!(out.status, AttemptStatus::Crashed);
        assert!(out.output_excerpt.contains("429"), "{}", out.output_excerpt);
        assert!(out.output_excerpt.contains("rate limit reached"));
        assert!(!out.output_excerpt.contains(KEY));
        assert_eq!(
            (out.tokens_in, out.tokens_out, out.cost_usd_micros),
            (0, 0, 0)
        );
        // A 200 that is not a reply is a crash too, not an empty success.
        let mock = Mock::start(|_| ok(json!({ "unexpected": true })));
        let out = adapter("kimi-api", &mock)
            .attempt(spec(dir.path(), 1_000_000))
            .await
            .unwrap();
        assert_eq!(out.status, AttemptStatus::Crashed);
    }

    #[tokio::test]
    async fn an_engine_that_is_down_is_refused_and_a_slow_one_times_out() {
        let dir = repo();
        // Nothing listens on this port: the attempt never started.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", free.local_addr().unwrap());
        drop(free);
        let down = HttpApiAdapter::new(lookup_vendor("ollama").unwrap())
            .unwrap()
            .with_base_url(&url)
            .unwrap();
        let out = down.attempt(spec(dir.path(), 1)).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Refused, "{}", out.output_excerpt);

        let mock = Mock::start(|_| {
            let mut r = openai_reply();
            r.delay = Duration::from_secs(3);
            r
        });
        let mut s = spec(dir.path(), 1);
        s.timeout_secs = 1;
        let slow = HttpApiAdapter::new(lookup_vendor("ollama").unwrap())
            .unwrap()
            .with_base_url(&mock.url)
            .unwrap();
        let (sink, beats) = sink();
        let slow = slow.with_liveness(seq_iri(1), ProjectId(seq_iri(2)), None, sink);
        let out = slow.attempt(s).await.unwrap();
        assert_eq!(out.status, AttemptStatus::Timeout, "{}", out.output_excerpt);
        assert_eq!(
            beats.lock().unwrap().last().unwrap().outcome,
            Some(AttemptStatus::Timeout)
        );
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed_so_the_key_goes_nowhere_else() {
        let dir = repo();
        let elsewhere = Mock::start(|_| openai_reply());
        let target = format!("{}/stolen", elsewhere.url);
        let mock = Mock::start(move |_| Reply {
            status: 307,
            body: String::new(),
            delay: Duration::ZERO,
            location: Some(target.clone()),
        });
        let out = adapter("kimi-api", &mock)
            .attempt(spec(dir.path(), 1_000_000))
            .await
            .unwrap();
        assert_eq!(out.status, AttemptStatus::Crashed, "{}", out.output_excerpt);
        assert!(
            elsewhere.requests().is_empty(),
            "the redirect target saw a request"
        );
    }

    #[tokio::test]
    async fn a_vendor_registered_at_run_time_runs_through_the_same_adapter() {
        let dir = repo();
        let mock = Mock::start(|_| openai_reply());
        let id = "exec-test-vllm";
        let mut d = lookup_vendor("ollama").unwrap();
        d.id = id.into();
        d.display_name = "My vLLM".into();
        d.default_model = "qwen-local".into();
        fl_core::register_vendor(d).unwrap();
        assert!(VendorRegistry::global().get(id).is_some());
        let out = HttpApiAdapter::new(lookup_vendor(id).unwrap())
            .unwrap()
            .with_base_url(&mock.url)
            .unwrap()
            .attempt(spec(dir.path(), 1))
            .await
            .unwrap();
        assert_eq!(
            out.status,
            AttemptStatus::Completed,
            "{}",
            out.output_excerpt
        );
        assert_eq!(mock.requests()[0].body["model"], "qwen-local");
    }
}
