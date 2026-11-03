//! Universal cross-vendor permissions: one canonical [`PermissionPolicy`] per agent and a
//! pure, deterministic, fail-closed compiler into Claude Code, Codex, Muse, Grok and
//! Antigravity native forms.
//!
//! Design: `docs/superpowers/specs/2026-11-03-universal-permissions-design.md` (§0 to §2 are
//! what this module implements; §3 live interception and §4 UI are not here). User-facing
//! description: `docs/permissions.md`.
//!
//! This module is pure: no I/O, clock, environment, randomness or `unsafe`. A compile reads
//! only its three arguments, and the same arguments give the same bytes.

mod compile;
mod facts;
mod plan;
mod policy;

#[cfg(test)]
mod tests;

pub use compile::compile;
pub use facts::{
    ChangeField, MuseProfileFact, ProfileCeiling, ProvenFact, Surface, Transport, Vendor,
    VendorFacts,
};
pub use plan::{
    ChangeClass, ChangeClassEntry, ChangeKey, CompileRefusal, CompiledPlan, ConfigBase,
    ConfigTarget, EvidenceClass, NativeFile, NativeFormat, Origin, ReceiptClass, ReceiptLine,
    ReceiptSubject, evidence_for,
};
pub use policy::{
    Capability, Effect, MAX_ARGV_TOKENS, MAX_PATH_SEGMENTS, MAX_PATTERN_BYTES, MAX_RULES,
    MAX_TOKEN_BYTES, PathClass, Pattern, PermissionPolicy, PolicyError, Posture, Preset, Rule,
    SCHEMA_VERSION, SandboxChoice, canonical_default,
};
