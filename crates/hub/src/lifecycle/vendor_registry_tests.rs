//! Vendor-registry tests (Local Records 69/70): spin-up and remodel accept every registered
//! vendor, resolve hyphenated vendor ids, fit the state slug per vendor, open no tmux window for
//! an HTTP API vendor, and check an API vendor's key before any side effect.
//!
//! The vendor registry is process-global and tests run in parallel, so every custom vendor here
//! has an id of its own and is registered with `ensure_vendor_registered` (idempotent).

use super::*;
use fl_core::telemetry::RateCard;
use fl_core::{ApiDialect, MemStore, ensure_vendor_registered};
use tempfile::TempDir;

fn custom_http(id: &str, env_key: Option<&str>, vault_secret: Option<&str>) -> String {
    ensure_vendor_registered(VendorDescriptor {
        id: id.to_string(),
        display_name: format!("Custom {id}"),
        transport: VendorTransport::HttpApi {
            default_base_url: "https://example.org/v1".to_string(),
            api_dialect: ApiDialect::OpenAiChatCompletions,
            vault_secret_name: vault_secret.map(str::to_string),
            env_key_name: env_key.map(str::to_string),
        },
        default_model: "m".to_string(),
        default_context_window: 12_345,
        default_rate_card: RateCard::default(),
        mcp_config_rel_path: None,
    })
    .unwrap();
    id.to_string()
}

pub(super) fn custom_cli(id: &str) -> String {
    ensure_vendor_registered(VendorDescriptor {
        id: id.to_string(),
        display_name: format!("Custom {id}"),
        transport: VendorTransport::SubscriptionCli {
            default_binary: "custom-cli".to_string(),
        },
        default_model: String::new(),
        default_context_window: 0,
        default_rate_card: RateCard::default(),
        mcp_config_rel_path: None,
    })
    .unwrap();
    id.to_string()
}

fn req(persona: &str, vendor: &str) -> SpinUpRequest {
    SpinUpRequest {
        persona: persona.into(),
        project: "FerroLoop".into(),
        role: "assistant".into(),
        vendor: vendor.into(),
        ..SpinUpRequest::default()
    }
}

fn env_with<'a>(
    home: &'a TempDir,
    fake: &'a FakeRuntime,
    keys: &'a dyn ApiKeyProbe,
) -> LifecycleEnv<'a> {
    LifecycleEnv {
        key_probe: keys,
        ..fake.env(home.path())
    }
}

fn spin(
    store: &MemStore,
    env: &LifecycleEnv<'_>,
    persona: &str,
    vendor: &str,
) -> Result<SpinUpResponse, LifecycleError> {
    spin_up(store, env, &req(persona, vendor), "owner", Caller::Owner)
}

fn remodel(
    store: &MemStore,
    env: &LifecycleEnv<'_>,
    id: &str,
    to: &str,
) -> Result<RemodelResponse, LifecycleError> {
    let target = SessionTarget::resolve(store, id, None).unwrap();
    let req = RemodelRequest {
        vendor: to.into(),
        ..RemodelRequest::default()
    };
    remodel_session(store, env, &target, &req, "owner", Caller::Owner, |_| {})
}

fn record(store: &MemStore, id: &str) -> HostedSessionRecord {
    find_hosted_session(store, &[id])
        .unwrap()
        .filter(|r| r.session_id == id)
        .unwrap_or_else(|| panic!("no hosted record `{id}`"))
}

// ------------------------------------------------------------------- vendor ids

