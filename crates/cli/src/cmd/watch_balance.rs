//! `fl watch balance`: automated subscription usage balancing (Local Record 60), as a client of
//! the hub's `/api/v1/watch/balance*` endpoints. The hub holds the live quota sources (its
//! discovered sessions and account probes) and serializes lifecycle calls, so the plan and the
//! apply both run there.
//!
//! - `fl watch balance [--dry-run]`: the plan, with nothing changed.
//! - `fl watch balance --apply [--warrant ID_OR_TOKEN]`: run the plan. The Owner's token is
//!   enough; a peer token needs an Owner warrant of scope `watch.balance.apply` that names the
//!   agent (`--agent`, default `$AGENT_PERSONA`, sent as `X-Fl-Agent`).
//! - `fl watch balance policy`: the stored policy.
//! - `fl watch balance policy set --file policy.json`: lay a full or partial policy over it
//!   (Owner only).
//!
//! The hub is `--hub`, else `$FL_HUB_URL`, else `http://<[hub].listen>`, else
//! `http://127.0.0.1:8420`. The bearer token is `--token-file`, else `$FL_HUB_TOKEN`; it is sent
//! only to `https` or to this machine. Exit status: 0 on success, 1 when the hub refuses (any
//! HTTP status but 200) or an applied switch did not succeed, 2 for a usage or connection error.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;
use fl_watch::{BalancePlan, BalancePolicy, SwitchResult, SwitchStatus};
use serde_json::Value;

use crate::cmd::auth::may_receive_token;

const BALANCE: &str = "/api/v1/watch/balance";

/// How long an apply may take: each switch is a teardown and a spin-up.
const APPLY_TIMEOUT: Duration = Duration::from_secs(300);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(clap::Args)]
pub struct BalanceArgs {
    #[command(subcommand)]
    pub cmd: Option<BalanceSub>,
    /// Show the plan and change nothing (the default).
    #[arg(long, conflicts_with = "apply")]
    pub dry_run: bool,
    /// Run the plan: remodel each agent it moves.
    #[arg(long)]
    pub apply: bool,
    /// An Owner warrant (grant id or `flw1.` token) of scope `watch.balance.apply`, for a peer.
    #[arg(long, requires = "apply")]
    pub warrant: Option<String>,
    /// Hub base URL (default `$FL_HUB_URL`, else `[hub].listen`, else `127.0.0.1:8420`).
    #[arg(long, global = true)]
    pub hub: Option<String>,
    /// File that holds the hub bearer token (default `$FL_HUB_TOKEN`).
    #[arg(long, global = true)]
    pub token_file: Option<PathBuf>,
    /// The agent name a peer token acts as (default `$AGENT_PERSONA`).
    #[arg(long, global = true)]
    pub agent: Option<String>,
    /// Output JSON.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum BalanceSub {
    /// Show the balance policy, or change it.
    Policy {
        #[command(subcommand)]
        cmd: Option<PolicySub>,
    },
}

#[derive(Subcommand)]
pub enum PolicySub {
    /// Lay a JSON policy (full or partial) over the stored one. Owner only.
    Set {
        /// The JSON object to lay over the stored policy.
        #[arg(long)]
        file: PathBuf,
    },
}

/// Where to reach the hub, and as whom.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HubClient {
    base: String,
    token: String,
    agent: Option<String>,
}

fn normalize_url(raw: &str) -> String {
    let raw = raw.trim().trim_end_matches('/');
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    }
}

fn client(
    args: &BalanceArgs,
    listen: Option<&str>,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<HubClient, (i32, String)> {
    let nonempty = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let base = nonempty(args.hub.clone())
        .or_else(|| nonempty(get("FL_HUB_URL")))
        .or_else(|| nonempty(listen.map(str::to_string)))
        .unwrap_or_else(|| fl_hub::DEFAULT_HUB_LISTEN.to_string());
    let base = normalize_url(&base);
    let token = match &args.token_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| (2, format!("could not read {}: {e}", path.display())))?
                .trim()
                .to_string(),
        ),
        None => nonempty(get("FL_HUB_TOKEN")),
    };
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Err((
            2,
            "no hub token: set $FL_HUB_TOKEN or pass --token-file".to_string(),
        ));
    };
    if token.chars().any(char::is_control) {
        return Err((2, "the hub token is malformed".to_string()));
    }
    if !may_receive_token(&base) {
        return Err((
            2,
            format!("refusing to send the hub token to {base}: use https or a local address"),
        ));
    }
    let agent = nonempty(args.agent.clone()).or_else(|| nonempty(get("AGENT_PERSONA")));
    Ok(HubClient { base, token, agent })
}

