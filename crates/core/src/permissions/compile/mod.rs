//! The five-vendor fail-closed compiler.
//!
//! `compile` reads a policy and the caller's facts and returns native launch arguments and
//! files, or a typed refusal. The rules it follows:
//!
//! * An allow that has no exact native form is **not granted** (it never becomes an allow);
//!   the receipt says whether the vendor falls back to a proven prompt channel or to a deny.
//! * A deny that has no native enforcement is **refused**, unless a strictly stronger native
//!   form exists, which is used and receipted as a downgrade. A deny never becomes an ask.
//! * An ask that has no native form falls to the vendor's prompt path, and every allow of the
//!   same capability is withdrawn, since an allow would outrank the ask.
//! * Bypass flags exist only for [`Posture::Bypass`]; a sandbox-off flag only for
//!   [`SandboxChoice::Off`]. A final audit refuses a plan that breaks either.
//! * Nothing is `RuntimeProven` unless the facts say so.

mod antigravity;
mod claude;
mod codex;
mod grok;
mod muse;

use super::facts::{ChangeField, ProvenFact, Surface, Vendor, VendorFacts};
use super::plan::{
    ChangeClass, ChangeClassEntry, ChangeKey, CompileRefusal, CompiledPlan, EvidenceClass,
    NativeFile, Origin, ReceiptClass, ReceiptLine, ReceiptSubject, UnenforcedItem, evidence_for,
};
use std::collections::BTreeSet;

use super::policy::{
    Capability, Effect, PathClass, Pattern, PermissionPolicy, Posture, Rule, SandboxChoice,
    UnenforcedDefaults, canonical_default,
};

/// Compiles `policy` for `vendor`. Pure and deterministic: the same inputs give the same
/// bytes.
pub fn compile(
    vendor: Vendor,
    facts: &VendorFacts,
    policy: &PermissionPolicy,
) -> Result<CompiledPlan, CompileRefusal> {
    facts.validate().map_err(CompileRefusal::InvalidFacts)?;
    if !vendor.supports(facts.transport) {
        return Err(CompileRefusal::UnsupportedTransport {
            vendor,
            transport: facts.transport,
        });
    }
    let policy = policy.normalized().map_err(CompileRefusal::InvalidPolicy)?;
    let mut cx = Cx {
        vendor,
        facts,
        policy: &policy,
        args: Vec::new(),
        files: Vec::new(),
        receipts: Vec::new(),
        forced_withdraw: BTreeSet::new(),
    };
    match vendor {
        Vendor::Claude => claude::compile(&mut cx)?,
        Vendor::Codex => codex::compile(&mut cx)?,
        Vendor::Muse => muse::compile(&mut cx)?,
        Vendor::Grok => grok::compile(&mut cx)?,
        Vendor::Antigravity => antigravity::compile(&mut cx)?,
    }
    cx.finish()
}

/// Compile state shared by the vendor modules.
pub(crate) struct Cx<'a> {
    pub vendor: Vendor,
    pub facts: &'a VendorFacts,
    pub policy: &'a PermissionPolicy,
    pub args: Vec<String>,
    pub files: Vec<NativeFile>,
    pub receipts: Vec<ReceiptLine>,
    /// Capabilities whose allows are withdrawn because an ask elsewhere could not be stated.
    pub forced_withdraw: BTreeSet<Capability>,
}

/// Native entries by effect.
#[derive(Default)]
pub(crate) struct Entries {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

impl Entries {
    pub fn by(&mut self, effect: Effect) -> &mut Vec<String> {
        match effect {
            Effect::Allow => &mut self.allow,
            Effect::Ask => &mut self.ask,
            Effect::Deny => &mut self.deny,
        }
    }

    pub fn sort_dedup(&mut self) {
        for v in [&mut self.allow, &mut self.ask, &mut self.deny] {
            v.sort();
            v.dedup();
        }
    }
}

/// The native form of one rule.
pub(crate) enum Translation {
    Native {
        entries: Vec<String>,
        class: ReceiptClass,
        note: String,
    },
    Unsupported(String),
}

impl Cx<'_> {
    pub fn ev(&self, surface: Surface) -> EvidenceClass {
        evidence_for(self.vendor, surface, self.facts)
    }

    pub fn proves(&self, fact: &ProvenFact) -> bool {
        self.facts.proves(fact)
    }

    pub fn interactive(&self) -> bool {
        self.proves(&ProvenFact::InteractiveAsk)
    }

    pub fn rules(&self, cap: Capability) -> Vec<Rule> {
        self.policy.rules_for(cap).cloned().collect()
    }

    /// Capabilities with no rule. Empty under bypass, which disables the defaults.
    pub fn omitted(&self) -> Vec<Capability> {
        if self.policy.posture == Posture::Bypass {
            return Vec::new();
        }
        Capability::ALL
            .into_iter()
            .filter(|c| self.policy.rules_for(*c).next().is_none())
            .collect()
    }

