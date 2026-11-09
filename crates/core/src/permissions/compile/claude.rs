//! Claude Code (spec §1 and §2, "Claude Code"): `--permission-mode`, and
//! `permissions.{allow,deny,ask}` in project-local settings. Grok reuses the rule syntax.

use super::{Cx, Entries, Translation, safe_token, surface_of};
use crate::permissions::facts::{Surface, Vendor};
use crate::permissions::plan::{
    CompileRefusal, ConfigTarget, NativeFile, NativeFormat, Origin, ReceiptClass, ReceiptSubject,
};
use crate::permissions::policy::{
    Capability, Effect, PathClass, Pattern, Posture, Rule, SandboxChoice,
};
use serde_json::{Map, Value, json};

/// The protected globs of a class, by vendor.
fn protected_globs(vendor: Vendor, class: PathClass) -> &'static [&'static str] {
    match (vendor, class) {
        (Vendor::Grok, PathClass::Settings) => &[".grok/config.toml"],
        (Vendor::Grok, PathClass::ToolConfig) => &[".grok/**", ".mcp.json"],
        (_, PathClass::Settings) => &[".claude/settings.json", ".claude/settings.local.json"],
        (_, PathClass::ToolConfig) => &[".claude/**", ".mcp.json"],
        (_, PathClass::Git) => &[".git/**"],
    }
}

/// A workspace glob anchored with `./`, so a bare name (`Makefile`, `*.rs`) is not read with
/// gitignore any-depth semantics, which would widen an allow.
fn anchored(p: &Option<Pattern>) -> String {
    match p {
        Some(Pattern::PathGlob { glob }) => format!("./{glob}"),
        _ => "./**".to_owned(),
    }
}

fn native(entries: Vec<String>, class: ReceiptClass, note: &str) -> Translation {
    Translation::Native {
        entries,
        class,
        note: note.to_owned(),
    }
}

/// A rule in Claude-compatible syntax.
pub(super) fn translate(vendor: Vendor, rule: &Rule) -> Translation {
    // A deny or ask through a tool rule does not bind other access paths.
    let scoped = if rule.effect == Effect::Allow {
        ReceiptClass::Exact
    } else {
        ReceiptClass::ToolScopedEmulation
    };
    let tool_note = if rule.effect == Effect::Allow {
        "tool rule"
    } else {
        "tool-scoped: binds this tool only, not shell, MCP or plugins"
    };
    match (rule.capability, &rule.pattern) {
        (Capability::FsReadWorkspace, p) => {
            let g = anchored(p);
            native(vec![format!("Read({g})")], scoped, tool_note)
        }
        (Capability::FsWriteWorkspace, p) => {
            let g = anchored(p);
            native(vec![format!("Edit({g})"), format!("Write({g})")], scoped, tool_note)
        }
        (Capability::FsReadExternal, Some(Pattern::AbsPathGlob { glob })) => {
            native(vec![format!("Read(/{glob})")], scoped, tool_note)
        }
        (Capability::FsWriteExternal, Some(Pattern::AbsPathGlob { glob })) => native(
            vec![format!("Edit(/{glob})"), format!("Write(/{glob})")],
            scoped,
            tool_note,
        ),
        (Capability::FsReadExternal | Capability::FsWriteExternal, _) => Translation::Unsupported(
            "every path outside the workspace cannot be separated from workspace paths in a tool rule"
                .to_owned(),
        ),
        (Capability::FsWriteProtected, p) => {
            let classes: Vec<PathClass> = match p {
                Some(Pattern::PathClass { class }) => vec![*class],
                _ => PathClass::ALL.to_vec(),
            };
            let mut v = Vec::new();
            for c in classes {
                for g in protected_globs(vendor, c) {
                    v.push(format!("Edit({g})"));
                    v.push(format!("Write({g})"));
                }
            }
            native(
                v,
                ReceiptClass::ToolScopedEmulation,
                "emulation: the vendor has no protected-path rule; Edit/Write rules bind those tools only",
            )
        }
        (Capability::ShellExec, None) => native(vec!["Bash".to_owned()], ReceiptClass::Exact, "whole Bash tool"),
        (Capability::ShellExec, Some(Pattern::ArgvPrefix { argv })) => {
            let safe = argv.iter().take_while(|t| safe_token(t)).count();
            if safe == argv.len() {
                let p = argv.join(" ");
                native(
                    vec![format!("Bash({p})"), format!("Bash({p} *)")],
                    ReceiptClass::Exact,
                    "argv prefix as `prefix` and `prefix *`",
                )
            } else if rule.effect == Effect::Allow || safe == 0 {
                Translation::Unsupported(
                    "an argv token needs shell quoting or holds a wildcard, which a Bash rule cannot state exactly"
                        .to_owned(),
                )
            } else {
                let p = argv[..safe].join(" ");
                native(
                    vec![format!("Bash({p})"), format!("Bash({p} *)")],
                    ReceiptClass::StricterDowngrade,
                    "truncated at the first token that needs quoting: matches more than requested (stricter)",
                )
            }
        }
        (Capability::ShellExec, Some(Pattern::ShellGlob { glob })) if glob.ends_with(":*") => {
            // The legacy prefix form: the vendor would read it as every command starting there.
            Translation::Unsupported("a trailing `:*` reads as a legacy prefix and matches more".to_owned())
        }
        (Capability::ShellExec, Some(Pattern::ShellGlob { glob })) => {
            native(vec![format!("Bash({glob})")], ReceiptClass::Exact, "string glob")
        }
        (Capability::ShellExec, _) => Translation::Unsupported("pattern kind".to_owned()),
        (Capability::NetworkWeb, None) => native(
            vec!["WebFetch".to_owned(), "WebSearch".to_owned()],
            scoped,
            tool_note,
        ),
        (Capability::NetworkWeb, Some(Pattern::Domain { domain })) => {
            if domain.starts_with("*.") {
                if rule.effect == Effect::Allow {
                    Translation::Unsupported("a wildcard domain has no documented WebFetch form".to_owned())
                } else {
                    native(
                        vec!["WebFetch".to_owned()],
                        ReceiptClass::ToolScopedEmulation,
                        "a wildcard domain has no documented form: every WebFetch is covered (stricter); tool-scoped",
                    )
                }
            } else {
                native(vec![format!("WebFetch(domain:{domain})")], scoped, tool_note)
            }
        }
        (Capability::NetworkWeb, _) => Translation::Unsupported("pattern kind".to_owned()),
        (Capability::McpToolCall, Some(Pattern::Mcp { server, tool })) => {
            let e = match tool {
                None => format!("mcp__{server}"),
                Some(t) => format!("mcp__{server}__{t}"),
            };
            native(vec![e], ReceiptClass::Exact, "MCP rule")
        }
        (Capability::McpToolCall, _) => {
            Translation::Unsupported("every MCP server has no documented wildcard form".to_owned())
        }
    }
}