impl HubClient {
    /// `method path` with an optional JSON body: the status and the JSON answer (`null` when
    /// the answer is not JSON).
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<(u16, Value), (i32, String)> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .into();
        let url = format!("{}{path}", self.base);
        let auth = format!("Bearer {}", self.token);
        let unreachable =
            |e: ureq::Error| (2, format!("could not reach the hub at {}: {e}", self.base));
        let mut resp = if method == "GET" {
            let mut req = agent.get(&url).header("Authorization", &auth);
            if let Some(a) = &self.agent {
                req = req.header("X-Fl-Agent", a);
            }
            req.call().map_err(unreachable)?
        } else {
            let mut req = agent.post(&url).header("Authorization", &auth);
            if let Some(a) = &self.agent {
                req = req.header("X-Fl-Agent", a);
            }
            match body {
                Some(b) => req.send_json(b),
                None => req.send_empty(),
            }
            .map_err(unreachable)?
        };
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }
}

fn print_json(out: &mut dyn Write, v: &Value) -> Result<(), (i32, String)> {
    let text = serde_json::to_string_pretty(v).map_err(|e| (2, e.to_string()))?;
    writeln!(out, "{text}").map_err(|e| (2, e.to_string()))
}

fn lines(out: &mut dyn Write, lines: Vec<String>) -> Result<(), (i32, String)> {
    for l in lines {
        writeln!(out, "{l}").map_err(|e| (2, e.to_string()))?;
    }
    Ok(())
}

/// A refusal from the hub: its `error`, with the status.
fn refused(status: u16, body: &Value) -> (i32, String) {
    let why = body["error"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| body.to_string());
    (1, format!("the hub answered HTTP {status}: {why}"))
}

fn decode<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T, (i32, String)> {
    serde_json::from_value(v.clone()).map_err(|e| (2, format!("unreadable hub answer: {e}")))
}

/// Columns padded to their widest cell.
fn table(rows: &[Vec<String>]) -> Vec<String> {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|r| {
            let mut line = String::new();
            for (i, cell) in r.iter().enumerate() {
                if i + 1 == r.len() {
                    line.push_str(cell);
                } else {
                    line.push_str(&format!("{cell:<w$}  ", w = widths[i]));
                }
            }
            line.trim_end().to_string()
        })
        .collect()
}

fn as_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The lines `fl watch balance` prints for a plan.
pub fn render_plan(plan: &BalancePlan) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(why) = &plan.blocked {
        out.push(format!("BLOCKED: {why}"));
    }
    if plan.switches.is_empty() {
        out.push("no switches".to_string());
    } else {
        out.push(format!("switches ({}):", plan.switches.len()));
        let mut rows = vec![vec![
            "PERSONA".to_string(),
            "MOVE".to_string(),
            "REASON".to_string(),
        ]];
        for s in &plan.switches {
            rows.push(vec![
                s.persona.clone(),
                format!("{} -> {}", s.from_vendor, s.to_vendor),
                s.reason.clone(),
            ]);
        }
        out.extend(table(&rows).into_iter().map(|l| format!("  {l}")));
    }
    if !plan.skipped.is_empty() {
        out.push(format!("skipped ({}):", plan.skipped.len()));
        let mut rows = vec![vec![
            "PERSONA".to_string(),
            "VENDOR".to_string(),
            "WHY".to_string(),
            "DETAIL".to_string(),
        ]];
        for s in &plan.skipped {
            rows.push(vec![
                s.persona.clone(),
                s.vendor.clone(),
                as_str(&s.reason),
                s.detail.clone(),
            ]);
        }
        out.extend(table(&rows).into_iter().map(|l| format!("  {l}")));
    }
    out
}

