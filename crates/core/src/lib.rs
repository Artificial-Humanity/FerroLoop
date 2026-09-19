//! Pure domain model. No IO, no async, no clock, no network.

pub mod verdict;
pub use verdict::{FailReason, Population, Verdict};