/// Compiles every capability through Claude-compatible rule syntax, including the omitted
/// capabilities' defaults. `grok`: the vendor has no ask list.
pub(super) fn compile_rules(
    cx: &mut Cx,
    e: &mut Entries,
    grok: bool,
) -> Result<(), CompileRefusal> {
    let vendor = cx.vendor;
    if grok {
        // No ask list: reads are never prompted, and an ask on a write capability cannot be
        // stated, so no allow that could cover the asked path may stay (Edit and Write rules
        // of the workspace and of the protected paths overlap).
        if let Some(r) = cx
            .rules(Capability::FsReadWorkspace)
            .iter()
            .find(|r| r.effect == Effect::Ask)
        {
            return Err(CompileRefusal::UnenforceableAsk {
                vendor,
                rule: r.describe(),
                reason: "workspace reads are never prompted, so the ask would run".to_owned(),
            });
        }
        // The workspace root is not known here, so an external glob may cover the workspace
        // (and a workspace or protected path may sit under an external glob): the overlap
        // cannot be ruled out, so every capability of the group is withdrawn.
        let asked = |caps: &[Capability], cx: &Cx| {
            caps.iter()
                .any(|c| cx.rules(*c).iter().any(|r| r.effect == Effect::Ask))
        };
        let writes = [
            Capability::FsWriteWorkspace,
            Capability::FsWriteProtected,
            Capability::FsWriteExternal,
        ];
        let reads = [Capability::FsReadWorkspace, Capability::FsReadExternal];
        if asked(&writes, cx) {
            cx.forced_withdraw.extend(writes);
        }
        if asked(&reads, cx) {
            cx.forced_withdraw.extend(reads);
        }
    }
    for cap in Capability::ALL {
        if cx.rules(cap).is_empty() {
            continue;
        }
        cx.drive(cap, surface_of(cap), e, &|r| {
            if grok && r.effect == Effect::Ask {
                Translation::Unsupported("--allow/--deny have no ask form".to_owned())
            } else {
                translate(vendor, r)
            }
        })?;
    }
    if cx.policy.posture == Posture::Bypass {
        return Ok(());
    }
    // The protected default: deny every class no rule names.
    // A class is covered by a rule that is at least as strict as the default, or that the
    // vendor can honour: an ask that cannot be stated falls back to the default deny.
    let forced = cx.forced_withdraw.contains(&Capability::FsWriteProtected);
    let covered: Vec<Option<PathClass>> = cx
        .rules(Capability::FsWriteProtected)
        .iter()
        .filter(|r| !(grok && r.effect == Effect::Ask) && !(forced && r.effect != Effect::Deny))
        .map(|r| match &r.pattern {
            Some(Pattern::PathClass { class }) => Some(*class),
            _ => None,
        })
        .collect();
    let all_covered = covered.iter().any(Option::is_none);
    for class in PathClass::ALL {
        if all_covered || covered.contains(&Some(class)) {
            continue;
        }
        let rule = Rule::new(
            Capability::FsWriteProtected,
            Effect::Deny,
            Some(Pattern::PathClass { class }),
        );
        if let Translation::Native {
            entries,
            class: rc,
            note,
        } = translate(vendor, &rule)
        {
            e.deny.extend(entries.iter().cloned());
            cx.push(
                ReceiptSubject::Capability(Capability::FsWriteProtected),
                Origin::Default,
                Some(Effect::Deny),
                format!(
                    "deny fs_write_protected class:{} (canonical default)",
                    class.as_str()
                ),
                entries,
                rc,
                Surface::ProtectedPath,
                note,
            );
        }
    }
    for cap in cx.omitted() {
        if cap == Capability::FsWriteProtected {
            continue;
        }
        cx.default_line(
            cap,
            surface_of(cap),
            Vec::new(),
            ReceiptClass::Exact,
            "no entry: the posture's native prompt path decides unmatched requests",
        );
    }
    Ok(())
}