/// The lines `fl watch balance --apply` prints for its results.
pub fn render_results(results: &[SwitchResult], audit_slug: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if results.is_empty() {
        out.push("nothing to apply".to_string());
    } else {
        let mut rows = vec![vec![
            "PERSONA".to_string(),
            "MOVE".to_string(),
            "RESULT".to_string(),
            "DETAIL".to_string(),
        ]];
        for r in results {
            let detail = match (&r.new_session_id, &r.error) {
                (Some(id), _) => format!("now {id}"),
                (None, Some(e)) => e.clone(),
                (None, None) => String::new(),
            };
            let result = match &r.outcome {
                Some(o) if r.status != SwitchStatus::Ok => format!("{} ({o})", as_str(&r.status)),
                _ => as_str(&r.status),
            };
            rows.push(vec![
                r.persona.clone(),
                format!("{} -> {}", r.from_vendor, r.to_vendor),
                result,
                detail,
            ]);
        }
        out.extend(table(&rows));
    }
    if let Some(slug) = audit_slug {
        out.push(format!("audit: {slug}"));
    }
    out
}

/// The lines `fl watch balance policy` prints.
pub fn render_policy(p: &BalancePolicy) -> Vec<String> {
    let list = |l: &[String]| {
        if l.is_empty() {
            "-".to_string()
        } else {
            l.join(", ")
        }
    };
    vec![
        format!("enabled: {}", p.enabled),
        format!(
            "triggers: primary >= {}%, secondary >= {}%",
            p.primary_trigger_pct, p.secondary_trigger_pct
        ),
        format!(
            "target maximums: primary <= {}%, secondary <= {}%",
            p.target_max_primary_pct, p.target_max_secondary_pct
        ),
        format!(
            "cooldown: {} ms, min interval: {} ms, max switches per run: {}",
            p.cooldown_ms, p.min_interval_ms, p.max_switches_per_run
        ),
        format!("pinned personas: {}", list(&p.pinned_personas)),
        format!("excluded personas: {}", list(&p.excluded_personas)),
        format!(
            "allowed target vendors: {}",
            if p.allowed_target_vendors.is_empty() {
                "all".to_string()
            } else {
                p.allowed_target_vendors.join(", ")
            }
        ),
        format!(
            "excluded target vendors: {}",
            list(&p.excluded_target_vendors)
        ),
        format!(
            "allow unknown quota targets: {}",
            p.allow_unknown_quota_targets
        ),
    ]
}

/// Run `fl watch balance` against the hub, writing to `out`. `listen` is `[hub].listen`.
pub fn run_with(
    args: BalanceArgs,
    listen: Option<&str>,
    get: &dyn Fn(&str) -> Option<String>,
    out: &mut dyn Write,
) -> Result<i32, (i32, String)> {
    if args.cmd.is_some() && (args.apply || args.dry_run || args.warrant.is_some()) {
        return Err((
            2,
            "--dry-run, --apply and --warrant do not apply to `policy`".to_string(),
        ));
    }
    let hub = client(&args, listen, get)?;
    let json = args.json;
    match args.cmd {
        Some(BalanceSub::Policy { cmd }) => {
            let (status, body) = match cmd {
                None => hub.call("GET", BALANCE, None, READ_TIMEOUT)?,
                Some(PolicySub::Set { file }) => {
                    let text = std::fs::read_to_string(&file)
                        .map_err(|e| (2, format!("could not read {}: {e}", file.display())))?;
                    let patch: Value = serde_json::from_str(&text)
                        .map_err(|e| (2, format!("{} is not JSON: {e}", file.display())))?;
                    if !patch.is_object() {
                        return Err((2, format!("{} must hold a JSON object", file.display())));
                    }
                    hub.call(
                        "POST",
                        &format!("{BALANCE}/policy"),
                        Some(&patch),
                        READ_TIMEOUT,
                    )?
                }
            };
            if status != 200 {
                if json {
                    print_json(out, &body)?;
                }
                return Err(refused(status, &body));
            }
            if json {
                print_json(out, &body)?;
            } else {
                let policy: BalancePolicy = decode(&body["policy"])?;
                lines(out, render_policy(&policy))?;
            }
            Ok(0)
        }
        None if args.apply => {
            let body = match &args.warrant {
                Some(w) => serde_json::json!({ "warrant": w }),
                None => serde_json::json!({}),
            };
            let (status, answer) = hub.call(
                "POST",
                &format!("{BALANCE}/apply"),
                Some(&body),
                APPLY_TIMEOUT,
            )?;
            if json {
                print_json(out, &answer)?;
            }
            if status != 200 {
                if !json && let Ok(plan) = decode::<BalancePlan>(&answer["plan"]) {
                    lines(out, render_plan(&plan))?;
                }
                return Err(refused(status, &answer));
            }
            let results: Vec<SwitchResult> = decode(&answer["results"])?;
            if !json {
                let plan: BalancePlan = decode(&answer["plan"])?;
                lines(out, render_plan(&plan))?;
                lines(out, render_results(&results, answer["audit_slug"].as_str()))?;
                for w in answer["warnings"].as_array().into_iter().flatten() {
                    writeln!(out, "warning: {}", w.as_str().unwrap_or_default())
                        .map_err(|e| (2, e.to_string()))?;
                }
            }
            let all_ok = results.iter().all(|r| r.status == SwitchStatus::Ok);
            Ok(if all_ok { 0 } else { 1 })
        }
        None => {
            let (status, answer) =
                hub.call("POST", &format!("{BALANCE}/plan"), None, READ_TIMEOUT)?;
            if status != 200 {
                if json {
                    print_json(out, &answer)?;
                }
                return Err(refused(status, &answer));
            }
            if json {
                print_json(out, &answer)?;
            } else {
                let plan: BalancePlan = decode(&answer)?;
                lines(out, render_plan(&plan))?;
            }
            Ok(0)
        }
    }
}

