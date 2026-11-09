//! The compiler's output: the plan, its receipts and its typed refusals.

use super::facts::{ChangeField, Surface, Transport, Vendor, VendorFacts};
use super::policy::{AutoApproval, Capability, Effect, PolicyError, Posture};
use serde::{Deserialize, Serialize};

/// Where a native file belongs. Symbolic: no `~` is ever resolved by the compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigTarget {
    /// Claude Code project-local settings.
    ClaudeSettings,
    /// Codex `config.toml` fragment.
    CodexConfig,
    /// Codex execpolicy rules file.
    CodexRules,
}

/// What a [`ConfigTarget`] is relative to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigBase {
    /// The agent's workspace root.
    Workspace,
    /// The vendor's own home directory for this agent (an isolated home for a dedicated
    /// child). The caller decides which directory that is.
    VendorHome,
}

impl ConfigTarget {
    /// The base and the relative location. Never contains `~`.
    pub fn location(self) -> (ConfigBase, &'static str) {
        match self {
            ConfigTarget::ClaudeSettings => (ConfigBase::Workspace, ".claude/settings.local.json"),
            ConfigTarget::CodexConfig => (ConfigBase::VendorHome, "config.toml"),
            ConfigTarget::CodexRules => (ConfigBase::VendorHome, "rules/ferroloop.rules"),
        }
    }
}

/// A capability the vendor cannot enforce that the plan was emitted without (an acknowledged
/// unenforced default or deny, or an ungated ask default). Whoever applies the plan must surface it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UnenforcedItem {
    pub capability: Capability,
    pub vendor: Vendor,
    /// `deny`: a default or requested deny the vendor cannot enforce (refused unless
    /// acknowledged). `ask`: a canonical ask default the vendor does not gate; never refuses,
    /// listed so downstream can surface it.
    #[serde(default = "deny_kind")]
    pub kind: Effect,
}

fn deny_kind() -> Effect {
    Effect::Deny
}

/// A native file's syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeFormat {
    Json,
    Toml,
    Starlark,
    Text,
}

/// One native file: data only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeFile {
    pub target: ConfigTarget,
    pub format: NativeFormat,
    pub content: String,
}

/// How the native form relates to the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptClass {
    /// The native form means what was requested.
    Exact,
    /// The native form is stricter than requested (or the request was not granted).
    StricterDowngrade,
    /// A tool-scoped rule: it does not bind other access paths (shell, MCP, plugins).
    ToolScopedEmulation,
    /// The vendor has no native form; nothing was emitted for it.
    Unsupported,
}

/// What stands behind a claim about a native surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceClass {
    /// Vendor help text or documentation.
    Documented,
    /// An exported vendor schema; presence is not runtime proof.
    SchemaDefined,
    /// A conformance test ran it (asserted by the caller's facts).
    RuntimeProven,
    /// No evidence the vendor can do it.
    Unsupported,
}

/// What a receipt line is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptSubject {
    Posture,
    Sandbox,
    Capability(Capability),
}

/// Where a receipt line's request came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// A rule of the policy.
    Rule,
    /// The canonical default of an omitted capability.
    Default,
    /// The posture or sandbox axis.
    Axis,
}

/// One line of the compile receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptLine {
    pub subject: ReceiptSubject,
    pub origin: Origin,
    /// The effect asked for, when the line is about an effect.
    pub effect: Option<Effect>,
    pub requested: String,
    pub native: Vec<String>,
    pub class: ReceiptClass,
    pub evidence: EvidenceClass,
    pub note: String,
}

/// How quickly an edit to a field takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeClass {
    /// Applied at once; proven by facts only.
    Hot,
    /// Applies to the running session; proven by facts only.
    Session,
    /// A file write that takes effect at the next start. Proves only submitted state.
    PendingNextStart,
    /// The process must be replaced.
    Respawn,
}

/// The change class of one field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeClassEntry {
    pub field: ChangeField,
    pub class: ChangeClass,
}

/// The (vendor, transport, version) a plan was compiled for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeKey {
    pub vendor: Vendor,
    pub transport: Transport,
    pub installed_version: Option<String>,
}

