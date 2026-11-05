//! Codex (spec §1 and §2, "Codex"). Posture splits across two native axes, kept split:
//! `approval_policy` (`-a`) and `sandbox_mode` (`-s`). Filesystem scope uses explicit
//! `writable_roots` only when the request is exactly a directory subtree; a coarse grant
//! never satisfies a narrower policy, and `danger-full-access` is chosen only by
//! `SandboxChoice::Off`. Shell rules are execpolicy `prefix_rule` entries over argv prefixes.

use super::{Cx, exact_root, literal_prefix, prefixes_overlap, safe_token, surface_of};
use crate::permissions::facts::{ProvenFact, Surface};
use crate::permissions::plan::{
    CompileRefusal, ConfigTarget, NativeFile, NativeFormat, Origin, ReceiptClass, ReceiptSubject,
};
use crate::permissions::policy::{Capability, Effect, Pattern, Posture, Rule, SandboxChoice};
use std::collections::BTreeSet;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::ReadOnly => "read-only",
            Mode::WorkspaceWrite => "workspace-write",
            Mode::DangerFullAccess => "danger-full-access",
        }
    }
}

fn q(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn rule_line(tokens: &[String], decision: &str) -> String {
    let t: Vec<String> = tokens.iter().map(|t| q(t)).collect();
    format!(
        "prefix_rule(pattern=[{}], decision={})",
        t.join(", "),
        q(decision)
    )
}

pub(super) fn compile(cx: &mut Cx) -> Result<(), CompileRefusal> {
    let vendor = cx.vendor;
    let posture = cx.policy.posture;
    let off = cx.policy.sandbox == SandboxChoice::Off;

    // 1. Approval axis.
    let (mut approval, approval_class, approval_note) = match posture {
        Posture::Ask => (
            "untrusted",
            ReceiptClass::Exact,
            "ask for everything not known safe",
        ),
        Posture::AcceptEdits => (
            "on-request",
            ReceiptClass::Exact,
            "edits run in the workspace sandbox; the model asks to escalate",
        ),
        Posture::Auto => (
            "on-request",
            ReceiptClass::StricterDowngrade,
            "no reviewer is enabled: auto_review spends tokens and needs admission",
        ),
        Posture::Plan => (
            "on-request",
            ReceiptClass::Exact,
            "read-only sandbox; escalation asks",
        ),
        Posture::Bypass => (
            "never",
            ReceiptClass::Exact,
            "never is no-prompt, not a sandbox bypass",
        ),
    };
    if posture == Posture::Plan && off {
        return Err(CompileRefusal::SandboxConflict {
            vendor,
            reason: "plan is enforced by the read-only sandbox, which sandbox off removes".into(),
        });
    }

    // 2. Sandbox mode: the posture's mode, tightened by rules that need it.
    let all_ws_write = cx
        .rules(Capability::FsWriteWorkspace)
        .iter()
        .all(|r| r.effect == Effect::Allow)
        && cx
            .rules(Capability::FsWriteWorkspace)
            .iter()
            .any(|r| r.pattern.is_none());
    let mut mode = match (off, posture) {
        (true, _) => Mode::DangerFullAccess,
        (false, Posture::Plan) => Mode::ReadOnly,
        (false, Posture::AcceptEdits | Posture::Auto | Posture::Bypass) => Mode::WorkspaceWrite,
        (false, Posture::Ask) => {
            if all_ws_write {
                Mode::WorkspaceWrite
            } else {
                Mode::ReadOnly
            }
        }
    };
    let mut mode_notes: Vec<String> = Vec::new();
    let write_restrictions: Vec<Rule> =
        [Capability::FsWriteWorkspace, Capability::FsWriteProtected]
            .iter()
            .flat_map(|c| cx.rules(*c))
            .filter(|r| r.effect != Effect::Allow)
            .collect();
    if let Some(r) = write_restrictions.first() {
        if mode == Mode::DangerFullAccess {
            return Err(if r.effect == Effect::Deny {
                cx.refuse_deny(
                    r,
                    "sandbox off (danger-full-access) leaves no boundary to enforce it",
                )
            } else {
                CompileRefusal::UnenforceableAsk {
                    vendor,
                    rule: r.describe(),
                    reason: "with the sandbox off no write is ever escalated, so the ask would run"
                        .into(),
                }
            });
        }
        if mode != Mode::ReadOnly {
            mode = Mode::ReadOnly;
            mode_notes.push(
                "a write restriction the workspace-write sandbox cannot scope: the read-only sandbox (stricter) is used".into(),
            );
        }
    }

    // `workspace-write` keeps `/tmp` and `$TMPDIR` writable by default; an external write
    // restriction (deny or ask) would be silently open there unless both are excluded.
    let exclude_tmp = mode == Mode::WorkspaceWrite
        && cx
            .rules(Capability::FsWriteExternal)
            .iter()
            .any(|r| r.effect != Effect::Allow);
    let tmp_native = |on: bool| -> Vec<String> {
        if on {
            vec![
                "sandbox_workspace_write.exclude_slash_tmp=true".to_owned(),
                "sandbox_workspace_write.exclude_tmpdir_env_var=true".to_owned(),
            ]
        } else {
            vec![]
        }
    };
    let mut roots: Vec<String> = Vec::new();
    let mut net_access = false;
    let mut search: Option<&'static str> = None; // Some("live" | "disabled")
    let mut read_all = false;
    let mut prefix_lines: Vec<String> = Vec::new();
    let mut disabled_mcp: BTreeSet<String> = BTreeSet::new();

    // 3. Capabilities.
    for cap in Capability::ALL {
        let rules = cx.rules(cap);
        let surface = surface_of(cap);
        let sub = ReceiptSubject::Capability(cap);
        if rules.is_empty() {
            if cx.policy.posture == Posture::Bypass {
                continue;
            }
            match cap {
                Capability::FsReadWorkspace => cx.default_line(cap, surface, vec![], ReceiptClass::Unsupported,
                    "workspace reads are not gated by the sandbox; the ask default is not enforced"),
                Capability::FsWriteProtected if mode == Mode::ReadOnly => cx.default_line(cap, surface,
                    vec!["sandbox_mode=read-only".to_owned()], ReceiptClass::StricterDowngrade,
                    "no protected-path concept: the read-only sandbox denies every write (stricter)"),
                Capability::FsWriteProtected => cx.default_line(cap, surface, vec![], ReceiptClass::Unsupported,
                    "the vendor has no protected-path concept; the deny default is not enforced"),
                Capability::McpToolCall => cx.default_line(cap, surface, vec![], ReceiptClass::Unsupported,
                    "MCP calls are not gated by compiled rules"),
                Capability::NetworkWeb if mode == Mode::DangerFullAccess => cx.default_line(cap, surface,
                    vec![], ReceiptClass::Unsupported,
                    "sandbox off (danger-full-access) leaves the network open; the default is not enforced"),
                Capability::NetworkWeb => cx.default_line(cap, surface,
                    vec!["network off".to_owned()], ReceiptClass::StricterDowngrade,
                    "the sandbox network is off (and enabled only by an allow): an ask becomes blocked, escalation prompts"),
                Capability::FsReadExternal | Capability::FsWriteExternal | Capability::FsWriteWorkspace => {
                    let c = if mode == Mode::DangerFullAccess { ReceiptClass::Unsupported } else { ReceiptClass::Exact };
                    cx.default_line(cap, surface, vec![], c,
                        "the sandbox scope applies; leaving it needs an escalation prompt");
                }
                Capability::ShellExec => cx.default_line(cap, surface, vec![], ReceiptClass::Exact,
                    "the approval policy decides unmatched commands"),
            }
            continue;
        }
        let restricted = rules.iter().any(|r| r.effect != Effect::Allow);
        match cap {
            Capability::FsReadWorkspace => {
                for r in &rules {
                    match r.effect {
                        Effect::Deny => return Err(cx.refuse_deny(r, "the sandbox cannot hide workspace files from reads")),
                        Effect::Ask => return Err(CompileRefusal::UnenforceableAsk {
                            vendor, rule: r.describe(), reason: "workspace reads are never prompted, so the ask would run".into() }),
                        Effect::Allow if r.pattern.is_none() => cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(),
                            vec![], ReceiptClass::Exact, surface, "the workspace is readable in every sandbox mode"),
                        Effect::Allow => cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(),
                            vec![], ReceiptClass::Unsupported, surface,
                            "no path scope exists for reads; the whole workspace stays readable and nothing was emitted"),
                    }
                }
            }
            Capability::FsReadExternal => {
                for r in &rules {
                    match r.effect {
                        Effect::Deny | Effect::Ask if mode == Mode::DangerFullAccess => {
                            return Err(if r.effect == Effect::Deny {
                                cx.refuse_deny(r, "sandbox off reads the whole disk")
                            } else {
                                CompileRefusal::UnenforceableAsk { vendor, rule: r.describe(), reason: "sandbox off reads the whole disk".into() }
                            });
                        }
                        Effect::Deny => cx.push(sub, Origin::Rule, Some(Effect::Deny), r.describe(),
                            vec!["no disk-full-read-access".to_owned()], ReceiptClass::StricterDowngrade, surface,
                            "the sandbox read scope excludes external paths unless disk-full-read-access is set; all external reads are blocked (stricter for a pattern)"),
                        Effect::Ask => cx.push(sub, Origin::Rule, Some(Effect::Ask), r.describe(),
                            vec!["no disk-full-read-access".to_owned()], ReceiptClass::StricterDowngrade, surface,
                            "external reads are blocked and escalate to a prompt"),
                        Effect::Allow if restricted => cx.withdrawn_allow(r, surface),
                        Effect::Allow if r.pattern.is_none() => {
                            if mode == Mode::DangerFullAccess {
                                cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(), vec![], ReceiptClass::Exact, surface,
                                    "sandbox off already reads the whole disk");
                            } else {
                                read_all = true;
                                cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(),
                                    vec!["sandbox_permissions=[\"disk-full-read-access\"]".to_owned()], ReceiptClass::Exact, surface,
                                    "full-disk read (includes the workspace)");
                            }
                        }
                        Effect::Allow => cx.dropped_allow(r, surface, "no path-scoped read grant exists; the only grant is full-disk"),
                    }
                }
            }
            Capability::FsWriteWorkspace => {
                for r in &rules {
                    match r.effect {
                        Effect::Deny | Effect::Ask => {
                            let exact = r.effect == Effect::Deny && r.pattern.is_none();
                            cx.push(sub, Origin::Rule, Some(r.effect), r.describe(), vec![format!("sandbox_mode={}", mode.as_str())],
                                if exact { ReceiptClass::Exact } else { ReceiptClass::StricterDowngrade }, surface,
                                if exact { "read-only sandbox" } else { "the workspace-write sandbox cannot scope a path or prompt; read-only sandbox used (stricter)" });
                        }
                        Effect::Allow if restricted => cx.withdrawn_allow(r, surface),
                        Effect::Allow if r.pattern.is_none() && mode != Mode::ReadOnly => {
                            cx.push(
                                sub,
                                Origin::Rule,
                                Some(Effect::Allow),
                                r.describe(),
                                vec![format!("sandbox_mode={}", mode.as_str())],
                                ReceiptClass::Exact,
                                surface,
                                "workspace-write sandbox",
                            );
                        }
                        Effect::Allow if r.pattern.is_none() => cx.dropped_allow(
                            r,
                            surface,
                            "the read-only sandbox this policy needs does not write",
                        ),
                        Effect::Allow => cx.dropped_allow(
                            r,
                            surface,
                            "workspace-write grants the whole workspace, wider than the path",
                        ),
                    }
                }
            }
            Capability::FsWriteProtected => {
                for r in &rules {
                    if r.effect == Effect::Allow {
                        cx.dropped_allow(r, surface, "the vendor has no protected-path concept");
                    } else {
                        cx.push(sub, Origin::Rule, Some(r.effect), r.describe(), vec![format!("sandbox_mode={}", mode.as_str())],
                            ReceiptClass::StricterDowngrade, surface,
                            "no protected-path concept: the read-only sandbox denies every write (stricter)");
                    }
                }
                if mode == Mode::ReadOnly {
                    cx.protected_class_defaults(ReceiptClass::StricterDowngrade, vec!["sandbox_mode=read-only".to_owned()],
                        "no protected-path concept: the read-only sandbox denies every write (stricter)");
                } else {
                    cx.protected_class_defaults(ReceiptClass::Unsupported, vec![],
                        "the vendor has no protected-path concept; the deny default is not enforced");
                }
            }
            Capability::FsWriteExternal => {
                let blocks: Vec<Option<Vec<String>>> = rules
                    .iter()
                    .filter(|r| r.effect != Effect::Allow)
                    .map(|r| match &r.pattern {
                        Some(Pattern::AbsPathGlob { glob }) => Some(
                            literal_prefix(glob)
                                .iter()
                                .map(|s| (*s).to_owned())
                                .collect(),
                        ),
                        _ => None,
                    })
                    .collect();
                for r in &rules {
                    match r.effect {
                        Effect::Deny | Effect::Ask if mode == Mode::DangerFullAccess => {
                            return Err(if r.effect == Effect::Deny {
                                cx.refuse_deny(r, "sandbox off writes anywhere")
                            } else {
                                CompileRefusal::UnenforceableAsk { vendor, rule: r.describe(), reason: "sandbox off writes anywhere".into() }
                            });
                        }
                        Effect::Deny => cx.push(sub, Origin::Rule, Some(Effect::Deny), r.describe(), tmp_native(exclude_tmp), ReceiptClass::Exact, surface,
                            "writes outside the workspace and the writable roots are blocked by the sandbox; /tmp and $TMPDIR are excluded from the default writable set"),
                        Effect::Ask => cx.push(sub, Origin::Rule, Some(Effect::Ask), r.describe(), tmp_native(exclude_tmp), ReceiptClass::StricterDowngrade, surface,
                            "writes outside the writable roots are blocked and escalate to a prompt; /tmp and $TMPDIR are excluded from the default writable set"),
                        Effect::Allow => {
                            if mode == Mode::DangerFullAccess {
                                cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(), vec![], ReceiptClass::Exact, surface,
                                    "sandbox off writes anywhere (wider than the request; chosen by the sandbox axis)");
                                continue;
                            }
                            let Some(Pattern::AbsPathGlob { glob }) = &r.pattern else {
                                cx.dropped_allow(r, surface, "every external path would need danger-full-access, which only sandbox off selects");
                                continue;
                            };
                            let Some(root) = exact_root(glob) else {
                                cx.dropped_allow(r, surface, "writable roots are directory subtrees; this glob is not one");
                                continue;
                            };
                            if mode != Mode::WorkspaceWrite {
                                cx.dropped_allow(r, surface, "writable roots need the workspace-write sandbox, which this policy does not select");
                                continue;
                            }
                            let lit = literal_prefix(&root);
                            let clash = blocks.iter().any(|b| match b {
                                None => true,
                                Some(p) => prefixes_overlap(&lit, &p.iter().map(String::as_str).collect::<Vec<_>>()),
                            });
                            if clash {
                                cx.withdrawn_allow(r, surface);
                                continue;
                            }
                            roots.push(root.clone());
                            cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(),
                                vec![format!("sandbox_workspace_write.writable_roots += {root}")], ReceiptClass::Exact, surface,
                                "explicit writable root (absolute)");
                        }
                    }
                }
            }
            Capability::ShellExec => {
                let mut ask_unsupported = false;
                for r in &rules {
                    let dec = match r.effect {
                        Effect::Allow => "allow",
                        Effect::Ask => "prompt",
                        Effect::Deny => "forbidden",
                    };
                    // Only plain tokens become pattern elements; a glob is cut at the first
                    // token that is not plain or holds a wildcard (the cut matches more: stricter).
                    let tokens: Option<(Vec<String>, bool)> = match &r.pattern {
                        Some(Pattern::ArgvPrefix { argv }) => Some((argv.clone(), false)),
                        Some(Pattern::ShellGlob { glob }) if r.effect != Effect::Allow => {
                            let t: Vec<String> = glob
                                .split(' ')
                                .take_while(|t| safe_token(t))
                                .map(str::to_owned)
                                .collect();
                            if t.is_empty() { None } else { Some((t, true)) }
                        }
                        _ => None,
                    };
                    let keeps_sandbox = cx.proves(&ProvenFact::ExecpolicyAllowKeepsSandbox);
                    match (r.effect, tokens) {
                        (Effect::Allow, _) if ask_unsupported => cx.withdrawn_allow(r, surface),
                        (Effect::Allow, Some(_)) if mode == Mode::ReadOnly => cx.dropped_allow(
                            r,
                            surface,
                            "under the read-only sandbox a shell allow is never emitted",
                        ),
                        (Effect::Allow, Some((t, _))) if keeps_sandbox => {
                            let l = rule_line(&t, "allow");
                            prefix_lines.push(l.clone());
                            cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(), vec![l], ReceiptClass::Exact, surface,
                                "execpolicy prefix_rule over argv, compared token by token; the facts prove an allow keeps the sandbox");
                        }
                        (Effect::Allow, Some((t, _))) => {
                            let l = rule_line(&t, "prompt");
                            prefix_lines.push(l.clone());
                            cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(), vec![l], ReceiptClass::StricterDowngrade, surface,
                                "execpolicy allow may bypass the sandbox; unverified, so the command prompts instead");
                        }
                        (Effect::Allow, None) => cx.dropped_allow(r, surface, "a string glob or an all-commands allow has no execpolicy form"),
                        (e, Some((t, truncated))) => {
                            let l = rule_line(&t, dec);
                            prefix_lines.push(l.clone());
                            cx.push(sub, Origin::Rule, Some(e), r.describe(), vec![l],
                                if truncated { ReceiptClass::StricterDowngrade } else { ReceiptClass::Exact }, surface,
                                if truncated { "string glob reduced to its leading plain tokens: matches more (stricter)" } else { "execpolicy prefix_rule over argv, compared token by token" });
                        }
                        (Effect::Deny, None) => return Err(cx.refuse_deny(r, "execpolicy has no form for a command that is a pure glob or for all commands")),
                        (Effect::Ask, None) => {
                            ask_unsupported = true;
                            if approval != "never" { approval = "untrusted"; }
                            cx.push(sub, Origin::Rule, Some(Effect::Ask), r.describe(), vec!["-a untrusted".to_owned()], ReceiptClass::StricterDowngrade, surface,
                                "no prompt rule form: the approval policy is tightened to untrusted and allows of shell are withdrawn");
                        }
                    }
                }
            }
            Capability::NetworkWeb => {
                if restricted && mode == Mode::DangerFullAccess {
                    let r = rules
                        .iter()
                        .find(|r| r.effect != Effect::Allow)
                        .expect("restricted");
                    return Err(if r.effect == Effect::Deny {
                        cx.refuse_deny(r, "sandbox off leaves the network open")
                    } else {
                        CompileRefusal::UnenforceableAsk {
                            vendor,
                            rule: r.describe(),
                            reason: "sandbox off leaves the network open".into(),
                        }
                    });
                }
                let mut open_scoped_net = false;
                for r in &rules {
                    let d = r.describe();
                    let lines =
                        |cx: &mut Cx,
                         e: Effect,
                         t: (Vec<String>, ReceiptClass, &str),
                         h: (Vec<String>, ReceiptClass, &str),
                         w: (Vec<String>, ReceiptClass, &str)| {
                            for (leg, (n, c, note)) in [
                                ("sandbox network transport", t),
                                ("host approvals", h),
                                ("WebSearch tool", w),
                            ] {
                                cx.push(
                                    sub,
                                    Origin::Rule,
                                    Some(e),
                                    format!("{d} [{leg}]"),
                                    n,
                                    c,
                                    surface,
                                    note,
                                );
                            }
                        };
                    match r.effect {
                        Effect::Deny | Effect::Ask => {
                            search = Some("disabled");
                            let exact = r.effect == Effect::Deny && r.pattern.is_none();
                            let c = if exact { ReceiptClass::Exact } else { ReceiptClass::StricterDowngrade };
                            lines(cx, r.effect,
                                (vec!["network_access=false".to_owned()], c, "sandbox network off (all hosts: stricter for a domain or an ask)"),
                                (vec![], c, "no host approvals are pre-seeded; a host-scoped deny is not expressible"),
                                (vec!["web_search=\"disabled\"".to_owned()], c, "search tool disabled (key unverified until runtime-proven)"));
                        }
                        Effect::Allow if restricted => cx.withdrawn_allow(r, surface),
                        Effect::Allow if r.pattern.is_none() && mode == Mode::WorkspaceWrite => {
                            net_access = true;
                            search = Some("live");
                            lines(cx, Effect::Allow,
                                (vec!["network_access=true".to_owned()], ReceiptClass::Exact, "sandbox network on"),
                                (vec![], ReceiptClass::Exact, "all hosts allowed; no host rules"),
                                (vec!["--search".to_owned()], ReceiptClass::Exact, "search tool enabled"));
                        }
                        Effect::Allow if r.pattern.is_none() && mode == Mode::DangerFullAccess => {
                            lines(cx, Effect::Allow,
                                (vec![], ReceiptClass::Exact, "sandbox off: network already open"),
                                (vec![], ReceiptClass::Exact, "no host rules"),
                                (vec![], ReceiptClass::Unsupported, "search tool not managed under sandbox off"));
                        }
                        Effect::Allow if r.pattern.is_none() => cx.dropped_allow(r, surface, "network needs the workspace-write sandbox, which this policy does not select"),
                        Effect::Allow if mode == Mode::DangerFullAccess => {
                            // Sandbox off leaves every host open: the domain scope is not enforced.
                            lines(cx, Effect::Allow,
                                (vec![], ReceiptClass::Unsupported, "sandbox off: the network is open to every host, so the domain scope is not enforced"),
                                (vec![], ReceiptClass::Unsupported, "host approvals are decisions answered at runtime, not pre-seedable config; complete domain enforcement is unproven"),
                                (vec![], ReceiptClass::Unsupported, "search tool not managed under sandbox off"));
                            open_scoped_net = true;
                        }
                        Effect::Allow => {
                            lines(cx, Effect::Allow,
                                (vec![], ReceiptClass::StricterDowngrade, "transport stays off: a domain allow is not granted"),
                                (vec![], ReceiptClass::Unsupported, "host approvals are decisions answered at runtime, not pre-seedable config; complete domain enforcement is unproven"),
                                (vec![], ReceiptClass::StricterDowngrade, "search tool not enabled"));
                        }
                    }
                }
                if open_scoped_net && posture != Posture::Bypass {
                    cx.default_line(cap, surface, vec![], ReceiptClass::Unsupported,
                        "sandbox off leaves the network open; the ask default for every other host is not enforced");
                }
            }
            Capability::McpToolCall => {
                for r in &rules {
                    match r.effect {
                        Effect::Deny => {
                            let proven = cx.proves(&ProvenFact::HardDeny(Capability::McpToolCall));
                            match (&r.pattern, proven) {
                                (Some(Pattern::Mcp { server, tool }), true) => {
                                    disabled_mcp.insert(server.clone());
                                    cx.push(sub, Origin::Rule, Some(Effect::Deny), r.describe(),
                                        vec![format!("mcp_servers.{server}.enabled=false")],
                                        if tool.is_none() { ReceiptClass::Exact } else { ReceiptClass::StricterDowngrade }, surface,
                                        if tool.is_none() { "server disabled (disablement proven by the facts)" } else { "the whole server is disabled to deny one tool (stricter)" });
                                }
                                _ => return Err(cx.refuse_deny(r, "per-tool controls exist for app/connector tools only (AppToolConfig); arbitrary MCP servers have no proven native deny")),
                            }
                        }
                        Effect::Ask => return Err(CompileRefusal::UnenforceableAsk {
                            vendor, rule: r.describe(),
                            reason: "no proven MCP prompt control: the approval policy does not gate arbitrary MCP servers, so the ask would run".into() }),
                        Effect::Allow => cx.dropped_allow(r, surface, "per-tool approval config covers app/connector tools, not arbitrary MCP servers"),
                    }
                }
            }
        }
    }

    // 4. Emit.
    let approval_args = vec!["-a".to_owned(), approval.to_owned()];
    cx.args.extend(approval_args.iter().cloned());
    let mut note = approval_note.to_owned();
    let mut aclass = approval_class;
    if approval
        != match posture {
            Posture::Ask => "untrusted",
            Posture::Bypass => "never",
            _ => "on-request",
        }
    {
        aclass = ReceiptClass::StricterDowngrade;
        note = format!("{note}; tightened by a shell ask");
    }
    cx.push(
        ReceiptSubject::Posture,
        Origin::Axis,
        None,
        format!("posture {posture:?}"),
        approval_args,
        aclass,
        Surface::Posture,
        note,
    );

    let sandbox_args = vec!["-s".to_owned(), mode.as_str().to_owned()];
    cx.args.extend(sandbox_args.iter().cloned());
    cx.push(
        ReceiptSubject::Sandbox,
        Origin::Axis,
        None,
        format!("sandbox {}", if off { "off" } else { "vendor-default" }),
        sandbox_args,
        ReceiptClass::Exact,
        Surface::Sandbox,
        if mode_notes.is_empty() {
            "explicit sandbox_mode; no vendor default is relied on".to_owned()
        } else {
            mode_notes.join("; ")
        },
    );
    if read_all {
        cx.args.extend([
            "-c".to_owned(),
            "sandbox_permissions=[\"disk-full-read-access\"]".to_owned(),
        ]);
    }
    if search == Some("live") {
        cx.args.push("--search".to_owned());
    }

    prefix_lines.sort();
    prefix_lines.dedup();
    if !prefix_lines.is_empty() {
        let mut c = String::from(
            "# Generated by FerroLoop (permission compiler, schema_version 1). Do not edit.\n",
        );
        for l in &prefix_lines {
            c.push_str(l);
            c.push('\n');
        }
        cx.files.push(NativeFile {
            target: ConfigTarget::CodexRules,
            format: NativeFormat::Starlark,
            content: c,
        });
    }
    let mut toml = String::from(
        "# Generated by FerroLoop (permission compiler, schema_version 1). Do not edit.\n",
    );
    let mut any = false;
    if let Some(s) = search.filter(|s| *s == "disabled") {
        toml.push_str(&format!("web_search = {}\n", q(s)));
        any = true;
    }
    if mode == Mode::WorkspaceWrite {
        roots.sort();
        roots.dedup();
        toml.push_str("\n[sandbox_workspace_write]\n");
        toml.push_str(&format!("network_access = {net_access}\n"));
        let r: Vec<String> = roots.iter().map(|r| q(r)).collect();
        toml.push_str(&format!("writable_roots = [{}]\n", r.join(", ")));
        if exclude_tmp {
            toml.push_str("exclude_slash_tmp = true\nexclude_tmpdir_env_var = true\n");
        }
        any = true;
    }
    for s in &disabled_mcp {
        toml.push_str(&format!("\n[mcp_servers.{s}]\nenabled = false\n"));
        any = true;
    }
    if any {
        cx.files.push(NativeFile {
            target: ConfigTarget::CodexConfig,
            format: NativeFormat::Toml,
            content: toml,
        });
    }
    Ok(())
}
