//! Table, property and golden tests. Everything here is data in, data out: no vendor is run
//! and no vendor file is read.

use super::*;
use std::collections::BTreeSet;

// ---------------------------------------------------------------- helpers

fn facts(v: Vendor) -> VendorFacts {
    let mut f = VendorFacts::new(Transport::Pty);
    match v {
        Vendor::Muse => {
            f.muse_profiles = vec![
                MuseProfileFact {
                    id: "ro".into(),
                    ceiling: ProfileCeiling::ReadOnly,
                },
                MuseProfileFact {
                    id: "ws".into(),
                    ceiling: ProfileCeiling::Workspace,
                },
            ];
        }
        Vendor::Antigravity => {
            f.exact_commands = vec!["git status".into(), "npm test".into(), "git a;b".into()];
        }
        _ => {}
    }
    f
}

fn pol(posture: Posture, rules: &[Rule]) -> PermissionPolicy {
    let mut p = PermissionPolicy::new(posture, SandboxChoice::VendorDefault);
    p.rules = rules.to_vec();
    p
}

fn r(cap: Capability, eff: Effect, pat: Option<Pattern>) -> Rule {
    Rule::new(cap, eff, pat)
}

fn pg(g: &str) -> Option<Pattern> {
    Some(Pattern::PathGlob { glob: g.into() })
}
fn ag(g: &str) -> Option<Pattern> {
    Some(Pattern::AbsPathGlob { glob: g.into() })
}
fn argv(a: &[&str]) -> Option<Pattern> {
    Some(Pattern::ArgvPrefix {
        argv: a.iter().map(|s| (*s).to_owned()).collect(),
    })
}
fn sg(g: &str) -> Option<Pattern> {
    Some(Pattern::ShellGlob { glob: g.into() })
}
fn dom(d: &str) -> Option<Pattern> {
    Some(Pattern::Domain { domain: d.into() })
}
fn mcp(s: &str, t: Option<&str>) -> Option<Pattern> {
    Some(Pattern::Mcp {
        server: s.into(),
        tool: t.map(str::to_owned),
    })
}
fn class(c: PathClass) -> Option<Pattern> {
    Some(Pattern::PathClass { class: c })
}

fn ok(v: Vendor, p: &PermissionPolicy) -> CompiledPlan {
    compile(v, &facts(v), p).unwrap_or_else(|e| panic!("{v:?} refused: {e}"))
}

fn json(plan: &CompiledPlan) -> String {
    serde_json::to_string(plan).unwrap()
}

fn file(plan: &CompiledPlan, t: ConfigTarget) -> Option<&str> {
    plan.config_files
        .iter()
        .find(|f| f.target == t)
        .map(|f| f.content.as_str())
}

fn has_arg(plan: &CompiledPlan, a: &str) -> bool {
    plan.launch_args.iter().any(|x| x == a)
}

fn pair(plan: &CompiledPlan, k: &str, v: &str) -> bool {
    plan.launch_args.windows(2).any(|w| w[0] == k && w[1] == v)
}

fn claude_settings(plan: &CompiledPlan) -> serde_json::Value {
    serde_json::from_str(file(plan, ConfigTarget::ClaudeSettings).unwrap_or("{}")).unwrap()
}

fn claude_raw(plan: &CompiledPlan, which: &str) -> Vec<String> {
    claude_settings(plan)["permissions"][which]
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
        .unwrap_or_default()
}

/// The entries a user rule produced: without the protected-path defaults.
fn user_only(v: Vec<String>) -> Vec<String> {
    v.into_iter()
        .filter(|e| {
            ![".claude/", ".git/", ".mcp.json", ".grok/"]
                .iter()
                .any(|p| e.contains(p))
        })
        .collect()
}

fn claude_list(plan: &CompiledPlan, which: &str) -> Vec<String> {
    user_only(claude_raw(plan, which))
}

fn line_for<'a>(plan: &'a CompiledPlan, requested_prefix: &str) -> &'a ReceiptLine {
    plan.receipts
        .iter()
        .find(|l| l.requested.starts_with(requested_prefix))
        .unwrap_or_else(|| panic!("no receipt `{requested_prefix}` in {:#?}", plan.receipts))
}

fn refusal(v: Vendor, p: &PermissionPolicy) -> CompileRefusal {
    compile(v, &facts(v), p).expect_err("expected a refusal")
}

/// The fixture patterns per capability.
fn fixtures(cap: Capability) -> Vec<Option<Pattern>> {
    match cap {
        Capability::FsReadWorkspace | Capability::FsWriteWorkspace => vec![None, pg("src/**")],
        Capability::FsReadExternal | Capability::FsWriteExternal => {
            vec![None, ag("/opt/data/**"), ag("/opt/data/*.csv")]
        }
        Capability::FsWriteProtected => vec![None, class(PathClass::Git)],
        Capability::ShellExec => vec![
            None,
            argv(&["git", "status"]),
            argv(&["git", "a b"]),
            argv(&["rm", "-rf", "x;y"]),
            sg("git *"),
        ],
        Capability::NetworkWeb => vec![None, dom("example.com"), dom("*.example.com")],
        Capability::McpToolCall => vec![None, mcp("srv", None), mcp("srv", Some("tool"))],
    }
}

fn all_rules() -> Vec<Rule> {
    let mut v = Vec::new();
    for cap in Capability::ALL {
        for pat in fixtures(cap) {
            for eff in [Effect::Allow, Effect::Ask, Effect::Deny] {
                v.push(r(cap, eff, pat.clone()));
            }
        }
    }
    v
}

const BYPASS_ARGS: [&str; 7] = [
    "--yolo",
    "--disable-approval",
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--dangerously-bypass-approvals-and-sandbox",
    "--always-approve",
    "bypassPermissions",
];

fn bypass_construct(plan: &CompiledPlan) -> bool {
    plan.launch_args
        .iter()
        .any(|a| BYPASS_ARGS.contains(&a.as_str()))
        || plan
            .launch_args
            .windows(2)
            .any(|w| matches!(w[0].as_str(), "-a" | "--approval-mode") && w[1] == "never")
        || plan.config_files.iter().any(|f| {
            f.content
                .lines()
                .any(|l| l.starts_with("approval_policy = \"never\""))
        })
}

fn sandbox_off_construct(plan: &CompiledPlan) -> bool {
    has_arg(plan, "--disable-sandbox")
        || has_arg(plan, "danger-full-access")
        || has_arg(plan, "--yolo")
}

/// Splitmix-style generator, so the property runs are reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn wild(pat: &str, s: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'*', rest)) => (0..=s.len()).any(|i| go(rest, &s[i..])),
            Some((c, rest)) => s.split_first().is_some_and(|(h, t)| h == c && go(rest, t)),
        }
    }
    go(pat.as_bytes(), s.as_bytes())
}

// ---------------------------------------------------------------- policy wire + validation

#[test]
fn policy_wire_form_is_stable() {
    let json = serde_json::to_string(&Preset::Workspace.policy()).unwrap();
    assert_eq!(
        json,
        r#"{"schema_version":1,"posture":"ask","sandbox":"vendor-default","require_fs_enforcement":false,"rules":[{"capability":"fs_read_workspace","effect":"allow"},{"capability":"fs_write_workspace","effect":"allow"},{"capability":"network_web","effect":"deny"}]}"#
    );
    let back: PermissionPolicy = serde_json::from_str(&json).unwrap();
    assert_eq!(back, Preset::Workspace.policy());
    let typed = serde_json::to_string(&r(
        Capability::ShellExec,
        Effect::Ask,
        argv(&["git", "push"]),
    ))
    .unwrap();
    assert_eq!(
        typed,
        r#"{"capability":"shell_exec","effect":"ask","pattern":{"kind":"argv_prefix","argv":["git","push"]}}"#
    );
}