#[test]
fn spin_up_vendor_ids_are_the_registry_and_validate_accepts_each() {
    let custom = custom_http("hubreg-listed", None, None);
    let ids = spin_up_vendor_ids();
    for builtin in [
        "claude",
        "codex",
        "antigravity",
        "muse",
        "grok",
        "anthropic-api",
        "openai-api",
        "gemini-api",
        "xai-api",
        "openrouter-api",
        "kimi-api",
        "ollama",
        "lemonade",
    ] {
        assert!(ids.iter().any(|i| i == builtin), "{builtin} in {ids:?}");
    }
    assert!(ids.contains(&custom));
    for id in [
        "claude",
        "kimi-api",
        "openrouter-api",
        "ollama",
        custom.as_str(),
    ] {
        let v = req("ada", id).validate().unwrap();
        assert_eq!(v.vendor, id);
    }
    // The vendor is matched without case, like the registry.
    assert_eq!(
        req("ada", " Kimi-API ").validate().unwrap().vendor,
        "kimi-api"
    );
}

#[test]
fn an_unregistered_vendor_is_refused_with_the_registered_ids_listed() {
    custom_http("hubreg-named", None, None);
    for vendor in ["", "kimi", "gemini", "hubreg-missing", "claude2"] {
        let LifecycleError::Invalid(message) = req("ada", vendor).validate().unwrap_err() else {
            panic!("{vendor:?} must be Invalid");
        };
        assert!(message.starts_with("vendor must be one of: "), "{message}");
        for id in ["claude", "kimi-api", "openrouter-api", "hubreg-named"] {
            assert!(message.contains(id), "{id} in {message}");
        }
    }
}

#[test]
fn vendor_of_session_id_matches_the_longest_registered_id() {
    let short = custom_http("hubreg-pfx", None, None);
    let long = custom_http("hubreg-pfx-long", None, None);
    let uuid = "0b9a7a52-3f00-4a1c-9d1e-6f1d2c3b4a5d";
    let cases: Vec<(String, Option<&str>)> = vec![
        ("claude:ada".into(), Some("claude")),
        ("claude".into(), Some("claude")),
        (format!("muse-{uuid}"), Some("muse")),
        ("kimi-api:ada".into(), Some("kimi-api")),
        (format!("kimi-api-{uuid}"), Some("kimi-api")),
        ("openrouter-api:ada".into(), Some("openrouter-api")),
        (format!("{short}:ada"), Some(short.as_str())),
        (format!("{short}-{uuid}"), Some(short.as_str())),
        (format!("{long}:ada"), Some(long.as_str())),
        (format!("{long}-{uuid}"), Some(long.as_str())),
        (long.clone(), Some(long.as_str())),
        // `kimi` is not a registered vendor, and `claudex` is not `claude`.
        ("kimi-ada".into(), None),
        ("kimi:ada".into(), None),
        ("claudex:ada".into(), None),
        ("claude_ada".into(), None),
        ("".into(), None),
        ("ada".into(), None),
    ];
    for (id, want) in cases {
        assert_eq!(
            vendor_of_session_id(&id).as_deref(),
            want,
            "session id {id:?}"
        );
    }
    // The pure matcher, over a table with a prefix pair in either order.
    let table = |ids: &[&str]| ids.iter().map(ToString::to_string).collect::<Vec<_>>();
    for ids in [table(&["a", "a-b"]), table(&["a-b", "a"])] {
        assert_eq!(longest_vendor_prefix(&ids, "a-b:x").as_deref(), Some("a-b"));
        assert_eq!(longest_vendor_prefix(&ids, "a-c").as_deref(), Some("a"));
        assert_eq!(longest_vendor_prefix(&ids, "ab:x"), None);
    }
}

#[test]
fn split_vendor_id_reads_hyphenated_vendors_and_not_unregistered_ones() {
    assert_eq!(
        split_vendor_id("Kimi-API:ada"),
        Some(("kimi-api".to_string(), "ada"))
    );
    assert_eq!(split_vendor_id("kimi:ada"), None);
}

#[test]
fn a_session_without_a_record_resolves_its_hyphenated_vendor() {
    let store = MemStore::default();
    let target = SessionTarget::resolve(&store, "kimi-api:ada", None).unwrap();
    assert_eq!(target.persona, "ada");
    let rec = record_for_target(&store, &target).unwrap();
    assert_eq!(rec.vendor, "kimi-api");
    // A bare persona has no vendor.
    let target = SessionTarget::resolve(&store, "ada", None).unwrap();
    assert_eq!(
        record_for_target(&store, &target).unwrap().vendor,
        "unknown"
    );
}