/// The compiled, vendor-native form of a policy. Data only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledPlan {
    pub vendor: Vendor,
    pub key: ChangeKey,
    pub launch_args: Vec<String>,
    pub config_files: Vec<NativeFile>,
    /// Non-secret toggles only. The compiler currently emits none.
    pub env: Vec<(String, String)>,
    pub receipts: Vec<ReceiptLine>,
    pub change_classes: Vec<ChangeClassEntry>,
    /// True when a field this plan actually sets has the `respawn` class.
    pub requires_respawn: bool,
    /// Denies (defaults or requested) the vendor cannot enforce, present only under
    /// `unenforced_defaults: acknowledge`, and ask defaults the vendor does not gate (always).
    #[serde(default)]
    pub unenforced: Vec<UnenforcedItem>,
}

/// Why a policy was not compiled. A refusal is the fail-closed answer: nothing is emitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum CompileRefusal {
    #[error("the policy is invalid: {0}")]
    InvalidPolicy(PolicyError),
    #[error("the facts are invalid: {0}")]
    InvalidFacts(String),
    #[error("{vendor:?} is not compiled for the {transport:?} transport")]
    UnsupportedTransport {
        vendor: Vendor,
        transport: Transport,
    },
    #[error("{vendor:?} cannot enforce `{rule}`: {reason}")]
    UnenforceableDeny {
        vendor: Vendor,
        rule: String,
        reason: String,
    },
    #[error("{vendor:?} cannot hold `{rule}` without it becoming an allow: {reason}")]
    UnenforceableAsk {
        vendor: Vendor,
        rule: String,
        reason: String,
    },
    #[error("{vendor:?} cannot enforce posture {posture:?}: {reason}")]
    PostureNotEnforceable {
        vendor: Vendor,
        posture: Posture,
        reason: String,
    },
    #[error(
        "{vendor:?} does not support auto_approval `{auto_approval}` (supported: {})",
        supported.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(", ")
    )]
    AutoApprovalNotSupported {
        vendor: Vendor,
        auto_approval: AutoApproval,
        supported: Vec<AutoApproval>,
    },
    #[error("{vendor:?} cannot combine the requested posture and sandbox: {reason}")]
    SandboxConflict { vendor: Vendor, reason: String },
    #[error("{vendor:?}: the policy requires filesystem enforcement, but `{rule}` is {class:?}")]
    FsEnforcementRequired {
        vendor: Vendor,
        rule: String,
        class: ReceiptClass,
    },
    #[error(
        "{vendor:?} cannot enforce `{request}` ({capability:?}) and the policy does not acknowledge unenforced defaults"
    )]
    UnenforcedRequest {
        vendor: Vendor,
        capability: Capability,
        request: String,
    },
    #[error("internal safety audit failed: {0}")]
    InternalInvariant(String),
}

/// The evidence class of a vendor surface. Each row cites the spec section
/// (`docs/superpowers/specs/2026-11-03-universal-permissions-design.md`) it comes from.
/// Nothing here is `RuntimeProven`: the caller upgrades a surface through
/// `ProvenFact::RuntimeProven`.
pub fn evidence_for(vendor: Vendor, surface: Surface, facts: &VendorFacts) -> EvidenceClass {
    use super::facts::ProvenFact;
    use EvidenceClass::*;
    use Surface::*;
    if facts.proves(&ProvenFact::RuntimeProven(surface)) {
        return RuntimeProven;
    }
    match (vendor, surface) {
        // §1 Claude launch flags and settings keys, §2 Claude bullets: help text and docs.
        (Vendor::Claude, _) => Documented,
        // §2 Codex: stable schema `SandboxPolicy.workspaceWrite.writableRoots`, `AppToolConfig`
        // and the exported network decision are schema-defined, not runtime-proven.
        (Vendor::Codex, ShellRule) => Documented, // §2: execpolicy `prefix_rule` (what the approval UI writes)
        (Vendor::Codex, ProtectedPath) => Unsupported, // §2: no protected-path concept
        (Vendor::Codex, _) => SchemaDefined,
        // §1/§2 Muse: launch flags from `--help`; profile content is asserted by the facts.
        (Vendor::Muse, ProtectedPath) => Unsupported,
        (Vendor::Muse, _) => Documented,
        // §2 Grok: Claude-compatible rule syntax and flags from help text.
        (Vendor::Grok, _) => Documented,
        // §2 Antigravity: `command(...)` allows and `allowNonWorkspaceAccess` seen in live
        // config; flags from help. No path, network, MCP or protected-path grammar.
        (Vendor::Antigravity, Posture | Sandbox | ShellRule) => Documented,
        (Vendor::Antigravity, FsRule) => Documented, // the single boolean only
        (Vendor::Antigravity, ProtectedPath | NetworkRule | McpRule) => Unsupported,
    }
}