#[test]
fn inputs_refuse_unknown_fields_and_values() {
    let bad = [
        r#"{"schema_version":1,"posture":"ask","sandbox":"off","rules":[],"extra":1}"#,
        r#"{"schema_version":1,"posture":"ask","sandbox":"off","rules":[{"capability":"shell_exec","effect":"deny","x":1}]}"#,
        r#"{"schema_version":1,"posture":"ask","sandbox":"off","rules":[{"capability":"shell_exec","effect":"deny","pattern":{"kind":"argv_prefix","argv":["a"],"x":1}}]}"#,
        r#"{"schema_version":1,"posture":"ask","sandbox":"off","rules":[{"capability":"telepathy","effect":"deny"}]}"#,
        r#"{"schema_version":1,"posture":"yolo","sandbox":"off","rules":[]}"#,
        r#"{"posture":"ask","sandbox":"off","rules":[]}"#,
    ];
    for b in bad {
        assert!(serde_json::from_str::<PermissionPolicy>(b).is_err(), "{b}");
    }
    let mut p = Preset::Custom.policy();
    p.schema_version = 2;
    assert_eq!(
        p.normalized(),
        Err(PolicyError::UnsupportedSchemaVersion(2))
    );
    assert!(serde_json::from_str::<VendorFacts>(r#"{"transport":"pty","x":1}"#).is_err());
}

fn invalid(cap: Capability, pat: Option<Pattern>) -> PolicyError {
    pol(Posture::Ask, &[r(cap, Effect::Deny, pat)])
        .normalized()
        .expect_err("invalid")
}

#[test]
fn pattern_grammars_are_strict() {
    use Capability::*;
    let long = "a".repeat(MAX_PATTERN_BYTES + 1);
    let bad_paths = [
        "", "../x", "a/../b", "a/..", "/abs", "~/x", "C:/x", "a//b", "a/", "./a", "a/./b", "a\\b",
        "a\0b", "a\nb", "a/[b]", "a/{b,c}", "a**b", "!a", "#a", " a", "a ", &long, "a/***",
    ];
    for g in bad_paths {
        for cap in [FsReadWorkspace, FsWriteWorkspace] {
            assert!(
                matches!(invalid(cap, pg(g)), PolicyError::InvalidPattern { .. }),
                "workspace glob {g:?} must be refused"
            );
        }
    }
    let bad_abs = [
        "", "/", "rel/x", "/a/../b", "/a//b", "/a/", "~/x", "/a\0", "/a/[x]", "/a**",
    ];
    for g in bad_abs {
        assert!(
            matches!(
                invalid(FsReadExternal, ag(g)),
                PolicyError::InvalidPattern { .. }
            ),
            "{g:?}"
        );
    }
    for g in ["src/**", "a/*.rs", "**/x", "dir with space/x", "héllo/*"] {
        assert!(
            pol(Posture::Ask, &[r(FsWriteWorkspace, Effect::Deny, pg(g))])
                .normalized()
                .is_ok(),
            "{g}"
        );
    }
    // Shell.
    assert!(matches!(
        invalid(ShellExec, argv(&[])),
        PolicyError::InvalidPattern { .. }
    ));
    assert!(matches!(
        invalid(ShellExec, argv(&["-rf"])),
        PolicyError::InvalidPattern { .. }
    ));
    assert!(matches!(
        invalid(ShellExec, argv(&["git", ""])),
        PolicyError::InvalidPattern { .. }
    ));
    assert!(matches!(
        invalid(ShellExec, argv(&["git", "a\nb"])),
        PolicyError::InvalidPattern { .. }
    ));
    assert!(matches!(
        invalid(ShellExec, argv(&["git", "a\0"])),
        PolicyError::InvalidPattern { .. }
    ));
    let many: Vec<&str> = std::iter::repeat_n("x", MAX_ARGV_TOKENS + 1).collect();
    assert!(matches!(
        invalid(ShellExec, argv(&many)),
        PolicyError::InvalidPattern { .. }
    ));
    for g in [
        "", " git", "git(", "a\\b", "a**", "a?b", "a[b]", "a{b}", "a\0",
    ] {
        assert!(
            matches!(
                invalid(ShellExec, sg(g)),
                PolicyError::InvalidPattern { .. }
            ),
            "{g:?}"
        );
    }
    // Network.
    for d in [
        "",
        "Ex ample.com",
        "exa_mple.com",
        "-a.com",
        "a-.com",
        "a..com",
        "a.com.",
        "a.com:80",
        "https://a.com",
        "a.com/x",
        "héllo.com",
        "*",
        "*.",
        "**.a.com",
        "a.*.com",
    ] {
        let res = pol(Posture::Ask, &[r(NetworkWeb, Effect::Deny, dom(d))]).normalized();
        if d == "*" {
            assert!(res.is_ok());
        } else {
            assert!(
                matches!(res, Err(PolicyError::InvalidPattern { .. })),
                "{d:?}"
            );
        }
    }
    // MCP.
    for (s, t) in [
        ("", None),
        ("a__b", None),
        ("_a", None),
        ("a_", None),
        ("-a", None),
        ("a b", None),
        ("a", Some("")),
        ("a", Some("b c")),
    ] {
        assert!(
            matches!(
                invalid(McpToolCall, mcp(s, t)),
                PolicyError::InvalidPattern { .. }
            ),
            "{s:?} {t:?}"
        );
    }
    // Kind mismatch.
    assert!(matches!(
        invalid(ShellExec, pg("a")),
        PolicyError::PatternKindMismatch { .. }
    ));
    assert!(matches!(
        invalid(FsWriteProtected, pg("a")),
        PolicyError::PatternKindMismatch { .. }
    ));
    assert!(matches!(
        invalid(NetworkWeb, argv(&["a"])),
        PolicyError::PatternKindMismatch { .. }
    ));
    assert!(matches!(
        invalid(FsReadWorkspace, ag("/a")),
        PolicyError::PatternKindMismatch { .. }
    ));
}

#[test]
fn rule_count_is_bounded() {
    let rules: Vec<Rule> = (0..=MAX_RULES)
        .map(|i| {
            r(
                Capability::FsReadWorkspace,
                Effect::Allow,
                pg(&format!("d{i}/**")),
            )
        })
        .collect();
    assert_eq!(
        pol(Posture::Ask, &rules).normalized(),
        Err(PolicyError::TooManyRules(MAX_RULES + 1))
    );
}

#[test]
fn normalisation_canonicalises_dedups_and_orders() {
    let a = pol(
        Posture::Ask,
        &[
            r(Capability::NetworkWeb, Effect::Allow, dom("B.example.com")),
            r(Capability::FsReadWorkspace, Effect::Allow, pg("**")),
            r(Capability::NetworkWeb, Effect::Deny, dom("*")),
            r(Capability::NetworkWeb, Effect::Allow, dom("B.example.com")),
            r(Capability::ShellExec, Effect::Allow, sg("*")),
            r(Capability::FsReadExternal, Effect::Ask, ag("/**")),
        ],
    );
    let mut b = a.clone();
    b.rules.reverse();
    let (na, nb) = (a.normalized().unwrap(), b.normalized().unwrap());
    assert_eq!(na, nb, "input order must not matter");
    assert_eq!(na.rules.len(), 5);
    assert_eq!(
        na.rules[0],
        r(Capability::FsReadWorkspace, Effect::Allow, None)
    );
    // deny before allow within a capability; `*` and `**` collapse to the whole capability.
    assert_eq!(na.rules[3], r(Capability::NetworkWeb, Effect::Deny, None));
    assert_eq!(
        na.rules[4],
        r(Capability::NetworkWeb, Effect::Allow, dom("b.example.com"))
    );
    assert_eq!(na.normalized().unwrap(), na, "idempotent");
}

#[test]
fn conflicting_duplicates_are_refused() {
    let p = pol(
        Posture::Ask,
        &[
            r(Capability::ShellExec, Effect::Allow, argv(&["git"])),
            r(Capability::FsReadWorkspace, Effect::Allow, None),
            r(Capability::ShellExec, Effect::Deny, argv(&["git"])),
        ],
    );
    assert!(matches!(
        p.normalized(),
        Err(PolicyError::ConflictingRules {
            capability: Capability::ShellExec,
            ..
        })
    ));
    // Different patterns resolve by precedence instead.
    let q = pol(
        Posture::Ask,
        &[
            r(Capability::ShellExec, Effect::Allow, argv(&["git"])),
            r(Capability::ShellExec, Effect::Deny, argv(&["git", "push"])),
        ],
    );
    assert!(q.normalized().is_ok());
}

#[test]
fn postures_that_disable_needed_checks_are_refused() {
    for eff in [Effect::Deny, Effect::Ask] {
        let p = pol(
            Posture::Bypass,
            &[r(Capability::ShellExec, eff, argv(&["rm"]))],
        );
        assert!(matches!(
            p.normalized(),
            Err(PolicyError::BypassWithRestrictions { .. })
        ));
    }
    assert!(
        pol(
            Posture::Bypass,
            &[r(Capability::ShellExec, Effect::Allow, None)]
        )
        .normalized()
        .is_ok()
    );
    assert!(matches!(
        pol(
            Posture::Auto,
            &[r(Capability::ShellExec, Effect::Ask, None)]
        )
        .normalized(),
        Err(PolicyError::PostureDisablesChecks {
            posture: Posture::Auto,
            ..
        })
    ));
    assert!(matches!(
        pol(
            Posture::AcceptEdits,
            &[r(Capability::FsWriteWorkspace, Effect::Ask, None)]
        )
        .normalized(),
        Err(PolicyError::PostureDisablesChecks {
            posture: Posture::AcceptEdits,
            ..
        })
    ));
    assert!(
        pol(
            Posture::AcceptEdits,
            &[r(Capability::ShellExec, Effect::Ask, None)]
        )
        .normalized()
        .is_ok()
    );
    let mut p = pol(Posture::Ask, &[]);
    p.sandbox = SandboxChoice::Off;
    p.require_fs_enforcement = true;
    assert!(matches!(
        p.normalized(),
        Err(PolicyError::EnforcementWithoutBoundary(_))
    ));
    let mut p = pol(Posture::Bypass, &[]);
    p.require_fs_enforcement = true;
    assert!(matches!(
        p.normalized(),
        Err(PolicyError::EnforcementWithoutBoundary(_))
    ));
}

#[test]
fn presets_are_valid_and_documented() {
    for p in Preset::ALL {
        let pol = p.policy();
        assert_eq!(
            pol.normalized().unwrap().rules.len(),
            pol.rules.len(),
            "{p:?} is already canonical"
        );
    }
    assert_eq!(Preset::ReadOnly.policy().posture, Posture::Plan);
    assert!(Preset::Custom.policy().rules.is_empty());
    assert_eq!(
        canonical_default(Capability::FsWriteProtected),
        Effect::Deny
    );
    assert!(
        Capability::ALL
            .iter()
            .filter(|c| canonical_default(**c) == Effect::Ask)
            .count()
            == 7
    );
}

// ---------------------------------------------------------------- refusals before compile

#[test]
fn transport_facts_and_policy_are_checked_first() {
    let p = Preset::Custom.policy();
    let f = VendorFacts::new(Transport::Msp);
    assert!(matches!(
        compile(Vendor::Claude, &f, &p),
        Err(CompileRefusal::UnsupportedTransport { .. })
    ));
    let mut f = VendorFacts::new(Transport::Pty);
    f.installed_version = Some("1 2".into());
    assert!(matches!(
        compile(Vendor::Claude, &f, &p),
        Err(CompileRefusal::InvalidFacts(_))
    ));
    let mut bad = p.clone();
    bad.rules
        .push(r(Capability::ShellExec, Effect::Deny, argv(&[])));
    assert!(matches!(
        compile(Vendor::Claude, &facts(Vendor::Claude), &bad),
        Err(CompileRefusal::InvalidPolicy(_))
    ));
}

// ---------------------------------------------------------------- determinism + goldens

fn render(res: &Result<CompiledPlan, CompileRefusal>) -> String {
    let mut s = String::new();
    match res {
        Err(e) => {
            s.push_str(&format!("REFUSED: {e}\n"));
        }
        Ok(p) => {
            s.push_str("== launch_args ==\n");
            for a in &p.launch_args {
                s.push_str(a);
                s.push('\n');
            }
            for f in &p.config_files {
                let (base, rel) = f.target.location();
                s.push_str(&format!(
                    "== file {:?} {:?} ({base:?}:{rel}) ==\n{}",
                    f.target, f.format, f.content
                ));
                if !f.content.ends_with('\n') {
                    s.push('\n');
                }
            }
            s.push_str("== receipts ==\n");
            for l in &p.receipts {
                s.push_str(&format!(
                    "[{:?}/{:?}] {:?} {:?}: {} -> {} | {}\n",
                    l.class,
                    l.evidence,
                    l.subject,
                    l.origin,
                    l.requested,
                    l.native.join(" ; "),
                    l.note
                ));
            }
            s.push_str("== change classes ==\n");
            for c in &p.change_classes {
                s.push_str(&format!("{:?} {:?}\n", c.field, c.class));
            }
            s.push_str(&format!("requires_respawn {}\n", p.requires_respawn));
        }
    }
    s
}

fn golden_path(v: Vendor, p: Preset) -> String {
    format!(
        "{}/src/permissions/golden/{}_{}.txt",
        env!("CARGO_MANIFEST_DIR"),
        v.as_str(),
        p.as_str()
    )
}

#[test]
fn compile_is_deterministic() {
    for v in Vendor::ALL {
        for pr in Preset::ALL {
            let p = pr.policy();
            let a = compile(v, &facts(v), &p);
            let b = compile(v, &facts(v), &p);
            assert_eq!(a, b);
            assert_eq!(render(&a), render(&b));
            if let (Ok(a), Ok(b)) = (&a, &b) {
                assert_eq!(json(a), json(b));
            }
        }
    }
}

#[test]
fn presets_match_their_goldens() {
    let goldens: [(Vendor, Preset, &str); 20] = [
        (
            Vendor::Claude,
            Preset::ReadOnly,
            include_str!("golden/claude_read-only.txt"),
        ),
        (
            Vendor::Claude,
            Preset::Workspace,
            include_str!("golden/claude_workspace.txt"),
        ),
        (
            Vendor::Claude,
            Preset::WorkspaceWeb,
            include_str!("golden/claude_workspace-web.txt"),
        ),
        (
            Vendor::Claude,
            Preset::Custom,
            include_str!("golden/claude_custom.txt"),
        ),
        (
            Vendor::Codex,
            Preset::ReadOnly,
            include_str!("golden/codex_read-only.txt"),
        ),
        (
            Vendor::Codex,
            Preset::Workspace,
            include_str!("golden/codex_workspace.txt"),
        ),
        (
            Vendor::Codex,
            Preset::WorkspaceWeb,
            include_str!("golden/codex_workspace-web.txt"),
        ),
        (
            Vendor::Codex,
            Preset::Custom,
            include_str!("golden/codex_custom.txt"),
        ),
        (
            Vendor::Muse,
            Preset::ReadOnly,
            include_str!("golden/muse_read-only.txt"),
        ),
        (
            Vendor::Muse,
            Preset::Workspace,
            include_str!("golden/muse_workspace.txt"),
        ),
        (
            Vendor::Muse,
            Preset::WorkspaceWeb,
            include_str!("golden/muse_workspace-web.txt"),
        ),
        (
            Vendor::Muse,
            Preset::Custom,
            include_str!("golden/muse_custom.txt"),
        ),
        (
            Vendor::Grok,
            Preset::ReadOnly,
            include_str!("golden/grok_read-only.txt"),
        ),
        (
            Vendor::Grok,
            Preset::Workspace,
            include_str!("golden/grok_workspace.txt"),
        ),
        (
            Vendor::Grok,
            Preset::WorkspaceWeb,
            include_str!("golden/grok_workspace-web.txt"),
        ),
        (
            Vendor::Grok,
            Preset::Custom,
            include_str!("golden/grok_custom.txt"),
        ),
        (
            Vendor::Antigravity,
            Preset::ReadOnly,
            include_str!("golden/antigravity_read-only.txt"),
        ),
        (
            Vendor::Antigravity,
            Preset::Workspace,
            include_str!("golden/antigravity_workspace.txt"),
        ),
        (
            Vendor::Antigravity,
            Preset::WorkspaceWeb,
            include_str!("golden/antigravity_workspace-web.txt"),
        ),
        (
            Vendor::Antigravity,
            Preset::Custom,
            include_str!("golden/antigravity_custom.txt"),
        ),
    ];
    for (v, p, want) in goldens {
        let got = render(&compile(v, &facts(v), &p.policy()));
        assert_eq!(
            got,
            want,
            "golden {} {} differs; run the ignored `bless_goldens` test to review",
            v.as_str(),
            p.as_str()
        );
    }
}

/// `cargo test -p fl-core bless_goldens -- --ignored` rewrites the golden files.
#[test]
#[ignore = "rewrites the golden files"]
fn bless_goldens() {
    for v in Vendor::ALL {
        for p in Preset::ALL {
            let got = render(&compile(v, &facts(v), &p.policy()));
            std::fs::write(golden_path(v, p), got).unwrap();
        }
    }
    let plan = ok(Vendor::Claude, &Preset::Workspace.policy());
    std::fs::write(
        format!(
            "{}/src/permissions/golden/claude_workspace.plan.json",
            env!("CARGO_MANIFEST_DIR")
        ),
        serde_json::to_string_pretty(&plan).unwrap() + "\n",
    )
    .unwrap();
}

#[test]
fn compiled_plan_wire_shape_matches_its_golden() {
    let plan = ok(Vendor::Claude, &Preset::Workspace.policy());
    let want = include_str!("golden/claude_workspace.plan.json");
    assert_eq!(serde_json::to_string_pretty(&plan).unwrap() + "\n", want);
    let back: CompiledPlan = serde_json::from_str(want).unwrap();
    assert_eq!(back, plan);
}

#[test]
fn symbolic_targets_never_hold_a_tilde() {
    for t in [
        ConfigTarget::ClaudeSettings,
        ConfigTarget::CodexConfig,
        ConfigTarget::CodexRules,
    ] {
        let (_, rel) = t.location();
        assert!(!rel.contains('~') && !rel.starts_with('/'), "{rel}");
    }
    for v in Vendor::ALL {
        for p in Preset::ALL {
            if let Ok(plan) = compile(v, &facts(v), &p.policy()) {
                assert!(plan.env.is_empty(), "no env, so no secret can be there");
                for a in &plan.launch_args {
                    assert!(!a.starts_with('~'));
                }
            }
        }
    }
}

// ---------------------------------------------------------------- safety properties

/// The allow constructs a plan contains, by vendor. `ignore_mode`: skip the sandbox mode that
/// the posture itself selects.
fn allow_constructs(v: Vendor, plan: &CompiledPlan, posture: Posture) -> Vec<String> {
    let mut out = Vec::new();
    match v {
        Vendor::Claude => out.extend(claude_raw(plan, "allow")),
        Vendor::Grok => {
            for w in plan.launch_args.windows(2) {
                if w[0] == "--allow" || w[0] == "--add-dir" {
                    out.push(w[1].clone());
                }
            }
        }
        Vendor::Codex => {
            if let Some(rules) = file(plan, ConfigTarget::CodexRules) {
                out.extend(
                    rules
                        .lines()
                        .filter(|l| l.contains("decision=\"allow\""))
                        .map(str::to_owned),
                );
            }
            if let Some(cfg) = file(plan, ConfigTarget::CodexConfig) {
                for l in cfg.lines() {
                    if l.starts_with("network_access = true") {
                        out.push("net".into());
                    }
                    if l.starts_with("writable_roots = [\"") {
                        out.push(l.to_owned());
                    }
                }
            }
            if has_arg(plan, "--search") {
                out.push("search".into());
            }
            if plan
                .launch_args
                .iter()
                .any(|a| a.contains("disk-full-read-access"))
            {
                out.push("read-all".into());
            }
            if posture == Posture::Ask && pair(plan, "-s", "workspace-write") {
                out.push("workspace-write".into());
            }
        }
        Vendor::Muse => {
            if pair(plan, "--permission-profile", "ws") {
                out.push("ws-profile".into());
            }
            if pair(plan, "--sandbox-network", "enabled")
                || pair(plan, "--sandbox-network", "proxy-only")
            {
                out.push("net".into());
            }
        }
        Vendor::Antigravity => {
            if let Some(i) = plan.launch_args.iter().position(|a| a == "--settings") {
                let s: serde_json::Value = serde_json::from_str(&plan.launch_args[i + 1]).unwrap();
                if let Some(a) = s["permissions"]["allow"].as_array() {
                    out.extend(a.iter().map(|x| x.as_str().unwrap().to_owned()));
                }
                if s["allowNonWorkspaceAccess"] == serde_json::Value::Bool(true) {
                    out.push("nonworkspace".into());
                }
            }
        }
    }
    out
}

/// An independent oracle: may a lone `allow` of this rule produce an allow construct at all?
fn allow_supported(v: Vendor, posture: Posture, cap: Capability, pat: &Option<Pattern>) -> bool {
    use Capability::*;
    let safe_argv = |p: &Option<Pattern>| match p {
        Some(Pattern::ArgvPrefix { argv }) => argv.iter().all(|t| {
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_./:=@%+,-".contains(c))
        }),
        _ => true,
    };
    let claude_like = match cap {
        FsReadWorkspace | FsWriteWorkspace | FsWriteProtected => true,
        FsReadExternal | FsWriteExternal => pat.is_some(),
        ShellExec => safe_argv(pat),
        NetworkWeb => !matches!(pat, Some(Pattern::Domain { domain }) if domain.starts_with("*.")),
        McpToolCall => pat.is_some(),
    };
    match v {
        Vendor::Claude | Vendor::Grok => claude_like,
        Vendor::Codex => match cap {
            FsWriteWorkspace => pat.is_none(),
            FsReadExternal => pat.is_none(),
            FsWriteExternal => {
                matches!(pat, Some(Pattern::AbsPathGlob { glob }) if glob == "/opt/data/**")
                    && posture != Posture::Ask
            }
            ShellExec => matches!(pat, Some(Pattern::ArgvPrefix { .. })),
            NetworkWeb => pat.is_none() && posture != Posture::Ask,
            _ => false,
        },
        Vendor::Muse => match cap {
            FsWriteWorkspace => pat.is_none(),
            NetworkWeb => true,
            _ => false,
        },
        Vendor::Antigravity => match cap {
            ShellExec => match pat {
                Some(Pattern::ArgvPrefix { argv }) => {
                    argv == &["git".to_owned(), "status".to_owned()]
                }
                Some(Pattern::ShellGlob { .. }) => true,
                _ => false,
            },
            _ => false,
        },
    }
}

#[test]
fn an_uncompilable_allow_never_compiles_to_an_allow() {
    let mut supported_seen = 0;
    for v in Vendor::ALL {
        for posture in [Posture::Ask, Posture::AcceptEdits] {
            for cap in Capability::ALL {
                for pat in fixtures(cap) {
                    let policy = pol(posture, &[r(cap, Effect::Allow, pat.clone())]);
                    let plan = compile(v, &facts(v), &policy).unwrap_or_else(|e| {
                        panic!("a lone allow never refuses: {v:?} {cap:?} {pat:?}: {e}")
                    });
                    let constructs = allow_constructs(v, &plan, posture);
                    let want = allow_supported(v, posture, cap, &pat);
                    // A pre-existing mode (accept-edits workspace-write, default network) is
                    // not an allow construct of this rule.
                    if want {
                        supported_seen += 1;
                        if !(v == Vendor::Codex && cap == Capability::FsWriteWorkspace) {
                            assert!(
                                !constructs.is_empty()
                                    || matches!(cap, Capability::FsReadWorkspace),
                                "{v:?} {posture:?} {cap:?} {pat:?}: expected a native allow, got {constructs:?}"
                            );
                        }
                    } else {
                        assert!(
                            constructs.is_empty(),
                            "{v:?} {posture:?} {cap:?} {pat:?}: an uncompilable allow became {constructs:?}"
                        );
                        let l = plan
                            .receipts
                            .iter()
                            .find(|l| l.effect == Some(Effect::Allow) && l.origin == Origin::Rule);
                        let l = l.expect("every rule has a receipt line");
                        // A baseline or posture-granted allow is Exact with no construct of its own.
                        if !(l.class == ReceiptClass::Exact && l.native.is_empty()) {
                            assert!(
                                matches!(
                                    l.class,
                                    ReceiptClass::StricterDowngrade | ReceiptClass::Unsupported
                                ),
                                "{l:?}"
                            );
                        }
                    }
                }
            }
        }
    }
    assert!(
        supported_seen > 40,
        "the oracle must exercise real allows ({supported_seen})"
    );
}

#[test]
fn a_dropped_allow_names_its_fallback_by_the_facts() {
    let p = pol(
        Posture::Ask,
        &[r(Capability::McpToolCall, Effect::Allow, mcp("srv", None))],
    );
    let no = compile(Vendor::Codex, &facts(Vendor::Codex), &p).unwrap();
    assert!(
        line_for(&no, "allow mcp_tool_call")
            .note
            .contains("treated as denied")
    );
    let yes = compile(
        Vendor::Codex,
        &facts(Vendor::Codex).with(ProvenFact::InteractiveAsk),
        &p,
    )
    .unwrap();
    assert!(
        line_for(&yes, "allow mcp_tool_call")
            .note
            .contains("native prompt")
    );
}

#[test]
fn rules_never_produce_bypass_or_sandbox_off() {
    let pool = all_rules();
    let mut rng = Rng(7);
    let mut compiled = 0;
    for v in Vendor::ALL {
        for posture in [
            Posture::Ask,
            Posture::AcceptEdits,
            Posture::Auto,
            Posture::Plan,
        ] {
            for _ in 0..120 {
                let n = 1 + rng.below(5);
                let rules: Vec<Rule> = (0..n)
                    .map(|_| pool[rng.below(pool.len())].clone())
                    .collect();
                let mut policy = pol(posture, &rules);
                policy.sandbox = SandboxChoice::VendorDefault;
                match compile(v, &facts(v), &policy) {
                    Ok(plan) => {
                        compiled += 1;
                        assert!(
                            !bypass_construct(&plan),
                            "{v:?} {posture:?} {rules:?}\n{}",
                            render(&Ok(plan.clone()))
                        );
                        assert!(!sandbox_off_construct(&plan), "{v:?} {posture:?} {rules:?}");
                    }
                    Err(CompileRefusal::InternalInvariant(m)) => {
                        panic!("audit tripped: {m} for {v:?} {rules:?}")
                    }
                    Err(_) => {}
                }
                // And with the sandbox off the flag is allowed, but a bypass still is not.
                policy.sandbox = SandboxChoice::Off;
                match compile(v, &facts(v), &policy) {
                    Ok(plan) => assert!(!bypass_construct(&plan), "{v:?} {posture:?} {rules:?}"),
                    Err(CompileRefusal::InternalInvariant(m)) => panic!("audit tripped: {m}"),
                    Err(_) => {}
                }
            }
        }
    }
    assert!(
        compiled > 300,
        "the run must compile real plans ({compiled})"
    );
}

#[test]
fn bypass_flags_exist_only_for_the_bypass_posture_and_follow_it() {
    for v in Vendor::ALL {
        let plan = ok(v, &pol(Posture::Bypass, &[]));
        assert!(
            bypass_construct(&plan),
            "{v:?} bypass must be visible: {:?}",
            plan.launch_args
        );
        assert!(
            !sandbox_off_construct(&plan),
            "{v:?} bypass keeps the sandbox axis separate"
        );
        // Sandbox off adds the sandbox construct and nothing else.
        let mut off = pol(Posture::Bypass, &[]);
        off.sandbox = SandboxChoice::Off;
        let plan = ok(v, &off);
        assert!(bypass_construct(&plan));
    }
    let mut off = pol(Posture::Bypass, &[]);
    off.sandbox = SandboxChoice::Off;
    assert!(has_arg(&ok(Vendor::Muse, &off), "--yolo"));
    assert!(has_arg(
        &ok(Vendor::Muse, &pol(Posture::Bypass, &[])),
        "--disable-approval"
    ));
    assert!(!has_arg(
        &ok(Vendor::Muse, &pol(Posture::Bypass, &[])),
        "--yolo"
    ));
    let c = ok(Vendor::Claude, &pol(Posture::Bypass, &[]));
    assert!(
        pair(&c, "--permission-mode", "bypassPermissions")
            && has_arg(&c, "--allow-dangerously-skip-permissions")
    );
    assert!(
        !has_arg(&c, "--dangerously-skip-permissions"),
        "the offer, not the skip flag"
    );
    let x = ok(Vendor::Codex, &pol(Posture::Bypass, &[]));
    assert!(pair(&x, "-a", "never") && pair(&x, "-s", "workspace-write"));
    assert!(pair(&ok(Vendor::Codex, &off), "-s", "danger-full-access"));
    assert!(has_arg(
        &ok(Vendor::Antigravity, &pol(Posture::Bypass, &[])),
        "--dangerously-skip-permissions"
    ));
    assert!(pair(
        &ok(Vendor::Grok, &pol(Posture::Bypass, &[])),
        "--permission-mode",
        "bypassPermissions"
    ));
}

#[test]
fn bypass_with_any_deny_or_ask_is_refused_on_every_vendor() {
    for v in Vendor::ALL {
        for cap in Capability::ALL {
            for pat in fixtures(cap) {
                for eff in [Effect::Deny, Effect::Ask] {
                    let e = compile(
                        v,
                        &facts(v),
                        &pol(Posture::Bypass, &[r(cap, eff, pat.clone())]),
                    )
                    .unwrap_err();
                    assert!(
                        matches!(
                            e,
                            CompileRefusal::InvalidPolicy(
                                PolicyError::BypassWithRestrictions { .. }
                            )
                        ),
                        "{v:?} {cap:?} {eff:?}: {e}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_deny_is_enforced_or_refused_never_unsupported_or_asked() {
    let mut refused = 0;
    let mut compiled = 0;
    for v in Vendor::ALL {
        for cap in Capability::ALL {
            for pat in fixtures(cap) {
                let policy = pol(Posture::Ask, &[r(cap, Effect::Deny, pat.clone())]);
                match compile(v, &facts(v), &policy) {
                    Err(CompileRefusal::UnenforceableDeny { .. }) => refused += 1,
                    Err(e) => panic!("{v:?} {cap:?} {pat:?}: unexpected {e}"),
                    Ok(plan) => {
                        compiled += 1;
                        for l in plan.receipts.iter().filter(|l| l.origin == Origin::Rule) {
                            assert_eq!(l.effect, Some(Effect::Deny));
                            assert_ne!(
                                l.class,
                                ReceiptClass::Unsupported,
                                "{v:?} {cap:?} {pat:?}: {l:?}"
                            );
                        }
                        assert!(
                            allow_constructs(v, &plan, Posture::Plan).is_empty()
                                || v == Vendor::Codex
                                || v == Vendor::Muse,
                            "{v:?} {cap:?} {pat:?}"
                        );
                    }
                }
            }
        }
    }
    assert!(
        refused > 20 && compiled > 20,
        "{refused} refused, {compiled} compiled"
    );
}

#[test]
fn an_unenforceable_deny_refuses_the_whole_plan() {
    use Capability::*;
    // Antigravity has no native deny at all.
    for cap in [
        ShellExec,
        NetworkWeb,
        McpToolCall,
        FsWriteProtected,
        FsReadExternal,
        FsWriteExternal,
        FsReadWorkspace,
        FsWriteWorkspace,
    ] {
        let e = refusal(
            Vendor::Antigravity,
            &pol(Posture::Ask, &[r(cap, Effect::Deny, None)]),
        );
        assert!(
            matches!(
                e,
                CompileRefusal::UnenforceableDeny {
                    vendor: Vendor::Antigravity,
                    ..
                }
            ),
            "{cap:?}: {e}"
        );
    }
    // A proven hard deny lifts it.
    let f = facts(Vendor::Antigravity).with(ProvenFact::HardDeny(NetworkWeb));
    assert!(
        compile(
            Vendor::Antigravity,
            &f,
            &pol(Posture::Ask, &[r(NetworkWeb, Effect::Deny, None)])
        )
        .is_ok()
    );
    // Claude: every external path cannot be separated from the workspace.
    assert!(matches!(
        refusal(
            Vendor::Claude,
            &pol(Posture::Ask, &[r(FsWriteExternal, Effect::Deny, None)])
        ),
        CompileRefusal::UnenforceableDeny { .. }
    ));
    assert!(matches!(
        refusal(
            Vendor::Grok,
            &pol(Posture::Ask, &[r(FsReadExternal, Effect::Deny, None)])
        ),
        CompileRefusal::UnenforceableDeny { .. }
    ));
    assert!(matches!(
        refusal(
            Vendor::Claude,
            &pol(Posture::Ask, &[r(McpToolCall, Effect::Deny, None)])
        ),
        CompileRefusal::UnenforceableDeny { .. }
    ));
    // A deny whose first token cannot be stated.
    assert!(matches!(
        refusal(
            Vendor::Claude,
            &pol(
                Posture::Ask,
                &[r(ShellExec, Effect::Deny, argv(&["a b", "x"]))]
            )
        ),
        CompileRefusal::UnenforceableDeny { .. }
    ));
    // Codex: no deny-all-commands, no read hiding, no MCP deny without proof.
    for rule in [
        r(ShellExec, Effect::Deny, None),
        r(ShellExec, Effect::Deny, sg("*x")),
        r(FsReadWorkspace, Effect::Deny, pg("secret/**")),
        r(McpToolCall, Effect::Deny, mcp("s", None)),
    ] {
        assert!(
            matches!(
                refusal(
                    Vendor::Codex,
                    &pol(Posture::Ask, std::slice::from_ref(&rule))
                ),
                CompileRefusal::UnenforceableDeny { .. }
            ),
            "{rule:?}"
        );
    }
    // Codex with the sandbox off has no boundary for any fs or network deny.
    for rule in [
        r(FsWriteWorkspace, Effect::Deny, pg("a/**")),
        r(FsWriteExternal, Effect::Deny, ag("/etc/**")),
        r(FsReadExternal, Effect::Deny, None),
        r(NetworkWeb, Effect::Deny, None),
        r(FsWriteProtected, Effect::Deny, None),
    ] {
        let mut p = pol(Posture::Ask, std::slice::from_ref(&rule));
        p.sandbox = SandboxChoice::Off;
        assert!(
            matches!(
                refusal(Vendor::Codex, &p),
                CompileRefusal::UnenforceableDeny { .. }
            ),
            "{rule:?}"
        );
    }
    let mut p = pol(Posture::Ask, &[r(NetworkWeb, Effect::Deny, None)]);
    p.sandbox = SandboxChoice::Off;
    assert!(matches!(
        refusal(Vendor::Muse, &p),
        CompileRefusal::UnenforceableDeny { .. }
    ));
    // Muse: a scoped write deny needs a verified read-only profile.
    let bare = VendorFacts::new(Transport::Pty);
    let p = pol(
        Posture::Ask,
        &[r(FsWriteWorkspace, Effect::Deny, pg("src/**"))],
    );
    assert!(matches!(
        compile(Vendor::Muse, &bare, &p),
        Err(CompileRefusal::UnenforceableDeny { .. })
    ));
    assert!(compile(Vendor::Muse, &facts(Vendor::Muse), &p).is_ok());
    assert!(matches!(
        compile(Vendor::Muse, &bare, &pol(Posture::Plan, &[])),
        Err(CompileRefusal::PostureNotEnforceable { .. })
    ));
}

#[test]
fn a_deny_never_degrades_to_an_ask_on_any_vendor() {
    // For each vendor the deny is either a native deny/forbidden entry, a strictly stronger
    // restriction, or a refusal; never an ask entry.
    let p = pol(
        Posture::Ask,
        &[r(
            Capability::ShellExec,
            Effect::Deny,
            argv(&["git", "push"]),
        )],
    );
    let c = ok(Vendor::Claude, &p);
    assert!(claude_list(&c, "ask").is_empty());
    assert_eq!(
        claude_list(&c, "deny"),
        ["Bash(git push *)", "Bash(git push)"]
    );
    let x = ok(Vendor::Codex, &p);
    assert!(
        file(&x, ConfigTarget::CodexRules)
            .unwrap()
            .contains("decision=\"forbidden\"")
    );
    assert!(
        !file(&x, ConfigTarget::CodexRules)
            .unwrap()
            .contains("prompt")
    );
    let g = ok(Vendor::Grok, &p);
    assert!(pair(&g, "--deny", "Bash(git push)") && !has_arg(&g, "--allow"));
    for v in [Vendor::Muse, Vendor::Antigravity] {
        assert!(
            matches!(refusal(v, &p), CompileRefusal::UnenforceableDeny { .. }),
            "{v:?}"
        );
    }
}

#[test]
fn an_ask_that_cannot_be_stated_withdraws_the_allows_it_would_lose_to() {
    // Grok has no ask list: `git push` ask must not coexist with a `git` allow.
    let p = pol(
        Posture::Ask,
        &[
            r(Capability::ShellExec, Effect::Allow, argv(&["git"])),
            r(Capability::ShellExec, Effect::Ask, argv(&["git", "push"])),
        ],
    );
    let g = ok(Vendor::Grok, &p);
    assert!(!has_arg(&g, "--allow"), "{:?}", g.launch_args);
    assert_eq!(
        line_for(&g, "allow shell_exec").class,
        ReceiptClass::StricterDowngrade
    );
    // Claude states it natively and keeps both.
    let c = ok(Vendor::Claude, &p);
    assert_eq!(
        claude_list(&c, "ask"),
        ["Bash(git push *)", "Bash(git push)"]
    );
    assert_eq!(claude_list(&c, "allow"), ["Bash(git *)", "Bash(git)"]);
    // Codex: `prompt` beats `allow` in execpolicy; both are kept.
    let x = ok(Vendor::Codex, &p);
    let rules = file(&x, ConfigTarget::CodexRules).unwrap();
    assert!(
        rules.contains("pattern=[\"git\"], decision=\"allow\"")
            && rules.contains("decision=\"prompt\"")
    );
    // Antigravity: matching commands leave the enumeration.
    let mut f = facts(Vendor::Antigravity);
    f.exact_commands = vec!["git status".into(), "git push".into()];
    let a = compile(Vendor::Antigravity, &f, &p).unwrap();
    let s = a.launch_args.iter().find(|x| x.starts_with('{')).unwrap();
    assert!(
        s.contains("command(git status)") && !s.contains("command(git push)"),
        "{s}"
    );
    // An ask the sandbox cannot make a prompt at all is refused rather than run.
    let mut off = pol(
        Posture::Ask,
        &[r(Capability::FsWriteExternal, Effect::Ask, ag("/opt/x/**"))],
    );
    off.sandbox = SandboxChoice::Off;
    assert!(matches!(
        refusal(Vendor::Codex, &off),
        CompileRefusal::UnenforceableAsk { .. }
    ));
    assert!(matches!(
        refusal(
            Vendor::Codex,
            &pol(
                Posture::Ask,
                &[r(Capability::FsReadWorkspace, Effect::Ask, None)]
            )
        ),
        CompileRefusal::UnenforceableAsk { .. }
    ));
}

// ---------------------------------------------------------------- named fixtures

#[test]
fn fixture_deny_fallback() {
    // Claude: a wildcard-domain deny widens to every fetch, receipted.
    let p = pol(
        Posture::Ask,
        &[r(
            Capability::NetworkWeb,
            Effect::Deny,
            dom("*.evil.example"),
        )],
    );
    let c = ok(Vendor::Claude, &p);
    assert_eq!(claude_list(&c, "deny"), ["WebFetch"]);
    assert_eq!(
        line_for(&c, "deny network_web").class,
        ReceiptClass::ToolScopedEmulation
    );
    // Muse: a domain deny becomes the restricted mode.
    let m = ok(Vendor::Muse, &p);
    assert!(pair(&m, "--sandbox-network", "restricted"));
    assert_eq!(
        line_for(&m, "deny network_web").class,
        ReceiptClass::StricterDowngrade
    );
    // Codex: a path deny in the workspace becomes the read-only sandbox, never a wider one.
    let w = pol(
        Posture::AcceptEdits,
        &[r(
            Capability::FsWriteWorkspace,
            Effect::Deny,
            pg("secrets/**"),
        )],
    );
    let x = ok(Vendor::Codex, &w);
    assert!(pair(&x, "-s", "read-only") && !has_arg(&x, "danger-full-access"));
    assert_eq!(
        line_for(&x, "deny fs_write_workspace").class,
        ReceiptClass::StricterDowngrade
    );
    // Codex: a string-glob deny keeps its literal leading tokens.
    let g = pol(
        Posture::Ask,
        &[r(Capability::ShellExec, Effect::Deny, sg("git push*"))],
    );
    let x = ok(Vendor::Codex, &g);
    assert!(
        file(&x, ConfigTarget::CodexRules)
            .unwrap()
            .contains("prefix_rule(pattern=[\"git\"], decision=\"forbidden\")")
    );
    assert_eq!(
        line_for(&x, "deny shell_exec").class,
        ReceiptClass::StricterDowngrade
    );
}

#[test]
fn fixture_codex_never_widens_to_satisfy_a_narrow_policy() {
    for posture in [
        Posture::Ask,
        Posture::AcceptEdits,
        Posture::Auto,
        Posture::Plan,
    ] {
        for rule in all_rules() {
            if let Ok(plan) = compile(
                Vendor::Codex,
                &facts(Vendor::Codex),
                &pol(posture, std::slice::from_ref(&rule)),
            ) {
                assert!(
                    !has_arg(&plan, "danger-full-access"),
                    "{posture:?} {rule:?}"
                );
            }
        }
    }
    // Narrow external write allow: an exact subtree becomes a root, anything else is dropped.
    let p = pol(
        Posture::AcceptEdits,
        &[
            r(
                Capability::FsWriteExternal,
                Effect::Allow,
                ag("/data/out/**"),
            ),
            r(
                Capability::FsWriteExternal,
                Effect::Allow,
                ag("/data/*.log"),
            ),
            r(Capability::FsWriteExternal, Effect::Allow, None),
        ],
    );
    let x = ok(Vendor::Codex, &p);
    let cfg = file(&x, ConfigTarget::CodexConfig).unwrap();
    assert!(cfg.contains("writable_roots = [\"/data/out\"]"), "{cfg}");
    assert!(pair(&x, "-s", "workspace-write"));
    // A deny that overlaps a root withdraws the root.
    let p = pol(
        Posture::AcceptEdits,
        &[
            r(
                Capability::FsWriteExternal,
                Effect::Allow,
                ag("/data/out/**"),
            ),
            r(
                Capability::FsWriteExternal,
                Effect::Deny,
                ag("/data/out/secret/**"),
            ),
        ],
    );
    let x = ok(Vendor::Codex, &p);
    assert!(
        file(&x, ConfigTarget::CodexConfig)
            .unwrap()
            .contains("writable_roots = []")
    );
    // Plan with the sandbox off is a conflict, not a silent widening.
    let mut off = pol(Posture::Plan, &[]);
    off.sandbox = SandboxChoice::Off;
    assert!(matches!(
        refusal(Vendor::Codex, &off),
        CompileRefusal::SandboxConflict { .. }
    ));
}

#[test]
fn fixture_codex_network_splits_three_ways() {
    let p = pol(
        Posture::AcceptEdits,
        &[r(Capability::NetworkWeb, Effect::Allow, None)],
    );
    let x = ok(Vendor::Codex, &p);
    let legs: Vec<_> = x
        .receipts
        .iter()
        .filter(|l| l.requested.contains('['))
        .collect();
    assert_eq!(legs.len(), 3);
    assert!(legs[0].requested.ends_with("[sandbox network transport]"));
    assert!(legs[1].requested.ends_with("[host approvals]"));
    assert!(legs[2].requested.ends_with("[WebSearch tool]"));
    assert!(
        file(&x, ConfigTarget::CodexConfig)
            .unwrap()
            .contains("network_access = true")
            && has_arg(&x, "--search")
    );
    // A domain allow grants nothing: host approvals are runtime decisions, not config.
    let d = ok(
        Vendor::Codex,
        &pol(
            Posture::AcceptEdits,
            &[r(Capability::NetworkWeb, Effect::Allow, dom("example.com"))],
        ),
    );
    assert!(
        file(&d, ConfigTarget::CodexConfig)
            .unwrap()
            .contains("network_access = false")
    );
    assert!(!has_arg(&d, "--search"));
    assert_eq!(
        d.receipts
            .iter()
            .filter(|l| l.requested.contains('['))
            .count(),
        3
    );
    // A deny turns the search tool off too.
    let n = ok(
        Vendor::Codex,
        &pol(
            Posture::AcceptEdits,
            &[r(Capability::NetworkWeb, Effect::Deny, None)],
        ),
    );
    assert!(
        file(&n, ConfigTarget::CodexConfig)
            .unwrap()
            .contains("web_search = \"disabled\"")
    );
}

#[test]
fn fixture_codex_mcp_is_classified_not_substituted() {
    let allow = ok(
        Vendor::Codex,
        &pol(
            Posture::Ask,
            &[r(
                Capability::McpToolCall,
                Effect::Allow,
                mcp("srv", Some("t")),
            )],
        ),
    );
    assert_eq!(
        line_for(&allow, "allow mcp_tool_call").class,
        ReceiptClass::StricterDowngrade
    );
    let f = facts(Vendor::Codex).with(ProvenFact::HardDeny(Capability::McpToolCall));
    let d = compile(
        Vendor::Codex,
        &f,
        &pol(
            Posture::Ask,
            &[r(
                Capability::McpToolCall,
                Effect::Deny,
                mcp("srv", Some("t")),
            )],
        ),
    )
    .unwrap();
    assert!(
        file(&d, ConfigTarget::CodexConfig)
            .unwrap()
            .contains("[mcp_servers.srv]\nenabled = false")
    );
    assert_eq!(
        line_for(&d, "deny mcp_tool_call").class,
        ReceiptClass::StricterDowngrade,
        "disabling the server is stricter than one tool"
    );
    let s = compile(
        Vendor::Codex,
        &f,
        &pol(
            Posture::Ask,
            &[r(Capability::McpToolCall, Effect::Deny, mcp("srv", None))],
        ),
    )
    .unwrap();
    assert_eq!(
        line_for(&s, "deny mcp_tool_call").class,
        ReceiptClass::Exact
    );
    assert!(matches!(
        compile(
            Vendor::Codex,
            &f,
            &pol(
                Posture::Ask,
                &[r(Capability::McpToolCall, Effect::Deny, None)]
            )
        ),
        Err(CompileRefusal::UnenforceableDeny { .. })
    ));
}

#[test]
fn fixture_mixed_external_read_and_write() {
    use Capability::*;
    // Antigravity: one boolean for both.
    let both = pol(
        Posture::Ask,
        &[
            r(FsReadExternal, Effect::Allow, None),
            r(FsWriteExternal, Effect::Allow, None),
        ],
    );
    let a = ok(Vendor::Antigravity, &both);
    assert!(
        a.launch_args
            .iter()
            .any(|s| s.contains("\"allowNonWorkspaceAccess\":true"))
    );
    let one = pol(Posture::Ask, &[r(FsReadExternal, Effect::Allow, None)]);
    let a = ok(Vendor::Antigravity, &one);
    assert!(
        a.launch_args
            .iter()
            .any(|s| s.contains("\"allowNonWorkspaceAccess\":false")),
        "{:?}",
        a.launch_args
    );
    assert!(
        line_for(&a, "allow fs_read_external")
            .note
            .contains("also open the other")
    );
    let mixed = pol(
        Posture::Ask,
        &[
            r(FsReadExternal, Effect::Allow, None),
            r(FsWriteExternal, Effect::Ask, None),
        ],
    );
    let a = ok(Vendor::Antigravity, &mixed);
    assert!(
        a.launch_args
            .iter()
            .any(|s| s.contains("\"allowNonWorkspaceAccess\":false"))
    );
    assert_eq!(
        line_for(&a, "allow fs_read_external").class,
        ReceiptClass::StricterDowngrade
    );
    let deny = pol(
        Posture::Ask,
        &[
            r(FsReadExternal, Effect::Allow, None),
            r(FsWriteExternal, Effect::Deny, None),
        ],
    );
    assert!(matches!(
        refusal(Vendor::Antigravity, &deny),
        CompileRefusal::UnenforceableDeny { .. }
    ));
    // Codex: read-all and the write fence are independent mechanisms.
    let mixed = pol(
        Posture::AcceptEdits,
        &[
            r(FsReadExternal, Effect::Allow, None),
            r(FsWriteExternal, Effect::Deny, None),
        ],
    );
    let x = ok(Vendor::Codex, &mixed);
    assert!(
        x.launch_args
            .iter()
            .any(|a| a.contains("disk-full-read-access"))
    );
    assert_eq!(
        line_for(&x, "deny fs_write_external").class,
        ReceiptClass::Exact
    );
    // Codex: a read deny beats a read allow; the full-disk grant is withdrawn.
    let rd = pol(
        Posture::AcceptEdits,
        &[
            r(FsReadExternal, Effect::Allow, None),
            r(FsReadExternal, Effect::Deny, ag("/etc/**")),
        ],
    );
    let x = ok(Vendor::Codex, &rd);
    assert!(
        !x.launch_args
            .iter()
            .any(|a| a.contains("disk-full-read-access"))
    );
    // Claude: absolute globs are `//` rules; "all external" cannot be stated.
    let c = ok(
        Vendor::Claude,
        &pol(
            Posture::Ask,
            &[
                r(FsReadExternal, Effect::Allow, ag("/opt/data/**")),
                r(FsWriteExternal, Effect::Deny, ag("/etc/**")),
            ],
        ),
    );
    assert_eq!(claude_list(&c, "allow"), ["Read(//opt/data/**)"]);
    assert_eq!(
        claude_list(&c, "deny"),
        ["Edit(//etc/**)", "Write(//etc/**)"]
    );
    let c = ok(
        Vendor::Claude,
        &pol(Posture::Ask, &[r(FsReadExternal, Effect::Allow, None)]),
    );
    assert!(claude_list(&c, "allow").is_empty());
    // Grok: a root granted for both read and write is a launch root.
    let g = ok(
        Vendor::Grok,
        &pol(
            Posture::Ask,
            &[
                r(FsReadExternal, Effect::Allow, ag("/opt/shared/**")),
                r(FsWriteExternal, Effect::Allow, ag("/opt/shared/**")),
            ],
        ),
    );
    assert!(pair(&g, "--add-dir", "/opt/shared"));
    let g = ok(
        Vendor::Grok,
        &pol(
            Posture::Ask,
            &[r(FsWriteExternal, Effect::Allow, ag("/opt/shared/**"))],
        ),
    );
    assert!(!has_arg(&g, "--add-dir"));
}

#[test]
fn fixture_shell_writes_around_tool_rules() {
    // A protected-path deny on Claude is an Edit/Write emulation, and says so.
    let p = pol(
        Posture::Ask,
        &[r(
            Capability::FsWriteProtected,
            Effect::Deny,
            class(PathClass::Git),
        )],
    );
    let c = ok(Vendor::Claude, &p);
    assert_eq!(
        claude_raw(&c, "deny")
            .iter()
            .filter(|e| e.contains(".git/**"))
            .count(),
        2
    );
    assert_eq!(
        line_for(&c, "deny fs_write_protected").class,
        ReceiptClass::ToolScopedEmulation
    );
    // The defaults protect the other classes, emulated too.
    let defaults: Vec<_> = c
        .receipts
        .iter()
        .filter(|l| {
            l.origin == Origin::Default
                && l.subject == ReceiptSubject::Capability(Capability::FsWriteProtected)
        })
        .collect();
    assert_eq!(defaults.len(), 2);
    assert!(
        defaults
            .iter()
            .all(|l| l.class == ReceiptClass::ToolScopedEmulation)
    );
    // A policy that requires filesystem enforcement is refused...
    let mut req = p.clone();
    req.require_fs_enforcement = true;
    assert!(matches!(
        refusal(Vendor::Claude, &req),
        CompileRefusal::FsEnforcementRequired {
            class: ReceiptClass::ToolScopedEmulation,
            ..
        }
    ));
    assert!(matches!(
        refusal(Vendor::Grok, &req),
        CompileRefusal::FsEnforcementRequired { .. }
    ));
    // ...unless the facts prove an OS boundary covers every access path.
    let f = facts(Vendor::Claude).with(ProvenFact::FsEnforcementCoverage);
    assert!(compile(Vendor::Claude, &f, &req).is_ok());
    // Codex enforces a workspace write deny with the read-only sandbox: allowed.
    let mut sandboxed = pol(
        Posture::Ask,
        &[
            r(Capability::FsWriteWorkspace, Effect::Deny, None),
            r(Capability::FsWriteProtected, Effect::Deny, None),
        ],
    );
    sandboxed.require_fs_enforcement = true;
    let x = ok(Vendor::Codex, &sandboxed);
    assert!(pair(&x, "-s", "read-only"));
    // Workspace tool denies on Claude are tool-scoped as well.
    let w = ok(
        Vendor::Claude,
        &pol(
            Posture::Ask,
            &[r(
                Capability::FsWriteWorkspace,
                Effect::Deny,
                pg("secrets/**"),
            )],
        ),
    );
    assert_eq!(
        line_for(&w, "deny fs_write_workspace").class,
        ReceiptClass::ToolScopedEmulation
    );
    assert_eq!(
        line_for(&w, "deny fs_write_workspace").evidence,
        EvidenceClass::Documented
    );
}

#[test]
fn fixture_antigravity_enumeration() {
    let mut f = facts(Vendor::Antigravity);
    f.exact_commands = vec![
        "git status".into(),
        "git status -s".into(),
        "git push".into(),
        "git a;b".into(),
        "gitx".into(),
        "git  status".into(),
        "npm test".into(),
    ];
    let p = pol(
        Posture::Ask,
        &[
            r(
                Capability::ShellExec,
                Effect::Allow,
                argv(&["git", "status"]),
            ),
            r(Capability::ShellExec, Effect::Allow, sg("npm *")),
            r(Capability::ShellExec, Effect::Allow, None),
        ],
    );
    let a = compile(Vendor::Antigravity, &f, &p).unwrap();
    let s = a.launch_args.iter().find(|x| x.starts_with('{')).unwrap();
    assert_eq!(
        s,
        r#"{"allowNonWorkspaceAccess":false,"permissions":{"allow":["command(git status)","command(git status -s)","command(npm test)"]}}"#
    );
    assert!(!s.contains("*"), "no prefix or glob is ever emitted");
    assert_eq!(
        line_for(&a, "allow shell_exec argv").class,
        ReceiptClass::StricterDowngrade
    );
    assert!(
        line_for(&a, "allow shell_exec argv")
            .note
            .contains("everything else stays ask")
    );
}

#[test]
fn fixture_antigravity_unmanaged_and_modes() {
    let a = ok(Vendor::Antigravity, &pol(Posture::Plan, &[]));
    assert!(pair(&a, "--mode", "plan"));
    let l = line_for(&a, "artifactReviewPolicy");
    assert_eq!(l.class, ReceiptClass::Unsupported);
    assert!(l.note.contains("unmanaged"));
    assert!(
        a.launch_args
            .iter()
            .any(|x| x.contains("\"allowNonWorkspaceAccess\":false"))
    );
    assert!(
        !ok(Vendor::Antigravity, &pol(Posture::Ask, &[]))
            .launch_args
            .contains(&"--mode".to_owned())
    );
}

#[test]
fn fixture_muse_profiles_and_network() {
    // Workspace preset: the workspace profile, restricted network.
    let m = ok(Vendor::Muse, &Preset::Workspace.policy());
    assert!(
        pair(&m, "--permission-profile", "ws")
            && pair(&m, "--sandbox-network", "restricted")
            && pair(&m, "--approval-mode", "untrusted")
    );
    assert!(pair(&m, "--approval-judge", "off"));
    // Read-only preset: the read-only profile.
    let m = ok(Vendor::Muse, &Preset::ReadOnly.policy());
    assert!(pair(&m, "--permission-profile", "ro"));
    // Domain allows degrade to proxy-only, receipted.
    let m = ok(
        Vendor::Muse,
        &pol(
            Posture::Ask,
            &[r(
                Capability::NetworkWeb,
                Effect::Allow,
                dom("a.example.com"),
            )],
        ),
    );
    assert!(pair(&m, "--sandbox-network", "proxy-only"));
    assert_eq!(
        line_for(&m, "allow network_web").class,
        ReceiptClass::StricterDowngrade
    );
    let m = ok(
        Vendor::Muse,
        &pol(
            Posture::Ask,
            &[r(Capability::NetworkWeb, Effect::Allow, None)],
        ),
    );
    assert!(pair(&m, "--sandbox-network", "enabled"));
    // A pattern-scoped write allow gets the read-only fallback, not the workspace profile.
    let m = ok(
        Vendor::Muse,
        &pol(
            Posture::Ask,
            &[r(Capability::FsWriteWorkspace, Effect::Allow, pg("src/**"))],
        ),
    );
    assert!(
        pair(&m, "--permission-profile", "ro"),
        "{:?}",
        m.launch_args
    );
    // A write deny pins the read-only profile even beside a workspace allow.
    let m = ok(
        Vendor::Muse,
        &pol(
            Posture::Ask,
            &[
                r(Capability::FsWriteWorkspace, Effect::Allow, None),
                r(Capability::FsWriteWorkspace, Effect::Deny, pg("a/**")),
            ],
        ),
    );
    assert!(pair(&m, "--permission-profile", "ro"));
    // Auto does not enable the reviewer.
    let m = ok(Vendor::Muse, &pol(Posture::Auto, &[]));
    assert!(pair(&m, "--approval-judge", "off"));
    assert_eq!(
        line_for(&m, "posture").class,
        ReceiptClass::StricterDowngrade
    );
    // No profile in the facts: the plan says the vendor default governs.
    let bare = compile(
        Vendor::Muse,
        &VendorFacts::new(Transport::Pty),
        &Preset::Workspace.policy(),
    )
    .unwrap();
    assert!(!has_arg(&bare, "--permission-profile"));
    assert_eq!(
        line_for(&bare, "permission profile").class,
        ReceiptClass::Unsupported
    );
    // Sandbox off is its own flag.
    let mut off = pol(Posture::Ask, &[]);
    off.sandbox = SandboxChoice::Off;
    let m = ok(Vendor::Muse, &off);
    assert!(
        has_arg(&m, "--disable-sandbox")
            && !has_arg(&m, "--yolo")
            && !has_arg(&m, "--sandbox-network")
    );
}

#[test]
fn fixture_grok_reuses_the_claude_rules() {
    let rules = [
        r(
            Capability::ShellExec,
            Effect::Allow,
            argv(&["cargo", "test"]),
        ),
        r(Capability::NetworkWeb, Effect::Deny, None),
        r(Capability::FsWriteWorkspace, Effect::Deny, pg("secrets/**")),
    ];
    let p = pol(Posture::Ask, &rules);
    let c = ok(Vendor::Claude, &p);
    let g = ok(Vendor::Grok, &p);
    let from_args = |flag: &str| -> Vec<String> {
        user_only(
            g.launch_args
                .windows(2)
                .filter(|w| w[0] == flag)
                .map(|w| w[1].clone())
                .collect(),
        )
    };
    let mut d = claude_list(&c, "deny");
    d.sort();
    let mut gd = from_args("--deny");
    gd.sort();
    assert_eq!(gd, d);
    assert_eq!(
        from_args("--allow"),
        claude_list(&c, "allow")
            .into_iter()
            .collect::<Vec<_>>()
            .tap_sorted()
    );
    assert!(has_arg(&g, "--disable-web-search"));
    assert!(pair(&g, "--permission-mode", "default"));
    let mut f = facts(Vendor::Grok);
    f.sandbox_profile = Some("strict".into());
    let g = compile(Vendor::Grok, &f, &p).unwrap();
    assert!(pair(&g, "--sandbox", "strict"));
}

trait TapSorted {
    fn tap_sorted(self) -> Self;
}
impl TapSorted for Vec<String> {
    fn tap_sorted(mut self) -> Self {
        self.sort();
        self
    }
}

#[test]
fn claude_posture_and_files() {
    let expect = [
        (Posture::Ask, "manual"),
        (Posture::AcceptEdits, "acceptEdits"),
        (Posture::Auto, "auto"),
        (Posture::Plan, "plan"),
    ];
    for (p, m) in expect {
        let plan = ok(Vendor::Claude, &pol(p, &[]));
        assert!(pair(&plan, "--permission-mode", m));
        assert!(!has_arg(&plan, "--allow-dangerously-skip-permissions"));
    }
    let g = ok(Vendor::Grok, &pol(Posture::Ask, &[]));
    assert!(pair(&g, "--permission-mode", "default"));
    // The default settings protect the three classes and say so.
    let c = ok(Vendor::Claude, &pol(Posture::Ask, &[]));
    let deny = claude_raw(&c, "deny");
    for needle in [".claude/**", ".git/**", ".claude/settings.json"] {
        assert!(
            deny.contains(&format!("Edit({needle})")),
            "{needle} in {deny:?}"
        );
    }
    let (base, rel) = c.config_files[0].target.location();
    assert_eq!(
        (base, rel),
        (ConfigBase::Workspace, ".claude/settings.local.json")
    );
    // Bypass applies no defaults.
    assert!(
        ok(Vendor::Claude, &pol(Posture::Bypass, &[]))
            .config_files
            .is_empty()
    );
    // Claude sandbox off is unsupported, not silently dropped.
    let mut off = pol(Posture::Ask, &[]);
    off.sandbox = SandboxChoice::Off;
    assert_eq!(
        line_for(&ok(Vendor::Claude, &off), "sandbox off").class,
        ReceiptClass::Unsupported
    );
}

// ---------------------------------------------------------------- pattern escaping

/// A reference matcher for Claude `Bash(...)` entries: `prefix *` matches the prefix at a
/// token boundary followed by arguments free of shell operators; other entries match whole
/// or by `*` glob.
fn claude_bash_matches(inner: &str, cmd: &str) -> bool {
    const OPS: &str = ";|&<>$`\n()";
    if let Some(prefix) = inner.strip_suffix(" *") {
        let rest = if cmd == prefix {
            Some("")
        } else {
            cmd.strip_prefix(&format!("{prefix} "))
        };
        return rest.is_some_and(|r| !r.chars().any(|c| OPS.contains(c)));
    }
    if inner.contains('*') {
        return wild(inner, cmd);
    }
    cmd == inner
}

fn bash_inner(e: &str) -> Option<&str> {
    e.strip_prefix("Bash(")?.strip_suffix(')')
}

fn tokens_start_with(cmd: &str, want: &[String]) -> bool {
    let t: Vec<&str> = cmd.split(' ').collect();
    t.len() >= want.len() && t.iter().zip(want).all(|(a, b)| a == b)
}

#[test]
fn claude_shell_entries_never_match_more_than_requested() {
    let argvs: [&[&str]; 12] = [
        &["git", "status"],
        &["cargo", "test", "--", "--nocapture"],
        &["ls", "-la"],
        &["git", "-C", "x"],
        &["echo", "a b"],
        &["echo", "*"],
        &["echo", "a;b"],
        &["echo", "'q'"],
        &["echo", "\"dq\""],
        &["echo", "héllo"],
        &["echo", "$HOME"],
        &["rm", "-rf", "a*"],
    ];
    for a in argvs {
        let want: Vec<String> = a.iter().map(|s| (*s).to_owned()).collect();
        let joined = a.join(" ");
        let mut corpus = vec![
            joined.clone(),
            format!("{joined} extra"),
            format!("{joined}x"),
            format!("{joined}; rm -rf /"),
            format!("{joined} && rm -rf /"),
            format!("{joined} | sh"),
            format!("x {joined}"),
            joined.replacen(' ', "  ", 1),
            format!("{joined}$(id)"),
            format!("{joined} `id`"),
            a[..a.len() - 1].join(" "),
            a[0].to_owned(),
        ];
        corpus.push(format!("{} z", a[..a.len() - 1].join(" ")));
        for eff in [Effect::Allow, Effect::Ask, Effect::Deny] {
            let policy = pol(Posture::Ask, &[r(Capability::ShellExec, eff, argv(a))]);
            let res = compile(Vendor::Claude, &facts(Vendor::Claude), &policy);
            let Ok(plan) = res else {
                assert_eq!(eff, Effect::Deny, "only a deny may refuse: {a:?}");
                assert!(a[0].contains(' ') || a[0].contains('*'), "{a:?}");
                continue;
            };
            let list = claude_list(
                &plan,
                if eff == Effect::Allow {
                    "allow"
                } else if eff == Effect::Ask {
                    "ask"
                } else {
                    "deny"
                },
            );
            let entries: Vec<&str> = list.iter().filter_map(|e| bash_inner(e)).collect();
            let safe = a.iter().all(|t| {
                !t.is_empty()
                    && t.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_./:=@%+,-".contains(c))
            });
            if eff == Effect::Allow && !safe {
                assert!(
                    entries.is_empty(),
                    "an unquotable allow must not be emitted: {a:?} {entries:?}"
                );
                continue;
            }
            for cmd in &corpus {
                let hit = entries.iter().any(|e| claude_bash_matches(e, cmd));
                if eff == Effect::Allow {
                    if hit {
                        assert!(
                            tokens_start_with(cmd, &want),
                            "allow {entries:?} matched {cmd:?}, which is not an extension of {a:?}"
                        );
                        assert!(
                            !cmd.chars().any(|c| ";|&<>$`".contains(c)) || cmd == &joined,
                            "operators: {cmd:?}"
                        );
                    }
                } else if tokens_start_with(cmd, &want)
                    && !cmd.chars().any(|c| ";|&<>$`\n()".contains(c))
                    && cmd.split(' ').all(|t| !t.is_empty())
                {
                    // A restriction must catch everything the request names.
                    assert!(
                        hit || !safe && entries.is_empty(),
                        "{eff:?} {entries:?} missed {cmd:?} for {a:?}"
                    );
                }
            }
        }
    }
    // Truncation, exactly.
    let t = ok(
        Vendor::Claude,
        &pol(
            Posture::Ask,
            &[r(
                Capability::ShellExec,
                Effect::Deny,
                argv(&["echo", "a b"]),
            )],
        ),
    );
    assert_eq!(claude_list(&t, "deny"), ["Bash(echo *)", "Bash(echo)"]);
    assert_eq!(
        line_for(&t, "deny shell_exec").class,
        ReceiptClass::StricterDowngrade
    );
    // Leading dashes beyond the command name are ordinary tokens.
    let d = ok(
        Vendor::Claude,
        &pol(
            Posture::Ask,
            &[r(
                Capability::ShellExec,
                Effect::Allow,
                argv(&["git", "-C", "--no-pager"]),
            )],
        ),
    );
    assert_eq!(
        claude_list(&d, "allow"),
        ["Bash(git -C --no-pager *)", "Bash(git -C --no-pager)"]
    );
}

#[test]
fn claude_path_and_domain_entries_are_exact() {
    let c = ok(
        Vendor::Claude,
        &pol(
            Posture::Ask,
            &[
                r(Capability::FsReadWorkspace, Effect::Allow, pg("src/**")),
                r(Capability::FsWriteWorkspace, Effect::Allow, None),
                r(Capability::NetworkWeb, Effect::Allow, dom("Example.COM")),
                r(
                    Capability::McpToolCall,
                    Effect::Allow,
                    mcp("git_hub", Some("create_issue")),
                ),
                r(Capability::McpToolCall, Effect::Deny, mcp("evil", None)),
            ],
        ),
    );
    assert_eq!(
        claude_list(&c, "allow"),
        [
            "Edit(./**)",
            "Read(src/**)",
            "WebFetch(domain:example.com)",
            "Write(./**)",
            "mcp__git_hub__create_issue"
        ]
    );
    assert_eq!(
        claude_list(&c, "deny")
            .iter()
            .filter(|e| *e == "mcp__evil")
            .count(),
        1
    );
}

fn parse_starlark_tokens(line: &str) -> Vec<String> {
    let body = line.strip_prefix("prefix_rule(pattern=[").unwrap();
    let body = &body[..body.rfind("], decision=").unwrap()];
    let mut out = Vec::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut s = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => s.push(chars.next().unwrap()),
                '"' => break,
                c => s.push(c),
            }
        }
        out.push(s);
    }
    out
}

#[test]
fn codex_prefix_rules_roundtrip_exactly() {
    let cases: [&[&str]; 8] = [
        &["git", "push"],
        &["echo", "a \"b\" ; *"],
        &["echo", "back\\slash"],
        &["echo", "héllo wörld"],
        &["echo", "$(id)"],
        &["echo", "--", "-x"],
        &["echo", "tab\u{7f}"[..4].trim_end_matches('\u{7f}')],
        &["echo", "a,b]\", \"c"],
    ];
    for a in cases {
        let policy = pol(
            Posture::Ask,
            &[r(Capability::ShellExec, Effect::Allow, argv(a))],
        );
        let Ok(plan) = compile(Vendor::Codex, &facts(Vendor::Codex), &policy) else {
            continue;
        };
        let rules = file(&plan, ConfigTarget::CodexRules).unwrap();
        let line = rules
            .lines()
            .find(|l| l.starts_with("prefix_rule"))
            .unwrap();
        let want: Vec<String> = a.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(parse_starlark_tokens(line), want, "{line}");
        assert!(line.ends_with("decision=\"allow\")"));
    }
}

// ---------------------------------------------------------------- change classes + evidence

#[test]
fn change_classes_default_to_the_slow_path() {
    for v in Vendor::ALL {
        let plan = ok(v, &Preset::WorkspaceWeb.policy());
        assert!(plan.requires_respawn, "{v:?}");
        assert_eq!(plan.change_classes.len(), 2 + 8);
        for c in &plan.change_classes {
            assert!(
                matches!(
                    c.class,
                    ChangeClass::PendingNextStart | ChangeClass::Respawn
                ),
                "{v:?} {c:?}"
            );
        }
        let sandbox = plan
            .change_classes
            .iter()
            .find(|c| c.field == ChangeField::Sandbox)
            .unwrap();
        assert_eq!(sandbox.class, ChangeClass::Respawn);
        assert_eq!(plan.key.vendor, v);
    }
    let c = ok(Vendor::Claude, &Preset::Workspace.policy());
    let shell = c
        .change_classes
        .iter()
        .find(|c| c.field == ChangeField::Rules(Capability::ShellExec))
        .unwrap();
    assert_eq!(shell.class, ChangeClass::PendingNextStart);
    // Proven keys upgrade; the sandbox never does.
    let f = facts(Vendor::Claude)
        .with(ProvenFact::HotEdit(ChangeField::Rules(
            Capability::ShellExec,
        )))
        .with(ProvenFact::HotEdit(ChangeField::Posture))
        .with(ProvenFact::HotEdit(ChangeField::Sandbox));
    let c = compile(Vendor::Claude, &f, &Preset::Workspace.policy()).unwrap();
    let get = |field: ChangeField| {
        c.change_classes
            .iter()
            .find(|e| e.field == field)
            .unwrap()
            .class
    };
    assert_eq!(
        get(ChangeField::Rules(Capability::ShellExec)),
        ChangeClass::Hot
    );
    assert_eq!(get(ChangeField::Posture), ChangeClass::Session);
    assert_eq!(get(ChangeField::Sandbox), ChangeClass::Respawn);
    assert_eq!(
        get(ChangeField::Rules(Capability::NetworkWeb)),
        ChangeClass::PendingNextStart
    );
}

#[test]
fn evidence_is_runtime_proven_only_when_the_facts_say_so() {
    for v in Vendor::ALL {
        for p in Preset::ALL {
            if let Ok(plan) = compile(v, &facts(v), &p.policy()) {
                assert!(
                    plan.receipts
                        .iter()
                        .all(|l| l.evidence != EvidenceClass::RuntimeProven)
                );
            }
        }
    }
    let f = facts(Vendor::Codex).with(ProvenFact::RuntimeProven(Surface::Posture));
    let plan = compile(Vendor::Codex, &f, &Preset::Workspace.policy()).unwrap();
    assert_eq!(
        line_for(&plan, "posture").evidence,
        EvidenceClass::RuntimeProven
    );
    assert_ne!(
        line_for(&plan, "allow fs_write_workspace").evidence,
        EvidenceClass::RuntimeProven
    );
    assert_eq!(
        evidence_for(Vendor::Codex, Surface::ProtectedPath, &facts(Vendor::Codex)),
        EvidenceClass::Unsupported
    );
    assert_eq!(
        evidence_for(Vendor::Codex, Surface::FsRule, &facts(Vendor::Codex)),
        EvidenceClass::SchemaDefined
    );
    assert_eq!(
        evidence_for(Vendor::Claude, Surface::ShellRule, &facts(Vendor::Claude)),
        EvidenceClass::Documented
    );
    assert_eq!(
        evidence_for(
            Vendor::Antigravity,
            Surface::NetworkRule,
            &facts(Vendor::Antigravity)
        ),
        EvidenceClass::Unsupported
    );
}

#[test]
fn every_rule_and_every_omitted_capability_has_a_receipt() {
    let mut rng = Rng(99);
    let pool = all_rules();
    for v in Vendor::ALL {
        for _ in 0..80 {
            let n = rng.below(4);
            let rules: Vec<Rule> = (0..n)
                .map(|_| pool[rng.below(pool.len())].clone())
                .collect();
            let policy = pol(Posture::Ask, &rules);
            let Ok(plan) = compile(v, &facts(v), &policy) else {
                continue;
            };
            let norm = policy.normalized().unwrap();
            let covered: BTreeSet<Capability> = plan
                .receipts
                .iter()
                .filter_map(|l| match l.subject {
                    ReceiptSubject::Capability(c) => Some(c),
                    _ => None,
                })
                .collect();
            for cap in Capability::ALL {
                assert!(
                    covered.contains(&cap),
                    "{v:?} {cap:?} has no receipt: {rules:?}"
                );
            }
            for rule in &norm.rules {
                assert!(
                    plan.receipts
                        .iter()
                        .any(|l| l.origin == Origin::Rule
                            && l.requested.starts_with(&rule.describe())),
                    "{v:?}: no receipt for {}",
                    rule.describe()
                );
            }
        }
    }
}