/// Run `fl watch balance` with this process's environment, printing to stdout.
pub fn run(args: BalanceArgs, listen: Option<&str>) -> Result<i32, (i32, String)> {
    let get = |k: &str| std::env::var(k).ok();
    run_with(args, listen, &get, &mut std::io::stdout().lock())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: BalanceArgs,
    }

    fn parse(argv: &[&str]) -> Result<BalanceArgs, clap::Error> {
        let mut full = vec!["balance"];
        full.extend_from_slice(argv);
        Cli::try_parse_from(full).map(|c| c.args)
    }

    /// One request the fake hub saw.
    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        url: String,
        auth: String,
        agent: Option<String>,
        body: Value,
    }

    /// A hub on an ephemeral port that answers every request with `status` and `body`.
    struct FakeHub {
        base: String,
        seen: Arc<Mutex<Vec<Seen>>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeHub {
        fn start(status: u16, body: Value, requests: usize) -> Self {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let seen2 = Arc::clone(&seen);
            let thread = std::thread::spawn(move || {
                for _ in 0..requests {
                    let Ok(Some(mut req)) = server.recv_timeout(Duration::from_secs(10)) else {
                        return;
                    };
                    let mut text = String::new();
                    std::io::Read::read_to_string(req.as_reader(), &mut text).unwrap();
                    let header = |name: &'static str| {
                        req.headers()
                            .iter()
                            .find(|h| h.field.equiv(name))
                            .map(|h| h.value.to_string())
                    };
                    seen2.lock().unwrap().push(Seen {
                        method: req.method().to_string(),
                        url: req.url().to_string(),
                        auth: header("authorization").unwrap_or_default(),
                        agent: header("x-fl-agent"),
                        body: serde_json::from_str(&text).unwrap_or(Value::Null),
                    });
                    let resp = tiny_http::Response::from_string(body.to_string())
                        .with_status_code(status)
                        .with_header(
                            tiny_http::Header::from_bytes("content-type", "application/json")
                                .unwrap(),
                        );
                    let _ = req.respond(resp);
                }
            });
            Self {
                base,
                seen,
                thread: Some(thread),
            }
        }

        fn seen(mut self) -> Vec<Seen> {
            if let Some(t) = self.thread.take() {
                t.join().unwrap();
            }
            self.seen.lock().unwrap().clone()
        }
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    fn run_against(
        hub: &FakeHub,
        argv: &[&str],
        vars: &[(&str, &str)],
    ) -> (Result<i32, (i32, String)>, String) {
        let mut argv: Vec<&str> = argv.to_vec();
        argv.extend(["--hub", hub.base.as_str()]);
        let args = parse(&argv).unwrap();
        let mut out = Vec::new();
        let code = run_with(args, None, &env(vars), &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    fn plan_json() -> Value {
        json!({
            "evaluated_at_ms": 1,
            "policy": BalancePolicy::default(),
            "switches": [{
                "persona": "ada", "session_id": "claude:ada", "project": "p",
                "from_vendor": "claude", "to_vendor": "muse", "trigger": "primary",
                "reason": "claude primary window at 95.0% >= 90% trigger; moving to muse",
                "target_score": 10.0, "target_status": "known"
            }],
            "skipped": [{
                "persona": "bo", "session_id": "codex:bo", "vendor": "codex",
                "reason": "healthy", "detail": "codex primary 40.0% < 90%"
            }],
            "vendors": []
        })
    }

    const TOKEN: &[(&str, &str)] = &[("FL_HUB_TOKEN", "tok-1")];

    #[test]
    fn the_default_is_a_dry_run_that_posts_to_plan_with_the_bearer() {
        let hub = FakeHub::start(200, plan_json(), 1);
        let (code, out) = run_against(&hub, &[], TOKEN);
        assert_eq!(code, Ok(0));
        assert!(out.contains("switches (1):"), "{out}");
        assert!(out.contains("ada"), "{out}");
        assert!(out.contains("claude -> muse"), "{out}");
        assert!(out.contains("skipped (1):"), "{out}");
        assert!(out.contains("healthy"), "{out}");
        let seen = hub.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "POST");
        assert_eq!(seen[0].url, "/api/v1/watch/balance/plan");
        assert_eq!(seen[0].auth, "Bearer tok-1");
        assert_eq!(seen[0].agent, None);
    }

    #[test]
    fn json_output_is_the_hub_answer_and_the_agent_header_is_sent() {
        let hub = FakeHub::start(200, plan_json(), 1);
        let (code, out) = run_against(
            &hub,
            &["--dry-run", "--json"],
            &[("FL_HUB_TOKEN", "tok-1"), ("AGENT_PERSONA", "felicity")],
        );
        assert_eq!(code, Ok(0));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v, plan_json());
        assert_eq!(hub.seen()[0].agent.as_deref(), Some("felicity"));
    }

    #[test]
    fn apply_sends_the_warrant_and_exits_one_when_a_switch_failed() {
        let answer = |status: &str| {
            json!({
                "plan": plan_json(),
                "results": [{
                    "persona": "ada", "session_id": "claude:ada", "from_vendor": "claude",
                    "to_vendor": "muse", "status": status, "outcome": "restored",
                    "error": "spin-up on muse failed"
                }],
                "audit_slug": "watch-balance-audit-1-0",
                "warnings": []
            })
        };
        let hub = FakeHub::start(200, answer("failed"), 1);
        let (code, out) = run_against(
            &hub,
            &["--apply", "--warrant", "fgw-1", "--agent", "felicity"],
            TOKEN,
        );
        assert_eq!(code, Ok(1));
        assert!(out.contains("failed (restored)"), "{out}");
        assert!(out.contains("audit: watch-balance-audit-1-0"), "{out}");
        let seen = hub.seen();
        assert_eq!(seen[0].url, "/api/v1/watch/balance/apply");
        assert_eq!(seen[0].body, json!({ "warrant": "fgw-1" }));
        assert_eq!(seen[0].agent.as_deref(), Some("felicity"));

        let mut ok = answer("ok");
        ok["results"][0]["new_session_id"] = json!("muse:ada");
        ok["results"][0]["outcome"] = json!("remodeled");
        let hub = FakeHub::start(200, ok, 1);
        let (code, out) = run_against(&hub, &["--apply"], TOKEN);
        assert_eq!(code, Ok(0));
        assert!(out.contains("now muse:ada"), "{out}");
        assert_eq!(hub.seen()[0].body, json!({}));
    }

    #[test]
    fn a_hub_refusal_is_a_nonzero_exit_with_its_error() {
        let hub = FakeHub::start(
            409,
            json!({ "error": "balance apply blocked: wait", "plan": plan_json() }),
            1,
        );
        let (code, out) = run_against(&hub, &["--apply"], TOKEN);
        let (status, msg) = code.unwrap_err();
        assert_eq!(status, 1);
        assert!(msg.contains("409") && msg.contains("blocked"), "{msg}");
        assert!(
            out.contains("claude -> muse"),
            "the refused plan is shown: {out}"
        );

        let hub = FakeHub::start(403, json!({ "error": "owner only" }), 1);
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), r#"{"enabled": true}"#).unwrap();
        let path = file.path().to_str().unwrap();
        let (code, _) = run_against(&hub, &["policy", "set", "--file", path], TOKEN);
        assert_eq!(code.unwrap_err().0, 1);
        let seen = hub.seen();
        assert_eq!(seen[0].url, "/api/v1/watch/balance/policy");
        assert_eq!(seen[0].body, json!({ "enabled": true }));
    }

    #[test]
    fn policy_shows_the_stored_policy() {
        let mut policy = BalancePolicy {
            enabled: true,
            pinned_personas: vec!["ferris".into()],
            ..BalancePolicy::default()
        };
        policy.excluded_target_vendors = vec!["grok".into()];
        let hub = FakeHub::start(200, json!({ "policy": policy, "enabled": true }), 1);
        let (code, out) = run_against(&hub, &["policy"], TOKEN);
        assert_eq!(code, Ok(0));
        assert!(out.contains("enabled: true"), "{out}");
        assert!(out.contains("pinned personas: ferris"), "{out}");
        assert!(out.contains("allowed target vendors: all"), "{out}");
        assert!(out.contains("excluded target vendors: grok"), "{out}");
        let seen = hub.seen();
        assert_eq!(
            (seen[0].method.as_str(), seen[0].url.as_str()),
            ("GET", BALANCE)
        );
    }

    #[test]
    fn flags_conflict_and_the_token_is_required_and_kept_local() {
        assert!(parse(&["--dry-run", "--apply"]).is_err());
        assert!(
            parse(&["--warrant", "x"]).is_err(),
            "--warrant needs --apply"
        );
        assert!(
            parse(&["policy", "--json", "--hub", "h"]).is_ok(),
            "globals reach `policy`"
        );
        let args = parse(&["policy", "--hub", "127.0.0.1:1"]).unwrap();
        let err = client(&args, None, &env(&[])).unwrap_err();
        assert_eq!(err.0, 2);
        assert!(err.1.contains("FL_HUB_TOKEN"), "{}", err.1);
        let args = parse(&["--hub", "http://hub.example.com"]).unwrap();
        let err = client(&args, None, &env(TOKEN)).unwrap_err();
        assert!(err.1.contains("refusing"), "{}", err.1);
        // `--apply` with `policy` is refused before anything is sent.
        let args = parse(&["--apply", "policy"]).unwrap();
        let mut out = Vec::new();
        assert_eq!(
            run_with(args, None, &env(TOKEN), &mut out).unwrap_err().0,
            2
        );
    }

    #[test]
    fn the_hub_url_and_token_resolve_in_order() {
        let args = parse(&[]).unwrap();
        let c = client(&args, None, &env(TOKEN)).unwrap();
        assert_eq!(c.base, format!("http://{}", fl_hub::DEFAULT_HUB_LISTEN));
        let c = client(&args, Some("127.0.0.1:9000"), &env(TOKEN)).unwrap();
        assert_eq!(c.base, "http://127.0.0.1:9000");
        let vars = [
            ("FL_HUB_TOKEN", "t"),
            ("FL_HUB_URL", "https://hub.example.com/"),
        ];
        let c = client(&args, Some("127.0.0.1:9000"), &env(&vars)).unwrap();
        assert_eq!(c.base, "https://hub.example.com");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tok");
        std::fs::write(&path, "from-file\n").unwrap();
        let p = path.to_str().unwrap();
        let args = parse(&["--token-file", p, "--hub", "localhost:1/"]).unwrap();
        let c = client(&args, None, &env(TOKEN)).unwrap();
        assert_eq!(c.token, "from-file");
        assert_eq!(c.base, "http://localhost:1");
    }

    #[test]
    fn an_unreachable_hub_is_exit_two() {
        // Bind and drop a listener so the port is closed.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let args = parse(&["--hub", &format!("127.0.0.1:{port}")]).unwrap();
        let mut out = Vec::new();
        let err = run_with(args, None, &env(TOKEN), &mut out).unwrap_err();
        assert_eq!(err.0, 2);
        assert!(err.1.contains("could not reach"), "{}", err.1);
    }
}
