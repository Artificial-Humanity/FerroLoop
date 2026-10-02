//! `fl github` (GitHub tracker spec §3.4, §5.4; GitHub ledger spec §6).

use crate::ctx::Ctx;
use crate::refs::{self, Ref};
use anyhow::{Result, bail};
use clap::Subcommand;
use fl_core::Iri;
use std::path::Path;

#[derive(Subcommand)]
pub enum Cmd {
    /// Print who fl writes to GitHub as, the repository it binds, and the
    /// ledger with its mode.
    Whoami,
    /// Rewrite a diverged issue's fl labels and open/closed status from
    /// fl's own record in its body, and leave a comment naming who ran it.
    Repair {
        /// The issue: a handle (`41` or `#41`) or its URL.
        id: Ref,
        /// Who is repairing it; recorded in the comment.
        #[arg(long)]
        by: String,
    },
    /// The GitHub ledger: set it up, walk its history, quarantine a line.
    #[command(subcommand)]
    Ledger(crate::cmd::ledger::Cmd),
}

impl Cmd {
    fn refs(&self) -> Vec<&Ref> {
        match self {
            Cmd::Whoami | Cmd::Ledger(_) => vec![],
            Cmd::Repair { id, .. } => vec![id],
        }
    }
    pub fn iris(&self) -> Vec<Iri> {
        refs::iris(&self.refs())
    }
    pub fn has_handle(&self) -> bool {
        refs::has_handle(&self.refs())
    }
}

/// `root` is the project's root from its config entry: `ledger init` reads
/// the committed manifest there.
pub fn run(ctx: &Ctx<'_>, cmd: Cmd, root: Option<&Path>) -> Result<i32> {
    let Some(gh) = ctx.github else {
        bail!("`fl github` needs the project to be bound to a GitHub repository");
    };
    match cmd {
        Cmd::Whoami => {
            println!("writes as\t{}", gh.identity()?);
            println!("credential\t{}", gh.describe());
            println!("repository\t{}", gh.repo().full_name);
            match ctx.github_ledger {
                None => println!("ledger\tlocal"),
                Some(gl) => {
                    println!("ledger\tgithub");
                    // Spec §6.2: the mode in force, and what is missing when
                    // it is not protected.
                    println!("{}", crate::cmd::ledger::mode_line(&gl.mode()?));
                }
            }
        }
        Cmd::Repair { id, by } => {
            let iri = match &id {
                Ref::Handle(n) => gh.issue_url(*n),
                Ref::Iri(i) => i.clone(),
            };
            let done = gh.repair(&iri, &by)?;
            let word = if done.changed {
                "repaired"
            } else {
                "consistent"
            };
            println!("{word}\t{}\t{}", done.number, done.state);
        }
        Cmd::Ledger(c) => return crate::cmd::ledger::run(ctx, c, root),
    }
    Ok(0)
}
