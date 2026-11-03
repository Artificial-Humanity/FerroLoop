//! Grok Build (spec §1 and §2, "Grok"): Claude-shaped `--permission-mode`, `--allow` and
//! `--deny` rules (the Claude rule compiler is reused), `--disable-web-search`,
//! `--sandbox <PROFILE>` and `--add-dir`.

use super::claude::{compile_rules, mode};
use super::{Cx, Entries, exact_root};
use crate::permissions::facts::Surface;
use crate::permissions::plan::{CompileRefusal, Origin, ReceiptClass, ReceiptSubject};
use crate::permissions::policy::{Capability, Effect, Pattern, Posture, SandboxChoice};

pub(super) fn compile(cx: &mut Cx) -> Result<(), CompileRefusal> {
    let posture = cx.policy.posture;
    let args = vec![
        "--permission-mode".to_owned(),
        mode(posture, "default").to_owned(),
    ];
    cx.args.extend(args.iter().cloned());
    cx.push(
        ReceiptSubject::Posture,
        Origin::Axis,
        None,
        format!("posture {posture:?}"),
        args,
        ReceiptClass::Exact,
        Surface::Posture,
        "launch flag; the TUI can switch mode inside the Claude-shaped set",
    );
    match (cx.policy.sandbox, cx.facts.sandbox_profile.clone()) {
        (SandboxChoice::VendorDefault, Some(p)) => {
            let a = vec!["--sandbox".to_owned(), p];
            cx.args.extend(a.iter().cloned());
            cx.push(
                ReceiptSubject::Sandbox,
                Origin::Axis,
                None,
                "sandbox vendor-default".to_owned(),
                a,
                ReceiptClass::Exact,
                Surface::Sandbox,
                "the caller-verified profile; the sandbox is a separate axis from the rules",
            );
        }
        (SandboxChoice::VendorDefault, None) => cx.push(
            ReceiptSubject::Sandbox,
            Origin::Axis,
            None,
            "sandbox vendor-default".to_owned(),
            Vec::new(),
            ReceiptClass::Exact,
            Surface::Sandbox,
            "no profile named in the facts; the vendor default stands",
        ),
        (SandboxChoice::Off, _) => cx.push(
            ReceiptSubject::Sandbox,
            Origin::Axis,
            None,
            "sandbox off".to_owned(),
            Vec::new(),
            ReceiptClass::Unsupported,
            Surface::Sandbox,
            "no flag turns a sandbox off; nothing emitted",
        ),
    }
    let mut e = Entries::default();
    compile_rules(cx, &mut e, true)?;
    e.sort_dedup();
    for d in &e.deny {
        cx.args.extend(["--deny".to_owned(), d.clone()]);
    }
    for a in &e.allow {
        cx.args.extend(["--allow".to_owned(), a.clone()]);
    }
    // Pattern-less network deny: the hard switch, besides the WebFetch/WebSearch rules.
    let net_off = cx
        .rules(Capability::NetworkWeb)
        .iter()
        .any(|r| r.effect == Effect::Deny && r.pattern.is_none());
    if net_off && posture != Posture::Bypass {
        cx.args.push("--disable-web-search".to_owned());
        cx.push(
            ReceiptSubject::Capability(Capability::NetworkWeb),
            Origin::Rule,
            Some(Effect::Deny),
            "deny network_web (web search hard off)".to_owned(),
            vec!["--disable-web-search".to_owned()],
            ReceiptClass::Exact,
            Surface::NetworkRule,
            "hard off for the search tool; fetches are denied by the rules above",
        );
    }
    // --add-dir grants read and write, so it is emitted only for a root both allowed and
    // nothing restricts external access.
    // A launch root grants read and write, so it also yields to any withdrawn ask group.
    let ext_free = [Capability::FsReadExternal, Capability::FsWriteExternal]
        .iter()
        .all(|c| {
            !cx.forced_withdraw.contains(c)
                && cx.rules(*c).iter().all(|r| r.effect == Effect::Allow)
        });
    if ext_free {
        let roots = |cap| -> Vec<String> {
            cx.rules(cap)
                .iter()
                .filter_map(|r| match &r.pattern {
                    Some(Pattern::AbsPathGlob { glob }) => exact_root(glob),
                    _ => None,
                })
                .collect()
        };
        let reads = roots(Capability::FsReadExternal);
        let mut both: Vec<String> = roots(Capability::FsWriteExternal)
            .into_iter()
            .filter(|r| reads.contains(r))
            .collect();
        both.sort();
        both.dedup();
        for r in both {
            cx.args.extend(["--add-dir".to_owned(), r.clone()]);
            cx.push(
                ReceiptSubject::Capability(Capability::FsWriteExternal),
                Origin::Rule,
                Some(Effect::Allow),
                format!("allow fs_read_external+fs_write_external root {r}"),
                vec!["--add-dir".to_owned(), r],
                ReceiptClass::Exact,
                Surface::FsRule,
                "a launch root grants read and write together; emitted only where both are allowed",
            );
        }
    }
    Ok(())
}
