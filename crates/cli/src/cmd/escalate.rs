//! `fl record escalate` and `fl finding escalate` (routing spec §3): the
//! checks that need the working tree, the warning before a repository that
//! is not private, and what a person reads back. The router makes every
//! other check and runs the three steps.

use crate::ctx::Ctx;
use crate::refs::Ref;
use crate::tiers::Tiers;
use anyhow::{Context, Result};
use fl_core::escalation::Outgoing;
use fl_core::ids::Kind;
use fl_core::routing::Tier;
use fl_core::{Iri, Prepared};

/// The refusal in a store with no routing map: it has no tiers, so nothing
/// in it is local in the sense an escalation moves from (routing spec §3.1).
fn not_routed(kind: Kind) -> anyhow::Error {
    anyhow::anyhow!(
        "`fl {} escalate` moves an item from the local tier to GitHub, and this store has no \
         local tier to escalate from: its project declares no routing map. `fl routing set \
         --project <project> <area> <tier>` declares one",
        kind.as_wire()
    )
}

pub fn run(
    ctx: &Ctx<'_>,
    kind: Kind,
    id: &Ref,
    by: Option<&str>,
    reason: Option<&str>,
    abandon: bool,
) -> Result<i32> {
    let Some(t) = ctx.tiers else {
        return Err(not_routed(kind));
    };
    let iri = ctx.resolve_item(kind, id)?;
    // The handle the person knows the item by, read before the escalation
    // turns it into a tombstone's.
    let shown = ctx.show_item(kind, &iri)?;
    if abandon {
        t.router.abandon_escalation(&iri, kind)?;
        println!("{shown}\tabandoned");
        return Ok(0);
    }
    let (Some(by), Some(reason)) = (by, reason) else {
        unreachable!("clap requires --by and --reason unless --abandon is given")
    };
    match escalate(ctx, t, kind, &iri, &shown, by, reason) {
        Ok(issue) => {
            println!("{shown}\tescalated\t{}", ctx.show_item(kind, &issue)?);
            Ok(0)
        }
        // ⚠ Marked: the item refuses every write until the escalation is
        // finished or abandoned (routing spec §3.3). A read of the mark that
        // fails leaves the error as it is.
        Err(e) if matches!(t.router.escalating(&iri), Ok(Some(_))) => {
            let what = kind.as_wire();
            Err(e.context(format!(
                "the escalation of {what} {shown} stopped after its mark was written, and the \
                 local item refuses every write until it is finished. Run `fl {what} escalate \
                 {shown} --by <who> --reason <why>` to finish it, or `fl {what} escalate \
                 {shown} --abandon` to stop it"
            )))
        }
        Err(e) => Err(e),
    }
}

/// The escalation of `iri`, checked, warned about and run: the issue.
fn escalate(
    ctx: &Ctx<'_>,
    t: &Tiers<'_>,
    kind: Kind,
    iri: &Iri,
    shown: &str,
    by: &str,
    reason: &str,
) -> Result<Iri> {
    let prepared = t.router.prepare_escalation(iri, kind)?;
    // An issue an earlier run made passed every check when it was made:
    // only finishing it is left (routing spec §3.3).
    if prepared.found().is_none() {
        check_tree(ctx, &prepared)?;
        warn_disclosure(t, &prepared, by, reason)?;
    }
    if let Some(mark) = prepared.resumes()
        && (mark.by.as_str(), mark.reason.as_str()) != (by, reason)
    {
        eprintln!(
            "note: {shown} resumes the escalation marked by {} for {:?}: the mark's who and why \
             are kept, not the ones given here",
            mark.by, mark.reason
        );
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("the system clock is before 1970")?
        .as_millis();
    let now_ms = u64::try_from(now_ms).unwrap_or(u64::MAX);
    Ok(t.router.escalate(&prepared, by, reason, now_ms)?)
}

/// The checks that need the working tree, before anything is written: the
/// routing map the manifest carries (routing spec §1.2), and a finding's
/// reproduction gate in the committed manifest — the issue names it where
/// another machine reads it (GitHub tracker spec §4.3).
fn check_tree(ctx: &Ctx<'_>, prepared: &Prepared) -> Result<()> {
    let (project, gate) = match prepared.outgoing() {
        Outgoing::Record { record, .. } => (&record.project, None),
        Outgoing::Finding { finding, .. } => (&finding.project, finding.reproduction.as_ref()),
    };
    crate::cmd::manifest::ensure_routing_current(ctx.store, project)?;
    if let Some(gate) = gate {
        crate::cmd::manifest::ensure_publishable(ctx.store, project, Some(gate))?;
    }
    Ok(())
}

/// Routing spec §3.2: on a repository that is not private, say what the
/// escalation publishes before it is written. (Anything sensitive the router
/// has refused already.) A visibility that cannot be read refuses: an
/// unknown visibility is not private.
fn warn_disclosure(t: &Tiers<'_>, prepared: &Prepared, by: &str, reason: &str) -> Result<()> {
    let gh = t.github.open()?;
    let visibility = gh.visibility()?;
    if visibility == "private" {
        return Ok(());
    }
    // A rerun publishes the mark's who and why.
    let (by, reason) = match prepared.resumes() {
        Some(mark) => (mark.by.as_str(), mark.reason.as_str()),
        None => (by, reason),
    };
    // The local IRI is published too: the issue's text names it, and it
    // becomes one of the issue's aliases.
    let iri = prepared.id();
    let what = match prepared.outgoing() {
        Outgoing::Record { record, findings } => format!(
            "this record's title, {:?}, its local IRI, {iri}, the reason, {reason:?}, who \
             escalated it, {by}, and its {} open findings",
            record.title,
            findings.len()
        ),
        Outgoing::Finding { finding, record } => {
            let about = match record.tier {
                Tier::Local => format!(
                    ", and its local record's title, {:?}, and IRI",
                    record.title
                ),
                Tier::Github => String::new(),
            };
            format!(
                "this finding's claim, {:?}, its local IRI, {iri}, the reason, {reason:?}, who \
                 escalated it, {by}{about}",
                finding.claim
            )
        }
    };
    eprintln!(
        "warning: {} is {visibility}: the escalation publishes {what}, and anyone who can read \
         the repository will see them",
        gh.repo().full_name
    );
    Ok(())
}
