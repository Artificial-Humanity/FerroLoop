//! The GitHub ledger (GitHub ledger spec §1.1, §3): the `fl/ledger` branch
//! of the repository that backs the tracker, where mode B publishes each
//! decision and the evidence it rests on.

pub mod disclose;
mod git;
pub mod layout;
mod read;

use crate::client::Client;
use crate::tracker::Repo;
use fl_core::split::LedgerMemory;
use std::time::Duration;

pub use disclose::Visibility;

/// The GitHub side of mode B, for one repository and one command.
///
/// ⚠ It borrows the tracker's `Client` — one credential, one origin guard,
/// one rate-limit handling (spec §1.1) — and the local store, which keeps
/// the anchor, the last head this machine checked, and every file it read.
pub struct GithubLedger<'a> {
    pub(crate) client: &'a Client,
    pub(crate) repo: Repo,
    pub(crate) local: &'a dyn LedgerMemory,
    /// How many times a lagging answer is read again before it counts
    /// (spec §3.5 check 2), and the pause between reads.
    pub(crate) lag_reads: u32,
    pub(crate) lag_pause: Duration,
}

impl<'a> GithubLedger<'a> {
    pub fn new(client: &'a Client, repo: Repo, local: &'a dyn LedgerMemory) -> Self {
        Self {
            client,
            repo,
            local,
            lag_reads: 3,
            lag_pause: Duration::from_millis(500),
        }
    }

    /// Tests only: how often a lagging answer is read again, and the pause
    /// between reads.
    #[doc(hidden)]
    pub fn with_lag(mut self, reads: u32, pause: Duration) -> Self {
        self.lag_reads = reads;
        self.lag_pause = pause;
        self
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }
}