// ------------------------------------------------------------------- state slug

#[test]
fn a_persona_that_does_not_fit_the_vendors_slug_is_refused() {
    let long = "a".repeat(MAX_PERSONA_LEN);
    // `hosted-session-openrouter-api-` is 30 characters: 34 remain.
    assert_eq!(max_persona_len("openrouter-api"), 34);
    assert_eq!(max_persona_len("claude"), MAX_PERSONA_LEN);
    let LifecycleError::Invalid(message) = req(&long, "openrouter-api").validate().unwrap_err()
    else {
        panic!("a 36-character persona must not fit openrouter-api");
    };
    assert!(
        message.starts_with("persona")
            && message.contains("34")
            && message.contains("openrouter-api"),
        "{message}"
    );
    // The same persona fits a short vendor.
    req(&long, "claude").validate().unwrap();
    req(&"a".repeat(34), "openrouter-api").validate().unwrap();
    req("ada", "openrouter-api").validate().unwrap();
    // A 64-character vendor id leaves no room at all.
    let huge = custom_http(&format!("hubreg-{}", "x".repeat(57)), None, None);
    assert_eq!(huge.len(), 64);
    let LifecycleError::Invalid(message) = req("a", &huge).validate().unwrap_err() else {
        panic!("a 64-character vendor id cannot host a session");
    };
    assert!(
        message.starts_with("vendor") && message.contains("too long"),
        "{message}"
    );
}

#[test]
fn a_vendor_id_a_state_slug_cannot_hold_cannot_host() {
    let LifecycleError::Invalid(message) = check_persona_fits_slug("my.vllm", "ada").unwrap_err()
    else {
        panic!("a `.` is not a slug character");
    };
    assert!(message.starts_with("vendor"), "{message}");
}

#[test]
fn every_registered_vendor_with_its_longest_persona_persists_a_valid_slug() {
    custom_http("hubreg-slug-api", None, None);
    custom_cli("hubreg-slug-cli");
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = env_with(&home, &fake, &ProvisionedKeys);
    let mut hosted = 0;
    for vendor in spin_up_vendor_ids() {
        let max = max_persona_len(&vendor);
        if max == 0 || vendor.contains('.') {
            continue;
        }
        // A distinct persona per vendor, as long as the vendor allows.
        let persona = format!("p{}", "a".repeat(max - 1));
        let out = spin(&store, &env, &persona, &vendor).unwrap_or_else(|e| panic!("{vendor}: {e}"));
        let slug = hosted_session_slug(&out.session_id);
        assert!(slug.len() <= MAX_STATE_SLUG_LEN, "{slug}");
        assert_eq!(validate_slug(&slug).unwrap(), slug);
        // The record is in the store under exactly that slug.
        let entry = store
            .get_state_by_key(&StateScope::Workspace, StateConcern::Runtime, &slug)
            .unwrap()
            .unwrap_or_else(|| panic!("no entry `{slug}`"));
        assert_eq!(entry.slug, slug);
        assert_eq!(record(&store, &out.session_id).vendor, vendor);
        // One character more would not fit, unless the cap is what stops it.
        if max < MAX_PERSONA_LEN {
            assert!(req(&format!("{persona}a"), &vendor).validate().is_err());
        }
        hosted += 1;
    }
    assert!(hosted >= 15, "{hosted}");
    assert_eq!(list_hosted_sessions(&store).unwrap().len(), hosted);
}

// ------------------------------------------------------------------- no tmux for an API vendor

