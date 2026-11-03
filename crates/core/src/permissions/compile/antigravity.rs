//! Antigravity `agy` (spec §1 and §2, "Antigravity"): `--mode`, and
//! `--dangerously-skip-permissions` only for bypass. `permissions.allow` holds exact
//! `command(<exact command>)` entries: a canonical shell prefix compiles by enumeration of the
//! exact commands the caller supplied, never by a prefix or glob. One boolean,
//! `allowNonWorkspaceAccess`, stands for both external read and external write.
//! `artifactReviewPolicy` is left unmanaged. Settings go inline through `--settings`
//! (session-only, never saved), so no shared vendor file is written.

use super::{Cx, safe_token, surface_of};
use crate::permissions::facts::{ProvenFact, Surface};
use crate::permissions::plan::{CompileRefusal, Origin, ReceiptClass, ReceiptSubject};
use crate::permissions::policy::{Capability, Effect, Pattern, Posture, Rule};
use serde_json::{Map, Value, json};

/// `*` is the only wildcard. Iterative two-pointer matching: at most `pattern.len() *
/// s.len()` steps, with no backtracking blow-up.
pub(crate) fn wild_match(pat: &str, s: &str) -> bool {
    let (p, s) = (pat.as_bytes(), s.as_bytes());
    let (mut pi, mut si) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Cheap rejections before the matcher: the literal bytes the glob needs must fit, and its
/// literal head must be a prefix.
fn glob_could_match(glob: &str, s: &str) -> bool {
    let needed = glob.bytes().filter(|b| *b != b'*').count();
    let head = glob.split('*').next().unwrap_or("");
    needed <= s.len() && s.starts_with(head)
}

/// Most byte steps (a bound on matcher work) one compile may spend on matching.
const MATCH_BUDGET: usize = 100_000_000;

/// An upper bound on the matcher steps for one (rule, command) pair.
fn pair_cost(rule: &Rule, cmd: &str) -> usize {
    let p = match &rule.pattern {
        Some(Pattern::ShellGlob { glob }) => glob.len(),
        Some(Pattern::ArgvPrefix { argv }) => argv.len(),
        _ => 0,
    };
    (p + 1) * (cmd.len() + 1)
}

fn plain_command(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 512
        && !c.starts_with(' ')
        && !c.ends_with(' ')
        && !c.contains("  ")
        && c.split(' ').all(safe_token)
}

fn covers(rule: &Rule, cmd: &str) -> bool {
    match &rule.pattern {
        None => true,
        Some(Pattern::ArgvPrefix { argv }) => {
            let t: Vec<&str> = cmd.split(' ').collect();
            t.len() >= argv.len() && t.iter().zip(argv).all(|(a, b)| a == b)
        }
        Some(Pattern::ShellGlob { glob }) => glob_could_match(glob, cmd) && wild_match(glob, cmd),
        Some(_) => false,
    }
}

pub(super) fn compile(cx: &mut Cx) -> Result<(), CompileRefusal> {
    let vendor = cx.vendor;
    let posture = cx.policy.posture;
    let bypass = posture == Posture::Bypass;

    let (args, note): (Vec<String>, &str) = match posture {
        Posture::Ask => (
            vec![],
            "no flag: the vendor prompts by default (no explicit ask mode exists)",
        ),
        Posture::AcceptEdits => (vec!["--mode".into(), "accept-edits".into()], "launch flag"),
        Posture::Auto => (vec!["--mode".into(), "auto".into()], "launch flag"),
        Posture::Plan => (vec!["--mode".into(), "plan".into()], "launch flag"),
        Posture::Bypass => (
            vec!["--dangerously-skip-permissions".into()],
            "bypass; restart required",
        ),
    };
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
    cx.push(
        ReceiptSubject::Sandbox,
        Origin::Axis,
        None,
        format!(
            "sandbox {}",
            if cx.policy.sandbox == crate::permissions::policy::SandboxChoice::Off {
                "off"
            } else {
                "vendor-default"
            }
        ),
        vec![],
        ReceiptClass::Exact,
        Surface::Sandbox,
        "no --sandbox flag is compiled; terminal restrictions stay as the vendor ships them",
    );
    cx.push(
        ReceiptSubject::Posture,
        Origin::Axis,
        None,
        "artifactReviewPolicy".to_owned(),
        vec![],
        ReceiptClass::Unsupported,
        Surface::Posture,
        "a vendor-local review posture with no canonical counterpart: left unmanaged",
    );

    let mut commands: Vec<String> = Vec::new();
    let shell_rules = cx.rules(Capability::ShellExec);
    let mut work = 0usize;
    let mut non_ws: Option<bool> = None;

    for cap in Capability::ALL {
        let rules = cx.rules(cap);
        let surface = surface_of(cap);
        let sub = ReceiptSubject::Capability(cap);
        if rules.is_empty() {
            if bypass {
                continue;
            }
            match cap {
                Capability::FsReadExternal | Capability::FsWriteExternal => {
                    non_ws = Some(false);
                    cx.default_line(cap, surface, vec!["allowNonWorkspaceAccess=false".to_owned()], ReceiptClass::Exact,
                        "outside-workspace access prompts; one boolean stands for read and write");
                }
                Capability::FsWriteProtected | Capability::NetworkWeb | Capability::McpToolCall => cx.default_line(cap, surface, vec![], ReceiptClass::Unsupported,
                    "the vendor has no grammar for this capability; the default is not enforced"),
                _ => cx.default_line(cap, surface, vec![], ReceiptClass::Exact, "the mode and the prompt decide unmatched requests"),
            }
            continue;
        }
        let hard = cx.proves(&ProvenFact::HardDeny(cap));
        let restricted = rules.iter().any(|r| r.effect != Effect::Allow);
        let mut ask_seen = false;
        for r in &rules {
            if r.effect == Effect::Deny {
                // Every deny needs a proven hard restriction: nothing here denies natively.
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
                    continue;
                }
                return Err(cx.refuse_deny(
                    r,
                    match cap {
                        Capability::FsReadExternal | Capability::FsWriteExternal => {
                            "allowNonWorkspaceAccess=false asks; it is not a proven hard deny"
                        }
                        _ => "the vendor has no native deny for this capability",
                    },
                ));
            }
            match (cap, r.effect) {
                (_, Effect::Deny) => unreachable!("a deny is settled before the match"),
                (Capability::FsReadWorkspace, Effect::Allow) => cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(), vec![],
                    if r.pattern.is_none() { ReceiptClass::Exact } else { ReceiptClass::Unsupported }, surface,
                    if r.pattern.is_none() { "the workspace is readable" } else { "the workspace is readable as a whole; a narrower allow is not narrowed and nothing was emitted" }),
                (Capability::FsReadWorkspace, Effect::Ask) => return Err(CompileRefusal::UnenforceableAsk {
                    vendor, rule: r.describe(), reason: "workspace reads are not prompted".into() }),
                (Capability::FsWriteWorkspace, Effect::Allow) => {
                    if restricted {
                        cx.withdrawn_allow(r, surface);
                    } else if r.pattern.is_none() && matches!(posture, Posture::AcceptEdits | Posture::Auto) {
                        cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(), vec![], ReceiptClass::Exact, surface,
                            "the mode accepts workspace edits");
                    } else if r.pattern.is_none() {
                        cx.dropped_allow(r, surface, "no flag allows workspace edits outside accept-edits or auto");
                    } else {
                        cx.dropped_allow(r, surface, "no path grammar");
                    }
                }
                (Capability::FsWriteWorkspace | Capability::FsWriteProtected, Effect::Ask) => cx.push(sub, Origin::Rule, Some(Effect::Ask), r.describe(), vec![],
                    ReceiptClass::StricterDowngrade, surface, "every write prompts under this posture (stricter for a pattern)"),
                (Capability::FsReadExternal | Capability::FsWriteExternal, Effect::Ask) => {
                    non_ws = Some(false);
                    cx.push(sub, Origin::Rule, Some(Effect::Ask), r.describe(), vec!["allowNonWorkspaceAccess=false".to_owned()],
                        ReceiptClass::StricterDowngrade, surface, "one boolean stands for external read and write; false prompts for both");
                }
                (Capability::FsReadExternal | Capability::FsWriteExternal, Effect::Allow) => {
                    let other = if cap == Capability::FsReadExternal { Capability::FsWriteExternal } else { Capability::FsReadExternal };
                    let other_ok = {
                        let o = cx.rules(other);
                        !o.is_empty() && o.iter().all(|x| x.effect == Effect::Allow && x.pattern.is_none())
                    };
                    if restricted {
                        cx.withdrawn_allow(r, surface);
                    } else if r.pattern.is_some() {
                        cx.dropped_allow(r, surface, "no path grammar: the boolean opens every external path");
                    } else if !other_ok {
                        cx.dropped_allow(r, surface, "the single boolean would also open the other external capability");
                    } else {
                        non_ws = Some(true);
                        cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(), vec!["allowNonWorkspaceAccess=true".to_owned()],
                            ReceiptClass::Exact, surface, "one boolean stands for both external read and write; both are allowed here");
                    }
                }
                (Capability::ShellExec, Effect::Ask) => {
                    ask_seen = true;
                    cx.push(sub, Origin::Rule, Some(Effect::Ask), r.describe(), vec![], ReceiptClass::Exact, surface,
                        "everything outside the enumerated allows prompts; matching commands are removed from the enumeration");
                }
                (Capability::ShellExec, Effect::Allow) => {
                    if r.pattern.is_none() {
                        cx.dropped_allow(r, surface, "an all-commands allow has no exact-command form");
                        continue;
                    }
                    let mut total = 0usize;
                    let mut skipped = 0usize;
                    let mut mine: Vec<String> = Vec::new();
                    for c in &cx.facts.exact_commands {
                        work += pair_cost(r, c)
                            + shell_rules.iter().map(|o| pair_cost(o, c)).sum::<usize>();
                        if work > MATCH_BUDGET {
                            return Err(CompileRefusal::InvalidFacts(
                                "too many exact commands for the shell rules to match within the work budget".into(),
                            ));
                        }
                        if !covers(r, c) {
                            continue;
                        }
                        total += 1;
                        if !plain_command(c) {
                            skipped += 1;
                            continue;
                        }
                        let blocked = shell_rules.iter().any(|o| o.effect != Effect::Allow && covers(o, c));
                        if !blocked {
                            mine.push(c.clone());
                        }
                    }
                    mine.sort();
                    mine.dedup();
                    commands.extend(mine.iter().cloned());
                    let n = mine.len();
                    cx.push(sub, Origin::Rule, Some(Effect::Allow), r.describe(),
                        mine.iter().map(|c| format!("command({c})")).collect(), ReceiptClass::StricterDowngrade, surface,
                        format!("enumeration: {n} supplied exact command(s) pre-seeded ({skipped} of {total} matching skipped as not plain); everything else stays ask"));
                }
                (Capability::NetworkWeb | Capability::McpToolCall, Effect::Ask) => {
                    ask_seen = true;
                    cx.push(sub, Origin::Rule, Some(Effect::Ask), r.describe(), vec![], ReceiptClass::StricterDowngrade, surface,
                        "no grammar: the vendor prompt decides, and allows of this capability are withdrawn");
                }
                (Capability::NetworkWeb | Capability::McpToolCall | Capability::FsWriteProtected, Effect::Allow) => {
                    if ask_seen || restricted { cx.withdrawn_allow(r, surface) } else { cx.dropped_allow(r, surface, "the vendor has no grammar for this capability") }
                }
            }
        }
        if cap == Capability::FsWriteProtected {
            cx.protected_class_defaults(
                ReceiptClass::Unsupported,
                vec![],
                "the vendor has no grammar for this capability; the default is not enforced",
            );
        }
    }

    let mut settings = Map::new();
    commands.sort();
    commands.dedup();
    if !commands.is_empty() {
        let allow: Vec<String> = commands.iter().map(|c| format!("command({c})")).collect();
        settings.insert("permissions".to_owned(), json!({ "allow": allow }));
    }
    if let Some(b) = non_ws {
        settings.insert("allowNonWorkspaceAccess".to_owned(), Value::Bool(b));
    }
    if !settings.is_empty() {
        let s = serde_json::to_string(&Value::Object(settings))
            .map_err(|e| CompileRefusal::InternalInvariant(e.to_string()))?;
        cx.args.extend(["--settings".to_owned(), s]);
    }
    Ok(())
}
