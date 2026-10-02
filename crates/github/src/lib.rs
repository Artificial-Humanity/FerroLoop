//! GitHub Issues as an fl tracker (GitHub tracker spec). The only crate
//! that talks to GitHub.

pub mod client;
pub mod creds;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
#[cfg(any(test, feature = "fake"))]
pub mod fake_git;
pub mod ledger;
pub mod meta;
pub mod owner;
pub mod tracker;

pub use client::{Client, DEFAULT_API, GraphqlAnswer, Method, Reply};
pub use creds::{AppCredentials, Credentials, EnvToken};
pub use ledger::GithubLedger;
pub use tracker::{GithubTracker, Notice, Repaired, Repo};