    /// The canonical protected-path default for the classes no rule names, when some rule of
    /// `fs_write_protected` exists (with none, the vendor's whole-capability default line
    /// applies). A rule with no pattern addresses every class. Each uncovered class gets its
    /// own `default` receipt line, so the unenforced check never goes silent for a class
    /// because another class carries an allow.
    pub fn protected_class_defaults(
        &mut self,
        class: ReceiptClass,
        native: Vec<String>,
        note: &str,
    ) {
        if self.policy.posture == Posture::Bypass {
            return;
        }
        let rules = self.rules(Capability::FsWriteProtected);
        if rules.is_empty() || rules.iter().any(|r| r.pattern.is_none()) {
            return;
        }
        for c in PathClass::ALL {
            let named = rules
                .iter()
                .any(|r| matches!(&r.pattern, Some(Pattern::PathClass { class }) if *class == c));
            if named {
                continue;
            }
            self.push(
                ReceiptSubject::Capability(Capability::FsWriteProtected),
                Origin::Default,
                Some(Effect::Deny),
                format!(
                    "deny fs_write_protected class:{} (canonical default)",
                    c.as_str()
                ),
                native.clone(),
                class,
                Surface::ProtectedPath,
                note,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        subject: ReceiptSubject,
        origin: Origin,
        effect: Option<Effect>,
        requested: String,
        native: Vec<String>,
        class: ReceiptClass,
        surface: Surface,
        note: impl Into<String>,
    ) {
        // A line the vendor cannot carry is never `RuntimeProven`, whatever the facts say.
        let evidence = if class == ReceiptClass::Unsupported {
            evidence_for(
                self.vendor,
                surface,
                &VendorFacts::new(self.facts.transport),
            )
        } else {
            self.ev(surface)
        };
        self.receipts.push(ReceiptLine {
            subject,
            origin,
            effect,
            requested,
            native,
            class,
            evidence,
            note: note.into(),
        });
    }

    /// A receipt line for an omitted capability's canonical default.
    pub fn default_line(
        &mut self,
        cap: Capability,
        surface: Surface,
        native: Vec<String>,
        class: ReceiptClass,
        note: &str,
    ) {
        let eff = canonical_default(cap);
        self.push(
            ReceiptSubject::Capability(cap),
            Origin::Default,
            Some(eff),
            format!("{} {} (canonical default)", eff.as_str(), cap.as_str()),
            native,
            class,
            surface,
            note,
        );
    }

    pub fn refuse_deny(&self, rule: &Rule, reason: impl Into<String>) -> CompileRefusal {
        CompileRefusal::UnenforceableDeny {
            vendor: self.vendor,
            rule: rule.describe(),
            reason: reason.into(),
        }
    }

    /// Records an allow that was not granted.
    pub fn dropped_allow(&mut self, rule: &Rule, surface: Surface, reason: &str) {
        let tail = if self.interactive() {
            "not granted; the request falls to the native prompt (interactive channel proven)"
        } else {
            "not granted; no proven prompt channel, so it is treated as denied (not natively enforced)"
        };
        self.push(
            ReceiptSubject::Capability(rule.capability),
            Origin::Rule,
            Some(Effect::Allow),
            rule.describe(),
            Vec::new(),
            ReceiptClass::StricterDowngrade,
            surface,
            format!("{reason}; {tail}"),
        );
    }

    /// Records an allow withdrawn because an ask of the same capability has no native form.
    pub fn withdrawn_allow(&mut self, rule: &Rule, surface: Surface) {
        self.push(
            ReceiptSubject::Capability(rule.capability),
            Origin::Rule,
            Some(Effect::Allow),
            rule.describe(),
            Vec::new(),
            ReceiptClass::StricterDowngrade,
            surface,
            "withdrawn: an ask that covers this capability has no native form, and an allow would outrank it",
        );
    }

    /// Compiles every rule of `cap` through `translate` into `out`.
    pub fn drive(
        &mut self,
        cap: Capability,
        surface: Surface,
        out: &mut Entries,
        translate: &dyn Fn(&Rule) -> Translation,
    ) -> Result<(), CompileRefusal> {
        let mut ask_unsupported = self.forced_withdraw.contains(&cap);
        for rule in self.rules(cap) {
            let tr = translate(&rule);
            match (rule.effect, tr) {
                (Effect::Allow, _) if ask_unsupported => self.withdrawn_allow(&rule, surface),
                (Effect::Allow, Translation::Unsupported(why)) => {
                    self.dropped_allow(&rule, surface, &why);
                }
                (Effect::Deny, Translation::Unsupported(why)) => {
                    return Err(self.refuse_deny(&rule, why));
                }
                (Effect::Ask, Translation::Unsupported(why)) => {
                    ask_unsupported = true;
                    self.push(
                        ReceiptSubject::Capability(cap),
                        Origin::Rule,
                        Some(Effect::Ask),
                        rule.describe(),
                        Vec::new(),
                        ReceiptClass::StricterDowngrade,
                        surface,
                        format!("{why}; the native prompt path covers it and allows of this capability are withdrawn"),
                    );
                }
                (
                    effect,
                    Translation::Native {
                        entries,
                        class,
                        note,
                    },
                ) => {
                    out.by(effect).extend(entries.iter().cloned());
                    self.push(
                        ReceiptSubject::Capability(cap),
                        Origin::Rule,
                        Some(effect),
                        rule.describe(),
                        entries,
                        class,
                        surface,
                        note,
                    );
                }
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<CompiledPlan, CompileRefusal> {
        // A policy that requires filesystem enforcement is refused unless every filesystem
        // restriction is bound by a boundary, or the facts prove coverage.
        if self.policy.require_fs_enforcement && !self.proves(&ProvenFact::FsEnforcementCoverage) {
            for l in &self.receipts {
                let ReceiptSubject::Capability(c) = l.subject else {
                    continue;
                };
                let restriction = matches!(
                    (l.origin, l.effect),
                    (Origin::Rule, Some(Effect::Deny | Effect::Ask))
                        | (Origin::Default, Some(Effect::Deny))
                );
                if c.is_fs()
                    && restriction
                    && matches!(
                        l.class,
                        ReceiptClass::ToolScopedEmulation | ReceiptClass::Unsupported
                    )
                {
                    return Err(CompileRefusal::FsEnforcementRequired {
                        vendor: self.vendor,
                        rule: l.requested.clone(),
                        class: l.class,
                    });
                }
            }
        }
        // A deny (a canonical default or a request) the vendor cannot enforce is refused unless
        // the policy acknowledges it, in which case it is listed on the plan.
        let mut unenforced: Vec<UnenforcedItem> = Vec::new();
        for l in &self.receipts {
            let ReceiptSubject::Capability(c) = l.subject else {
                continue;
            };
            if l.effect == Some(Effect::Deny) && l.class == ReceiptClass::Unsupported {
                if self.policy.unenforced_defaults == UnenforcedDefaults::Refuse {
                    return Err(CompileRefusal::UnenforcedRequest {
                        vendor: self.vendor,
                        capability: c,
                        request: l.requested.clone(),
                    });
                }
                unenforced.push(UnenforcedItem {
                    capability: c,
                    vendor: self.vendor,
                    kind: Effect::Deny,
                });
            } else if l.effect == Some(Effect::Ask) && l.class == ReceiptClass::Unsupported {
                // An ask the vendor does not gate never refuses; it is listed for the UI.
                unenforced.push(UnenforcedItem {
                    capability: c,
                    vendor: self.vendor,
                    kind: Effect::Ask,
                });
            }
        }
        unenforced.sort();
        unenforced.dedup();
        let change_classes = self.change_classes();
        // Respawn matters only for a field this plan actually sets.
        let carried = |s: ReceiptSubject| {
            self.receipts
                .iter()
                .any(|l| l.subject == s && !l.native.is_empty())
        };
        let requires_respawn = change_classes.iter().any(|c| {
            c.class == ChangeClass::Respawn
                && carried(match &c.field {
                    ChangeField::Posture => ReceiptSubject::Posture,
                    ChangeField::Sandbox => ReceiptSubject::Sandbox,
                    ChangeField::Rules(cap) => ReceiptSubject::Capability(*cap),
                })
        });
        let plan = CompiledPlan {
            vendor: self.vendor,
            key: ChangeKey {
                vendor: self.vendor,
                transport: self.facts.transport,
                installed_version: self.facts.installed_version.clone(),
            },
            launch_args: std::mem::take(&mut self.args),
            config_files: std::mem::take(&mut self.files),
            env: Vec::new(),
            receipts: std::mem::take(&mut self.receipts),
            change_classes,
            requires_respawn,
            unenforced,
        };
        audit(&plan, self.policy)?;
        Ok(plan)
    }

    fn change_classes(&self) -> Vec<ChangeClassEntry> {
        let hot = |f: &ChangeField| self.proves(&ProvenFact::HotEdit(f.clone()));
        let mut v = vec![
            ChangeClassEntry {
                field: ChangeField::Posture,
                class: if hot(&ChangeField::Posture) {
                    ChangeClass::Session
                } else {
                    ChangeClass::Respawn
                },
            },
            // Sandbox changes always say respawn, whatever the facts claim.
            ChangeClassEntry {
                field: ChangeField::Sandbox,
                class: ChangeClass::Respawn,
            },
        ];
        for cap in Capability::ALL {
            let f = ChangeField::Rules(cap);
            let carried = rule_carrier(self.vendor, cap);
            v.push(ChangeClassEntry {
                class: if hot(&f) { ChangeClass::Hot } else { carried },
                field: f,
            });
        }
        v
    }
}

/// How a capability's rules reach the vendor, as a default change class: a file the vendor
/// reads at start is `PendingNextStart`; a launch argument or a sandbox mode is `Respawn`.
fn rule_carrier(vendor: Vendor, cap: Capability) -> ChangeClass {
    match vendor {
        Vendor::Claude => ChangeClass::PendingNextStart,
        Vendor::Codex if cap == Capability::ShellExec => ChangeClass::PendingNextStart,
        Vendor::Codex | Vendor::Muse | Vendor::Grok | Vendor::Antigravity => ChangeClass::Respawn,
    }
}

const BYPASS_TOKENS: [&str; 7] = [
    "--yolo",
    "--disable-approval",
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--dangerously-bypass-approvals-and-sandbox",
    "--always-approve",
    "bypassPermissions",
];
const SANDBOX_OFF_TOKENS: [&str; 4] = [
    "--yolo",
    "--disable-sandbox",
    "danger-full-access",
    "--dangerously-bypass-approvals-and-sandbox",
];

/// Defence in depth: a plan that carries a bypass or sandbox-off construct without the axis
/// that asked for it is never returned.
fn audit(plan: &CompiledPlan, policy: &PermissionPolicy) -> Result<(), CompileRefusal> {
    let bad = |what: String| CompileRefusal::InternalInvariant(what);
    let arg = |tok: &str| plan.launch_args.iter().any(|a| a == tok);
    // Structural keys only: a user pattern that merely spells a flag is not a flag.
    let file_line = |needle: &str| {
        plan.config_files.iter().any(|f| {
            f.content
                .lines()
                .any(|l| l.trim_start().starts_with(needle))
        })
    };
    if policy.posture != Posture::Bypass {
        for t in BYPASS_TOKENS {
            if arg(t) {
                return Err(bad(format!("`{t}` without a bypass posture")));
            }
        }
        for (i, a) in plan.launch_args.iter().enumerate() {
            if matches!(a.as_str(), "-a" | "--ask-for-approval" | "--approval-mode")
                && plan.launch_args.get(i + 1).is_some_and(|v| v == "never")
            {
                return Err(bad("approval `never` without a bypass posture".to_owned()));
            }
        }
        if file_line("approval_policy = \"never\"")
            || file_line("\"defaultMode\": \"bypassPermissions\"")
        {
            return Err(bad("a bypass setting without a bypass posture".to_owned()));
        }
    }
    if policy.sandbox != SandboxChoice::Off {
        for t in SANDBOX_OFF_TOKENS {
            if arg(t) {
                return Err(bad(format!("`{t}` without sandbox off")));
            }
        }
        if file_line("sandbox_mode = \"danger-full-access\"") {
            return Err(bad("danger-full-access without sandbox off".to_owned()));
        }
    }
    Ok(())
}

/// The evidence surface a capability's rules belong to.
pub(crate) fn surface_of(cap: Capability) -> Surface {
    match cap {
        Capability::FsReadWorkspace
        | Capability::FsReadExternal
        | Capability::FsWriteWorkspace
        | Capability::FsWriteExternal => Surface::FsRule,
        Capability::FsWriteProtected => Surface::ProtectedPath,
        Capability::ShellExec => Surface::ShellRule,
        Capability::NetworkWeb => Surface::NetworkRule,
        Capability::McpToolCall => Surface::McpRule,
    }
}

/// Tokens that need no shell quoting and no glob escaping.
pub(crate) fn safe_token(t: &str) -> bool {
    !t.is_empty()
        && t.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '.' | '/' | ':' | '=' | '@' | '%' | '+' | ',' | '-')
        })
}

/// The literal directory prefix of a glob, before its first wildcard segment.
pub(crate) fn literal_prefix(glob: &str) -> Vec<&str> {
    glob.split('/')
        .filter(|s| !s.is_empty())
        .take_while(|s| !s.contains('*') && !s.contains('?'))
        .collect()
}

/// `P` or `P/**` with a wildcard-free `P`: a directory subtree, exactly.
pub(crate) fn exact_root(glob: &str) -> Option<String> {
    let p = glob.strip_suffix("/**").unwrap_or(glob);
    if p.is_empty() || p.contains('*') || p.contains('?') {
        return None;
    }
    Some(p.to_owned())
}

/// Whether two literal path prefixes nest, either way.
pub(crate) fn prefixes_overlap(a: &[&str], b: &[&str]) -> bool {
    let n = a.len().min(b.len());
    a[..n] == b[..n]
}
