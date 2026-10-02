//! The pre-flight (GitHub ledger spec §2.4): before any gate or adapter
//! runs, a decision whose ledger is the GitHub ledger checks that its
//! evidence can be published — and refuses before anything runs or is
//! spent when it cannot.

use crate::ctx::Ctx;
use anyhow::{Context, Result, bail};
use fl_core::ids::ProjectId;
use fl_core::split::Outbox;
use fl_core::store::Bindings;
use fl_github::GithubLedger;

/// ⚠ Called once by each decision command — `record move`, `check
/// --record`, `finding reproduce`, `finding verify`, `fl attempt` — before
/// its first gate or adapter. Without the GitHub ledger it checks nothing:
/// a local ledger needs no network.
pub fn check(ctx: &Ctx<'_>, project: &ProjectId) -> Result<()> {
    let Some(gl) = ctx.github_ledger else {
        return Ok(());
    };
    checks(ctx, gl, project).context("refused before any gate or adapter ran")
}

/// In the spec's order, cheapest first.
fn checks(ctx: &Ctx<'_>, gl: &GithubLedger<'_>, project: &ProjectId) -> Result<()> {
    // Read live, every decision; the flush uses this answer (spec §5). A
    // visibility that cannot be read is not private, and refuses.
    gl.visibility()?;
    // The mode is never a refusal — detection-only works (decision 12) —
    // but rules fl cannot read are.
    gl.mode()?;
    // The branch, its descent from the anchor and from the last head this
    // machine saw, and its format (§3.5 checks 1, 2 and 7).
    gl.check_format()?;
    // ⚠ §6.1 step 5: a machine whose root came from the manifest has no
    // cut-over until it runs `init`; without one, its decisions would
    // publish nothing and say so only quietly.
    let repo = gl.repo();
    if ctx.store.cutover(&repo.node_id)?.is_none() {
        bail!(
            "this machine has no cut-over for the GitHub ledger of {}: it knows the ledger's \
             first commit, but `fl github ledger init` never ran here, so no decision made on \
             this machine would be published. Run `fl github ledger init` here; it records this \
             machine's cut-over and changes nothing on GitHub",
            repo.full_name
        );
    }
    // No gate IRI the committed manifest does not list reaches the shared
    // ledger.
    crate::cmd::manifest::ensure_publishable(ctx.store, project, None)?;
    // ⚠ §6.1 step 4, §7: the committed manifest carries the ledger's first
    // commit — every other machine's only anchor. Refused here, at the first
    // decision, so the "export and commit" `init` asked for cannot be lost.
    let held = ctx.store.ledger_root(&repo.node_id)?.with_context(|| {
        format!(
            "this machine records no first commit for the GitHub ledger of {}",
            repo.full_name
        )
    })?;
    crate::cmd::manifest::ensure_manifest_carries_root(ctx.store, project, &repo.node_id, &held)
}