#[test]
fn spin_up_on_an_api_vendor_opens_no_tmux_window() {
    let custom = custom_http("hubreg-nowindow", None, None);
    for vendor in [
        "kimi-api",
        "openrouter-api",
        "ollama",
        "lemonade",
        custom.as_str(),
    ] {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let env = env_with(&home, &fake, &ProvisionedKeys);
        let out = spin(&store, &env, "ada", vendor).unwrap();
        assert!(fake.calls.borrow().is_empty(), "{vendor}: {:?}", fake.calls);
        assert_eq!(out.session_id, format!("{vendor}:ada"));
        assert_eq!(out.tmux_target, None, "{vendor}");
        assert!(!out.runtime_spawned && out.hosted && out.status == "running");
        let rec = record(&store, &out.session_id);
        assert_eq!(rec.tmux_target, None);
        assert!(!rec.runtime_spawned && rec.hosted && rec.status == "running");
        // The agent is registered as for a CLI vendor.
        assert_eq!(
            store.agent_by_name("ada").unwrap().unwrap().status,
            AgentStatus::Active
        );
    }
}

#[test]
fn a_cli_vendor_still_opens_its_window() {
    let custom = custom_cli("hubreg-cli");
    for vendor in ["claude", custom.as_str()] {
        let store = MemStore::default();
        let home = TempDir::new().unwrap();
        let fake = FakeRuntime::default();
        let env = env_with(&home, &fake, &ProvisionedKeys);
        let out = spin(&store, &env, "ada", vendor).unwrap();
        assert_eq!(*fake.calls.borrow(), [format!("spawn Ada:{vendor}")]);
        assert!(out.runtime_spawned);
        assert_eq!(out.tmux_target, Some(format!("Ada:{vendor}")));
        assert!(record(&store, &out.session_id).runtime_spawned);
    }
}

#[test]
fn teardown_and_restart_of_an_api_session_touch_no_tmux_session() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    // Someone else's tmux session `Ada` runs: an API session must neither kill nor reopen it.
    let fake = FakeRuntime::running(&["Ada"]);
    let env = env_with(&home, &fake, &ProvisionedKeys);
    spin(&store, &env, "ada", "kimi-api").unwrap();

    let restarted = restart_hosted_session(&store, &env, "kimi-api:ada", "owner").unwrap();
    assert_eq!(restarted.record.restart_count, 1);
    assert!(!restarted.record.runtime_spawned && restarted.record.tmux_target.is_none());

    let target = SessionTarget::resolve(&store, "kimi-api:ada", None).unwrap();
    let out = teardown_session(&store, &env, &target, "owner", Caller::Owner, |_| {}).unwrap();
    assert_eq!(out.status, "decommissioned");
    assert!(!out.runtime_stopped);
    assert!(fake.calls.borrow().is_empty(), "{:?}", fake.calls);
    assert!(fake.is_live("Ada"));
    assert_eq!(
        store.agent_by_name("ada").unwrap().unwrap().status,
        AgentStatus::Retired
    );
}

#[test]
fn an_api_session_does_not_inherit_a_runtime_flag() {
    // A legacy record that claims a runtime (set by hand) is cleared by a fresh spin-up.
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = env_with(&home, &fake, &ProvisionedKeys);
    spin(&store, &env, "ada", "kimi-api").unwrap();
    let mut rec = record(&store, "kimi-api:ada");
    rec.runtime_spawned = true;
    persist_hosted_session(&store, &rec, "owner").unwrap();
    spin(&store, &env, "ada", "kimi-api").unwrap();
    assert!(!record(&store, "kimi-api:ada").runtime_spawned);
    assert!(fake.calls.borrow().is_empty());
}

