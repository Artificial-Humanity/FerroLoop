//! GitHub Issues as an fl tracker (GitHub tracker spec). The only crate
//! that talks to GitHub.

pub mod client;
pub mod creds;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod meta;
pub mod tracker;

pub use client::{Client, DEFAULT_API, Method, Reply};
pub use creds::{AppCredentials, Credentials, EnvToken};
pub use tracker::{GithubTracker, Notice, Repaired, Repo};
