//! The GitHub ledger (GitHub ledger spec §3): the `fl/ledger` branch of
//! the repository that backs the tracker, where mode B publishes each
//! decision and the evidence it rests on.

pub mod disclose;
pub mod layout;

pub use disclose::Visibility;