#[test]
fn remodel_between_a_cli_and_an_api_vendor_in_both_directions() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = env_with(&home, &fake, &ProvisionedKeys);
    spin(&store, &env, "ada", "claude").unwrap();
    assert!(fake.is_live("Ada"));

    // CLI -> API: the old window is stopped, no new one opens.
    let out = remodel(&store, &env, "claude:ada", "kimi-api").unwrap();
    assert_eq!(out.session_id, "kimi-api:ada");
    assert_eq!(out.from_vendor, "claude");
    assert!(out.runtime_stopped);
    assert_eq!(out.tmux_target, None);
    assert!(!fake.is_live("Ada"));
    assert_eq!(*fake.calls.borrow(), ["spawn Ada:claude", "kill Ada"]);
    let rec = record(&store, "kimi-api:ada");
    assert!(!rec.runtime_spawned && rec.tmux_target.is_none() && rec.status == "running");
    assert_eq!(record(&store, "claude:ada").status, "decommissioned");

    // API -> API, the source being a hyphenated id found from the session id alone.
    let out = remodel(&store, &env, "kimi-api:ada", "openrouter-api").unwrap();
    assert_eq!(out.from_vendor, "kimi-api");
    assert_eq!(out.session_id, "openrouter-api:ada");
    assert_eq!(fake.calls.borrow().len(), 2, "no tmux call for API vendors");

    // API -> CLI: nothing to stop, the new window opens.
    let out = remodel(&store, &env, "openrouter-api:ada", "codex").unwrap();
    assert_eq!(out.session_id, "codex:ada");
    assert!(!out.runtime_stopped);
    assert_eq!(out.tmux_target.as_deref(), Some("Ada:codex"));
    assert!(fake.is_live("Ada"));
    assert!(record(&store, "codex:ada").runtime_spawned);
}

#[test]
fn remodel_accepts_a_custom_vendor_as_target_and_as_source() {
    let custom = custom_http("hubreg-remodel", None, None);
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = env_with(&home, &fake, &ProvisionedKeys);
    spin(&store, &env, "ada", "claude").unwrap();
    let out = remodel(&store, &env, "claude:ada", &custom).unwrap();
    assert_eq!(out.session_id, format!("{custom}:ada"));
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    // The custom id, hyphenated, is read back as the source of the next remodel, and a failed
    // remodel can be restored onto it.
    let target = SessionTarget::resolve(&store, &format!("{custom}:ada"), None).unwrap();
    let plan = plan_remodel(
        &store,
        &env,
        &target,
        &RemodelRequest {
            vendor: "claude".into(),
            ..RemodelRequest::default()
        },
        Caller::Peer,
    )
    .unwrap();
    assert_eq!(plan.from_vendor, custom);
    assert_eq!(
        plan.restore.as_ref().map(|r| r.vendor.as_str()),
        Some(custom.as_str())
    );
    let out = remodel(&store, &env, &format!("{custom}:ada"), "claude").unwrap();
    assert_eq!(out.from_vendor, custom);

    let err = remodel(&store, &env, "claude:ada", "not-a-vendor").unwrap_err();
    assert!(matches!(err, LifecycleError::Invalid(m) if m.starts_with("vendor must be one of")));
}

// ------------------------------------------------------------------- API key check

#[test]
fn spin_up_on_an_api_vendor_without_its_key_is_refused_before_any_side_effect() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let keys = FakeKeys::default();
    let env = env_with(&home, &fake, &keys);
    let err = spin(&store, &env, "ada", "kimi-api").unwrap_err();
    assert!(matches!(err, LifecycleError::Conflict(_)), "{err:?}");
    assert_eq!(err.status(), 409);
    let message = err.to_string();
    for needle in [
        "kimi-api",
        "MOONSHOT_API_KEY",
        "environment variable",
        "Vault secret",
    ] {
        assert!(message.contains(needle), "{needle} in {message}");
    }
    assert!(store.agent_by_name("ada").unwrap().is_none());
    assert!(list_hosted_sessions(&store).unwrap().is_empty());
    assert!(fake.calls.borrow().is_empty());
    assert!(!keys.asked.borrow().is_empty());

    // With the key, the same request succeeds.
    let keys = FakeKeys {
        have: vec!["MOONSHOT_API_KEY".into()],
        ..FakeKeys::default()
    };
    let env = env_with(&home, &fake, &keys);
    spin(&store, &env, "ada", "kimi-api").unwrap();
    assert_eq!(record(&store, "kimi-api:ada").status, "running");
}