/// The `--permission-mode` value for a posture.
pub(super) fn mode(posture: Posture, ask_value: &'static str) -> &'static str {
    match posture {
        Posture::Ask => ask_value,
        Posture::AcceptEdits => "acceptEdits",
        Posture::Auto => "auto",
        Posture::Plan => "plan",
        Posture::Bypass => "bypassPermissions",
    }
}

pub(super) fn compile(cx: &mut Cx) -> Result<(), CompileRefusal> {
    let posture = cx.policy.posture;
    let mut args = vec![
        "--permission-mode".to_owned(),
        mode(posture, "manual").to_owned(),
    ];
    let mut note = "launch-fixed here; switchable in the TUI only inside the launch-offered set";
    if posture == Posture::Bypass {
        // The offer cannot be granted mid-session; bypass itself is the permission mode.
        args.push("--allow-dangerously-skip-permissions".to_owned());
        note = "bypass maps to bypassPermissions and needs the launch offer";
    }
    cx.args.extend(args.iter().cloned());
    cx.push(
        ReceiptSubject::Posture,
        Origin::Axis,
        None,
        format!("posture {posture:?}"),
        args,
        ReceiptClass::Exact,
        Surface::Posture,
        note,
    );
    match cx.policy.sandbox {
        SandboxChoice::VendorDefault => cx.push(
            ReceiptSubject::Sandbox,
            Origin::Axis,
            None,
            "sandbox vendor-default".to_owned(),
            Vec::new(),
            ReceiptClass::Exact,
            Surface::Sandbox,
            "no launch sandbox control is compiled; the vendor default stands",
        ),
        SandboxChoice::Off => cx.push(
            ReceiptSubject::Sandbox,
            Origin::Axis,
            None,
            "sandbox off".to_owned(),
            Vec::new(),
            ReceiptClass::Unsupported,
            Surface::Sandbox,
            "the compiler cannot switch off a sandbox the vendor config may enable; nothing emitted",
        ),
    }
    if cx.axes_only {
        return Ok(());
    }
    let mut e = Entries::default();
    compile_rules(cx, &mut e, false)?;
    e.sort_dedup();
    let mut perms = Map::new();
    for (k, v) in [("allow", &e.allow), ("ask", &e.ask), ("deny", &e.deny)] {
        if !v.is_empty() {
            perms.insert(k.to_owned(), Value::from(v.clone()));
        }
    }
    if !perms.is_empty() {
        let mut content = serde_json::to_string_pretty(&json!({ "permissions": perms }))
            .map_err(|e| CompileRefusal::InternalInvariant(e.to_string()))?;
        content.push('\n');
        cx.files.push(NativeFile {
            target: ConfigTarget::ClaudeSettings,
            format: NativeFormat::Json,
            content,
        });
    }
    Ok(())
}
