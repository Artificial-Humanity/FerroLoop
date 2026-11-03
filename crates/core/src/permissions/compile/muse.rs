//! Muse (spec §1 and §2, "Muse"): `--approval-mode`, `--permission-profile`, and the sandbox
//! pair `--disable-sandbox` / `--sandbox-network`. `--yolo` and `--disable-approval` exist
//! only for [`Posture::Bypass`] and are never derived from rules.
//!
//! A profile carries no glob grammar. The compiler picks a caller-verified profile whose
//! ceiling does not exceed what the policy allows; a rule a profile cannot express is
//! reported as the closest stricter profile, or refused when it is a deny.

use super::{Cx, surface_of};
use crate::permissions::facts::{ProfileCeiling, ProvenFact, Surface};
use crate::permissions::plan::{CompileRefusal, Origin, ReceiptClass, ReceiptSubject};
use crate::permissions::policy::{Capability, Effect, Posture, Rule, SandboxChoice};

pub(super) fn compile(cx: &mut Cx) -> Result<(), CompileRefusal> {
    let vendor = cx.vendor;
    let posture = cx.policy.posture;
    let off = cx.policy.sandbox == SandboxChoice::Off;
    let bypass = posture == Posture::Bypass;

    // Profile selection: the strongest ceiling the policy allows.
    let write_restricted = [Capability::FsWriteWorkspace, Capability::FsWriteProtected]
        .iter()
        .any(|c| cx.rules(*c).iter().any(|r| r.effect != Effect::Allow));
    let wants_workspace = !write_restricted
        && posture != Posture::Plan
        && cx
            .rules(Capability::FsWriteWorkspace)
            .iter()
            .any(|r| r.effect == Effect::Allow && r.pattern.is_none());
    let pick = |c: ProfileCeiling| {
        let mut ids: Vec<&str> = cx
            .facts
            .muse_profiles
            .iter()
            .filter(|p| p.ceiling == c)
            .map(|p| p.id.as_str())
            .collect();
        ids.sort_unstable();
        ids.first().map(|s| (*s).to_owned())
    };
    let ws_profile = if wants_workspace {
        pick(ProfileCeiling::Workspace)
    } else {
        None
    };
    let ro_profile = pick(ProfileCeiling::ReadOnly);
    let profile: Option<(String, ProfileCeiling)> = ws_profile
        .map(|p| (p, ProfileCeiling::Workspace))
        .or_else(|| ro_profile.clone().map(|p| (p, ProfileCeiling::ReadOnly)));
    let read_only_selected = matches!(profile, Some((_, ProfileCeiling::ReadOnly)));
    // The read-only profile is a boundary only while the sandbox is on: `--disable-sandbox`
    // removes it (see the plan and external-path branches).
    let boundary = read_only_selected && !off;
    if posture == Posture::Plan && !read_only_selected {
        return Err(CompileRefusal::PostureNotEnforceable {
            vendor,
            posture,
            reason: "plan needs a caller-verified read-only permission profile".into(),
        });
    }

    if posture == Posture::Plan && off {
        return Err(CompileRefusal::SandboxConflict {
            vendor,
            reason: "plan is carried by the read-only profile and the sandbox, which sandbox off removes".into(),
        });
    }

    // A shell ask has no pattern-scoped form: the strictest prompting mode is the only way it
    // prompts. `on-request` runs sandboxed commands without a prompt, so it is tightened.
    let shell_ask = cx
        .rules(Capability::ShellExec)
        .iter()
        .any(|r| r.effect == Effect::Ask);

    // Posture.
    let mut args: Vec<String> = Vec::new();
    let (note, class) = match posture {
        Posture::Bypass => {
            args.push(if off { "--yolo" } else { "--disable-approval" }.to_owned());
            (
                if off {
                    "bypass with the sandbox off: --yolo covers both axes"
                } else {
                    "bypass of approvals only: --disable-approval leaves the sandbox axis alone"
                },
                ReceiptClass::Exact,
            )
        }
        Posture::Ask => {
            args.extend(["--approval-mode".to_owned(), "untrusted".to_owned()]);
            ("ask for everything not trusted", ReceiptClass::Exact)
        }
        Posture::AcceptEdits if shell_ask => {
            args.extend(["--approval-mode".to_owned(), "untrusted".to_owned()]);
            (
                "tightened by a shell ask: on-request would run sandboxed commands without a prompt",
                ReceiptClass::StricterDowngrade,
            )
        }
        Posture::AcceptEdits => {
            args.extend(["--approval-mode".to_owned(), "on-request".to_owned()]);
            (
                "edits proceed under the profile; the model asks to escalate",
                ReceiptClass::Exact,
            )
        }
        Posture::Auto if shell_ask => {
            args.extend(["--approval-mode".to_owned(), "untrusted".to_owned()]);
            (
                "tightened by a shell ask; the approval judge stays off: a reviewer spends tokens and needs admission",
                ReceiptClass::StricterDowngrade,
            )
        }
        Posture::Auto => {
            args.extend(["--approval-mode".to_owned(), "on-request".to_owned()]);
            (
                "the approval judge stays off: a reviewer spends tokens and needs admission",
                ReceiptClass::StricterDowngrade,
            )
        }
        Posture::Plan => {
            args.extend(["--approval-mode".to_owned(), "untrusted".to_owned()]);
            (
                "plan is carried by the read-only profile",
                ReceiptClass::Exact,
            )
        }
    };
    if !bypass {
        args.extend(["--approval-judge".to_owned(), "off".to_owned()]);
    }
    cx.args.extend(args.iter().cloned());
    cx.push(
        ReceiptSubject::Posture,
        Origin::Axis,
        None,
        format!("posture {posture:?}"),
        args,
        class,
        Surface::Posture,
        note,
    );

    match &profile {
        Some((id, ceiling)) => {
            let a = vec!["--permission-profile".to_owned(), id.clone()];
            cx.args.extend(a.iter().cloned());
            cx.push(ReceiptSubject::Posture, Origin::Axis, None, "permission profile".to_owned(), a,
                ReceiptClass::Exact, Surface::FsRule,
                format!("the strongest caller-verified profile within the policy (ceiling {ceiling:?}); its content is asserted by the facts"));
        }
        None => cx.push(ReceiptSubject::Posture, Origin::Axis, None, "permission profile".to_owned(), vec![],
            ReceiptClass::Unsupported, Surface::FsRule,
            "no verified profile in the facts: the vendor default profile governs (unknown); only posture and sandbox flags are compiled"),
    }

    // Sandbox axis.
    if off && !bypass {
        cx.args.push("--disable-sandbox".to_owned());
    }
    cx.push(
        ReceiptSubject::Sandbox,
        Origin::Axis,
        None,
        format!("sandbox {}", if off { "off" } else { "vendor-default" }),
        if off {
            vec![
                if bypass {
                    "--yolo"
                } else {
                    "--disable-sandbox"
                }
                .to_owned(),
            ]
        } else {
            vec![]
        },
        ReceiptClass::Exact,
        Surface::Sandbox,
        "separate axis from approvals; launch-fixed",
    );

    // Rules.
    let mut net: Option<(&'static str, ReceiptClass)> = None;
    for cap in Capability::ALL {
        let rules = cx.rules(cap);
        let surface = surface_of(cap);
        let sub = ReceiptSubject::Capability(cap);
        if rules.is_empty() {
            if bypass {
                continue;
            }
            match cap {
                Capability::NetworkWeb if !off => {
                    net = Some(("restricted", ReceiptClass::StricterDowngrade));
                    cx.default_line(
                        cap,
                        surface,
                        vec!["--sandbox-network restricted".to_owned()],
                        ReceiptClass::StricterDowngrade,
                        "the canonical ask default is carried by the strictest network mode",
                    );
                }
                Capability::NetworkWeb => cx.default_line(
                    cap,
                    surface,
                    vec![],
                    ReceiptClass::Unsupported,
                    "sandbox off leaves the network open; the ask default is not enforced",
                ),
                Capability::FsWriteProtected if boundary => cx.default_line(
                    cap,
                    surface,
                    vec![],
                    ReceiptClass::StricterDowngrade,
                    "no protected-path concept: the read-only profile blocks every workspace write (stricter)",
                ),
                Capability::FsWriteProtected => cx.default_line(
                    cap,
                    surface,
                    vec![],
                    ReceiptClass::Unsupported,
                    if read_only_selected {
                        "sandbox off removes the read-only profile's boundary; the deny default is not enforced"
                    } else {
                        "the vendor has no protected-path concept; the deny default is not enforced"
                    },
                ),
                _ => cx.default_line(
                    cap,
                    surface,
                    vec![],
                    ReceiptClass::Exact,
                    "the approval mode and profile decide unmatched requests",
                ),
            }
            continue;
        }
        let restricted = rules.iter().any(|r| r.effect != Effect::Allow);
        let hard = cx.proves(&ProvenFact::HardDeny(cap));
        let mut ask_seen = false;
        for r in &rules {
            let refuse_unless_hard =
                |cx: &mut Cx, r: &Rule, why: &str| -> Result<(), CompileRefusal> {
                    if hard {
                        cx.push(
                            sub,
                            Origin::Rule,
                            Some(Effect::Deny),
                            r.describe(),
                            vec![],
                            ReceiptClass::Exact,
                            surface,
                            "enforcement asserted by the facts (HardDeny)",
                        );
                        Ok(())
                    } else {
                        Err(cx.refuse_deny(r, why.to_owned()))
                    }
                };
            match (cap, r.effect) {
                (Capability::FsReadWorkspace, Effect::Allow) => cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(), vec![],
                    if r.pattern.is_none() { ReceiptClass::Exact } else { ReceiptClass::Unsupported }, surface,
                    if r.pattern.is_none() { "profiles read the workspace" } else { "profiles read the workspace as a whole; a narrower allow is not narrowed and nothing was emitted" }),
                (Capability::FsReadWorkspace, Effect::Deny) => refuse_unless_hard(cx, r, "no profile can hide workspace files")?,
                (Capability::FsReadWorkspace, Effect::Ask) => return Err(CompileRefusal::UnenforceableAsk {
                    vendor, rule: r.describe(), reason: "workspace reads are not prompted".into() }),
                (Capability::FsWriteWorkspace | Capability::FsWriteProtected, Effect::Deny | Effect::Ask) => {
                    if boundary {
                        let exact = cap == Capability::FsWriteWorkspace && r.effect == Effect::Deny && r.pattern.is_none();
                        cx.push(sub, Origin::Rule, Some(r.effect), r.describe(), vec![],
                            if exact { ReceiptClass::Exact } else { ReceiptClass::StricterDowngrade }, surface,
                            "the read-only profile blocks every workspace write (stricter for a pattern, a class or an ask)");
                    } else if r.effect == Effect::Deny {
                        refuse_unless_hard(cx, r, if off {
                            "sandbox off removes the boundary the read-only profile relies on"
                        } else {
                            "no verified read-only profile can stand in for a scoped write deny"
                        })?;
                    } else {
                        return Err(CompileRefusal::UnenforceableAsk { vendor, rule: r.describe(),
                            reason: if off {
                                "sandbox off removes the boundary the read-only profile relies on, so the write would run"
                            } else {
                                "no verified read-only profile to make writes prompt"
                            }.into() });
                    }
                }
                (Capability::FsWriteWorkspace, Effect::Allow) => {
                    if restricted {
                        cx.withdrawn_allow(r, surface);
                    } else if r.pattern.is_none() && matches!(profile, Some((_, ProfileCeiling::Workspace))) {
                        cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(), vec![], ReceiptClass::Exact, surface,
                            "the workspace profile grants workspace writes");
                    } else if r.pattern.is_none() {
                        cx.dropped_allow(r, surface, "no verified workspace profile within the policy");
                    } else {
                        cx.dropped_allow(r, surface, "profiles carry no path grammar");
                    }
                }
                (Capability::FsWriteProtected, Effect::Allow) => cx.dropped_allow(r, surface, "the vendor has no protected-path concept"),
                (Capability::FsReadExternal | Capability::FsWriteExternal, Effect::Deny | Effect::Ask) => {
                    // Enforcement needs a selected verified profile that confines access to the
                    // workspace (a fact), or a proven hard deny. Never an empty-handed Exact.
                    let confined = profile.is_some() && cx.proves(&ProvenFact::ProfileConfinesExternal);
                    if hard && r.effect == Effect::Deny {
                        cx.push(sub, Origin::Rule, Some(Effect::Deny), r.describe(), vec![], ReceiptClass::Exact, surface,
                            "enforcement asserted by the facts (HardDeny)");
                    } else if off || !confined {
                        let why = if off {
                            "sandbox off leaves no boundary outside the workspace"
                        } else {
                            "no selected verified profile is proven to confine access to the workspace"
                        };
                        return Err(if r.effect == Effect::Deny {
                            cx.refuse_deny(r, why)
                        } else {
                            CompileRefusal::UnenforceableAsk { vendor, rule: r.describe(), reason: why.into() }
                        });
                    } else {
                        cx.push(sub, Origin::Rule, Some(r.effect), r.describe(), profile.iter().map(|(id, _)| format!("--permission-profile {id}")).collect(),
                            if r.effect == Effect::Deny && r.pattern.is_none() { ReceiptClass::Exact } else { ReceiptClass::StricterDowngrade }, surface,
                            "the selected profile confines access to the workspace (asserted by the facts); every external path is blocked or prompts (stricter for a pattern)");
                    }
                }
                (Capability::FsReadExternal | Capability::FsWriteExternal, Effect::Allow) => {
                    if restricted { cx.withdrawn_allow(r, surface) } else { cx.dropped_allow(r, surface, "profiles have no external-path grant") }
                }
                (Capability::ShellExec, Effect::Deny) => refuse_unless_hard(cx, r, "profiles carry no command grammar and no proven shell deny")?,
                (Capability::ShellExec, Effect::Ask) => {
                    ask_seen = true;
                    cx.push(sub, Origin::Rule, Some(Effect::Ask), r.describe(), vec!["--approval-mode untrusted".to_owned()], ReceiptClass::StricterDowngrade, surface,
                        "no pattern-scoped ask: the approval mode is tightened to untrusted (every command prompts) and allows of shell are withdrawn");
                }
                (Capability::McpToolCall, Effect::Ask) => return Err(CompileRefusal::UnenforceableAsk {
                    vendor, rule: r.describe(), reason: "no proven MCP prompt control: Muse's approval mode does not gate MCP calls, so the ask would run".into() }),
                (Capability::ShellExec | Capability::McpToolCall, Effect::Allow) => {
                    if ask_seen { cx.withdrawn_allow(r, surface) } else { cx.dropped_allow(r, surface, "profiles carry no grammar for this capability") }
                }
                (Capability::McpToolCall, Effect::Deny) => refuse_unless_hard(cx, r, "no proven native MCP deny")?,
                (Capability::NetworkWeb, _) => {}
            }
        }
        if cap == Capability::FsWriteProtected {
            if boundary {
                cx.protected_class_defaults(ReceiptClass::StricterDowngrade, vec![],
                    "no protected-path concept: the read-only profile blocks every workspace write (stricter)");
            } else {
                cx.protected_class_defaults(
                    ReceiptClass::Unsupported,
                    vec![],
                    if read_only_selected {
                        "sandbox off removes the read-only profile's boundary; the deny default is not enforced"
                    } else {
                        "the vendor has no protected-path concept; the deny default is not enforced"
                    },
                );
            }
        }
        if cap == Capability::NetworkWeb {
            if restricted {
                if off {
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
                            reason: "sandbox off".into(),
                        }
                    });
                }
                net = Some(("restricted", ReceiptClass::StricterDowngrade));
                for r in &rules {
                    if r.effect == Effect::Allow {
                        cx.withdrawn_allow(r, surface);
                    } else {
                        let exact = r.effect == Effect::Deny && r.pattern.is_none();
                        cx.push(sub, Origin::Rule, Some(r.effect), r.describe(), vec!["--sandbox-network restricted".to_owned()],
                            if exact { ReceiptClass::Exact } else { ReceiptClass::StricterDowngrade }, surface,
                            "network mode restricted (a domain deny or an ask has no native form: every host is blocked)");
                    }
                }
            } else if rules.iter().any(|r| r.pattern.is_none()) {
                net = Some(("enabled", ReceiptClass::Exact));
                for r in &rules {
                    cx.push(
                        sub,
                        Origin::Rule,
                        Some(Effect::Allow),
                        r.describe(),
                        if off {
                            vec![]
                        } else {
                            vec!["--sandbox-network enabled".to_owned()]
                        },
                        ReceiptClass::Exact,
                        surface,
                        if off {
                            "sandbox off: the network is already open; no flag is emitted"
                        } else {
                            "network mode enabled"
                        },
                    );
                }
            } else if off {
                // `--sandbox-network` is not emitted under `--disable-sandbox`: the network is open
                // to every host, so the domain scope is not enforced.
                for r in &rules {
                    cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(), vec![], ReceiptClass::Unsupported, surface,
                        "sandbox off leaves the network open to every host; the domain scope is not enforced and no flag is emitted");
                }
                if !bypass {
                    cx.default_line(cap, surface, vec![], ReceiptClass::Unsupported,
                        "sandbox off leaves the network open; the ask default for every other host is not enforced");
                }
            } else {
                net = Some(("proxy-only", ReceiptClass::StricterDowngrade));
                for r in &rules {
                    cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(), vec!["--sandbox-network proxy-only".to_owned()],
                        ReceiptClass::StricterDowngrade, surface,
                        "no domain grammar: the mode degrades to proxy-only, the honest middle. The proxy's own allowlist governs, and it may be broader than the requested domains; it is outside this plan");
                }
            }
        }
    }
    if let (Some((mode, _)), false) = (net, off) {
        cx.args
            .extend(["--sandbox-network".to_owned(), mode.to_owned()]);
    }
    Ok(())
}