#[test]
fn a_local_engine_and_a_cli_vendor_need_no_key() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let keys = FakeKeys::default();
    let env = env_with(&home, &fake, &keys);
    spin(&store, &env, "ada", "ollama").unwrap();
    spin(&store, &env, "bob", "lemonade").unwrap();
    spin(&store, &env, "cy", "claude").unwrap();
    assert!(keys.asked.borrow().is_empty(), "{:?}", keys.asked);
}

#[test]
fn a_custom_vendor_key_is_found_under_either_of_its_names() {
    let custom = custom_http(
        "hubreg-keyed",
        Some("HUBREG_KEYED_ENV"),
        Some("HUBREG_KEYED_SECRET"),
    );
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    for (have, ok) in [
        (vec![], false),
        (vec!["HUBREG_KEYED_ENV"], true),
        (vec!["HUBREG_KEYED_SECRET"], true),
        (vec!["OTHER"], false),
    ] {
        let store = MemStore::default();
        let keys = FakeKeys {
            have: have.iter().map(ToString::to_string).collect(),
            ..FakeKeys::default()
        };
        let env = env_with(&home, &fake, &keys);
        let out = spin(&store, &env, "ada", &custom);
        assert_eq!(out.is_ok(), ok, "{have:?}: {out:?}");
        if let Err(e) = out {
            let message = e.to_string();
            assert!(
                message.contains(&custom)
                    && message.contains("HUBREG_KEYED_ENV")
                    && message.contains("HUBREG_KEYED_SECRET"),
                "{message}"
            );
        }
    }
}

#[test]
fn a_remodel_onto_an_api_vendor_without_its_key_changes_nothing() {
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = env_with(&home, &fake, &ProvisionedKeys);
    spin(&store, &env, "ada", "claude").unwrap();
    let before_calls = fake.calls.borrow().clone();

    let keys = FakeKeys::default();
    let env = env_with(&home, &fake, &keys);
    for mode in [None, Some("prepare")] {
        let target = SessionTarget::resolve(&store, "claude:ada", None).unwrap();
        let req = RemodelRequest {
            vendor: "kimi-api".into(),
            mode: mode.map(str::to_string),
            ..RemodelRequest::default()
        };
        let err = remodel_session(&store, &env, &target, &req, "owner", Caller::Owner, |_| {})
            .unwrap_err();
        assert!(matches!(err, LifecycleError::Conflict(_)), "{err:?}");
        assert!(err.to_string().contains("MOONSHOT_API_KEY"));
    }
    assert_eq!(*fake.calls.borrow(), before_calls);
    assert!(fake.is_live("Ada"));
    let rec = record(&store, "claude:ada");
    assert_eq!(rec.status, "running");
    assert!(rec.runtime_spawned);
    assert_eq!(
        store.agent_by_name("ada").unwrap().unwrap().status,
        AgentStatus::Active
    );
    assert!(
        list_hosted_sessions(&store)
            .unwrap()
            .iter()
            .all(|r| r.vendor == "claude")
    );
    // No handoff was written either.
    assert!(
        store
            .get_state_by_key(
                &StateScope::Workspace,
                StateConcern::Handoff,
                &remodel_handoff_slug("ada"),
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn the_host_probe_reads_vault_metadata_and_a_missing_vault_is_not_a_key() {
    use fl_gate::{MASTER_KEY_LEN, SetSecretArgs, Vault, resolve_vault_path};
    let home = TempDir::new().unwrap();
    let probe = HostKeyProbe;
    // An environment variable that is not set, and no vault file.
    let unset = "FL_HUBREG_NEVER_SET_API_KEY_9F3A";
    assert!(!probe.key_provisioned(Some(home.path()), Some(unset), Some("HUBREG_VAULT_KEY")));
    assert!(
        !resolve_vault_path(Some(home.path())).exists(),
        "probing creates no vault"
    );

    let mut vault =
        Vault::open(resolve_vault_path(Some(home.path())), [7u8; MASTER_KEY_LEN]).unwrap();
    vault
        .set(SetSecretArgs {
            name: "HUBREG_VAULT_KEY".into(),
            value: "not-a-real-key".into(),
            description: None,
            allowed_hosts: Vec::new(),
            allowed_methods: Vec::new(),
            allowed_agents: Vec::new(),
            allow_private_network: false,
            lease_ttl_secs: None,
        })
        .unwrap();
    assert!(probe.key_provisioned(Some(home.path()), Some(unset), Some("HUBREG_VAULT_KEY")));
    assert!(probe.key_provisioned(Some(home.path()), None, Some("hubreg_vault_key")));
    assert!(!probe.key_provisioned(Some(home.path()), Some(unset), Some("HUBREG_OTHER_KEY")));
    assert!(!probe.key_provisioned(Some(home.path()), None, None));
}

#[test]
fn env_values_count_as_a_key_only_when_not_blank() {
    assert!(env_value_present(Some("sk-abc")));
    assert!(env_value_present(Some(" sk-abc ")));
    for blank in [None, Some(""), Some("   "), Some("\t\n")] {
        assert!(!env_value_present(blank), "{blank:?}");
    }
}

#[test]
fn the_host_probe_does_not_count_an_expired_vault_secret() {
    use fl_gate::{MASTER_KEY_LEN, SetSecretArgs, Vault, resolve_vault_path};
    let home = TempDir::new().unwrap();
    let mut vault =
        Vault::open(resolve_vault_path(Some(home.path())), [7u8; MASTER_KEY_LEN]).unwrap();
    for (name, ttl) in [
        ("HUBREG_EXPIRED_KEY", Some(0)),
        ("HUBREG_LEASED_KEY", Some(3_600)),
    ] {
        vault
            .set(SetSecretArgs {
                name: name.into(),
                value: "not-a-real-key".into(),
                description: None,
                allowed_hosts: Vec::new(),
                allowed_methods: Vec::new(),
                allowed_agents: Vec::new(),
                allow_private_network: false,
                lease_ttl_secs: ttl,
            })
            .unwrap();
    }
    // A zero-second lease expires the moment the clock ticks past it.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let probe = HostKeyProbe;
    let unset = "FL_HUBREG_NEVER_SET_API_KEY_9F3B";
    assert!(!probe.key_provisioned(Some(home.path()), Some(unset), Some("HUBREG_EXPIRED_KEY")));
    // An unexpired lease, and a secret that never expires, still count.
    assert!(probe.key_provisioned(Some(home.path()), Some(unset), Some("HUBREG_LEASED_KEY")));
}

// --------------------------------------------------------- slug sharing (hyphenated ids)

fn stored_record(session_id: &str) -> HostedSessionRecord {
    let (vendor, persona) = session_id.split_once(':').unwrap();
    HostedSessionRecord {
        session_id: session_id.into(),
        persona: persona.into(),
        project: "FerroLoop".into(),
        role: "assistant".into(),
        vendor: vendor.into(),
        model: None,
        node_id: DEFAULT_NODE_ID.into(),
        hosted: true,
        status: STATUS_RUNNING.into(),
        restart_policy: "on-failure".into(),
        restart_count: 0,
        tmux_target: None,
        cwd: None,
        hydration_dispatched: false,
        runtime_spawned: false,
        created_at_ms: 1,
        updated_at_ms: 1,
        decommission_prepared_at_ms: None,
    }
}

#[test]
fn a_stored_record_of_another_session_id_on_the_same_slug_is_never_overwritten() {
    // `ghost` is not a registered vendor (a legacy record), so only the stored record can
    // reveal that `ghost-y:ada` shares the slug of `ghost:y-ada`.
    let ghost_y = custom_cli("hubreg-ghost-y");
    let legacy = "hubreg-ghost:y-ada";
    assert_eq!(
        hosted_session_slug(legacy),
        hosted_session_slug(&format!("{ghost_y}:ada"))
    );
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = env_with(&home, &fake, &ProvisionedKeys);
    persist_hosted_session(&store, &stored_record(legacy), "owner").unwrap();
    let before = list_hosted_sessions(&store).unwrap();

    // The persistence itself refuses.
    let err = persist_hosted_session(&store, &stored_record(&format!("{ghost_y}:ada")), "owner")
        .unwrap_err();
    assert!(matches!(err, LifecycleError::Conflict(_)), "{err:?}");
    assert!(err.to_string().contains(legacy), "{err}");

    // A spin-up refuses before it registers the agent or touches the runtime.
    let err = spin(&store, &env, "ada", &ghost_y).unwrap_err();
    assert!(matches!(err, LifecycleError::Conflict(_)), "{err:?}");
    assert!(err.to_string().contains(legacy), "{err}");
    assert!(store.agent_by_name("ada").unwrap().is_none());
    assert!(fake.calls.borrow().is_empty());
    assert_eq!(list_hosted_sessions(&store).unwrap(), before);

    // The same id updates its own record.
    let mut own = stored_record(legacy);
    own.restart_count = 3;
    persist_hosted_session(&store, &own, "owner").unwrap();
    assert_eq!(record(&store, legacy).restart_count, 3);
}

#[test]
fn a_remodel_onto_a_session_id_that_shares_a_stored_slug_changes_nothing() {
    let ghost_y = custom_cli("hubreg-ghost2-y");
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let env = env_with(&home, &fake, &ProvisionedKeys);
    spin(&store, &env, "ada", "claude").unwrap();
    persist_hosted_session(&store, &stored_record("hubreg-ghost2:y-ada"), "owner").unwrap();
    let before_calls = fake.calls.borrow().clone();

    let err = remodel(&store, &env, "claude:ada", &ghost_y).unwrap_err();
    assert!(matches!(err, LifecycleError::Conflict(_)), "{err:?}");
    assert_eq!(*fake.calls.borrow(), before_calls);
    assert!(fake.is_live("Ada"));
    assert_eq!(record(&store, "claude:ada").status, "running");
    assert_eq!(
        record(&store, "hubreg-ghost2:y-ada").vendor,
        "hubreg-ghost2"
    );
}

// ------------------------------------------------------------ restore skips the key check

#[test]
fn the_restore_after_a_failed_remodel_does_not_need_the_old_vendors_key() {
    let old = custom_http("hubreg-restore", Some("HUBREG_RESTORE_ENV"), None);
    let store = MemStore::default();
    let home = TempDir::new().unwrap();
    let fake = FakeRuntime::default();
    let with_key = FakeKeys {
        have: vec!["HUBREG_RESTORE_ENV".into()],
        ..FakeKeys::default()
    };
    spin(&store, &env_with(&home, &fake, &with_key), "ada", &old).unwrap();

    // The key has vanished by the time of the remodel; the target is a CLI that needs none, and
    // its spin-up fails.
    let no_key = FakeKeys::default();
    let env = env_with(&home, &fake, &no_key);
    FAIL_SPIN_UP_VENDORS.with_borrow_mut(|v| v.push("codex".into()));
    let out = remodel(&store, &env, &format!("{old}:ada"), "codex");
    FAIL_SPIN_UP_VENDORS.with_borrow_mut(Vec::clear);
    let LifecycleError::Partial {
        message,
        restored_session_id,
    } = out.unwrap_err()
    else {
        panic!("a failed spin-up is a partial remodel");
    };
    assert_eq!(restored_session_id, Some(format!("{old}:ada")), "{message}");
    assert!(!message.contains("DOWN"), "{message}");
    assert_eq!(record(&store, &format!("{old}:ada")).status, "running");
    assert_eq!(
        store.agent_by_name("ada").unwrap().unwrap().status,
        AgentStatus::Active
    );

    // A plain spin-up still checks the key.
    let err = spin(&store, &env, "bob", &old).unwrap_err();
    assert!(matches!(err, LifecycleError::Conflict(_)), "{err:?}");
}
