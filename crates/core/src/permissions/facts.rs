//! The caller-supplied facts a compile depends on. The compiler never probes a vendor, a
//! filesystem or a version: everything it may rely on beyond the policy is in [`VendorFacts`].

use super::policy::Capability;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The five vendor runtimes the compiler targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vendor {
    Claude,
    Codex,
    Muse,
    Grok,
    Antigravity,
}

impl Vendor {
    /// Every vendor, in a fixed order.
    pub const ALL: [Vendor; 5] = [
        Vendor::Claude,
        Vendor::Codex,
        Vendor::Muse,
        Vendor::Grok,
        Vendor::Antigravity,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Vendor::Claude => "claude",
            Vendor::Codex => "codex",
            Vendor::Muse => "muse",
            Vendor::Grok => "grok",
            Vendor::Antigravity => "antigravity",
        }
    }

    /// The transports the compiler knows how to compile for.
    pub fn supports(self, transport: Transport) -> bool {
        match self {
            Vendor::Claude => matches!(transport, Transport::Pty | Transport::Print),
            Vendor::Codex => matches!(
                transport,
                Transport::Pty | Transport::Print | Transport::AppServer
            ),
            Vendor::Muse => matches!(transport, Transport::Pty | Transport::Msp),
            Vendor::Grok | Vendor::Antigravity => matches!(transport, Transport::Pty),
        }
    }
}

/// How fl reaches the vendor process. Part of the change-class key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Pty,
    Print,
    AppServer,
    Msp,
}

/// A class of native surface; the unit evidence is recorded against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Posture,
    Sandbox,
    FsRule,
    ProtectedPath,
    ShellRule,
    NetworkRule,
    McpRule,
}

/// A field of the compiled plan whose change class is keyed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeField {
    Posture,
    Sandbox,
    Rules(Capability),
}

/// One thing the caller has proven for this (vendor, transport, version).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenFact {
    /// An interactive approval channel works on this transport: an `ask` is answered by a
    /// person. Without it a dropped allow is reported as denied, never as asked.
    InteractiveAsk,
    /// An OS or runtime boundary covers every enabled filesystem access path.
    FsEnforcementCoverage,
    /// A native restriction hard-denies this capability (a deny is enforced, not prompted).
    HardDeny(Capability),
    /// Editing this field is applied by a supported channel with effective read-back.
    HotEdit(ChangeField),
    /// Conformance tests have run this surface.
    RuntimeProven(Surface),
    /// Codex: an execpolicy `allow` rule keeps the command inside the sandbox (it does not
    /// bypass it). Without this, an allow is compiled as `prompt`.
    ExecpolicyAllowKeepsSandbox,
    /// Muse: the verified profiles confine access to the workspace, so a deny or ask on an
    /// external path is enforced by the selected profile.
    ProfileConfinesExternal,
}

/// The most authority a Muse permission profile grants. Asserted by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileCeiling {
    /// Workspace reads only: no workspace writes.
    ReadOnly,
    /// Workspace reads and writes. Shell follows the approval mode and network follows the
    /// sandbox network flag; a profile does not grant either.
    Workspace,
}

/// A Muse permission profile the caller verified exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MuseProfileFact {
    pub id: String,
    pub ceiling: ProfileCeiling,
}

/// What the caller knows about the vendor install the plan is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorFacts {
    pub transport: Transport,
    #[serde(default)]
    pub installed_version: Option<String>,
    #[serde(default)]
    pub capabilities_proven: BTreeSet<ProvenFact>,
    /// Antigravity only: exact commands that may be pre-seeded as `command(...)` allows.
    #[serde(default)]
    pub exact_commands: Vec<String>,
    /// Muse only: verified permission profiles.
    #[serde(default)]
    pub muse_profiles: Vec<MuseProfileFact>,
    /// Grok only: a verified `--sandbox` profile id used for `SandboxChoice::VendorDefault`.
    #[serde(default)]
    pub sandbox_profile: Option<String>,
}

pub(crate) const MAX_EXACT_COMMANDS: usize = 1024;
pub(crate) const MAX_EXACT_COMMAND_BYTES: usize = 512;

fn id_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl VendorFacts {
    /// Facts proving nothing, for a transport.
    pub fn new(transport: Transport) -> Self {
        Self {
            transport,
            installed_version: None,
            capabilities_proven: BTreeSet::new(),
            exact_commands: Vec::new(),
            muse_profiles: Vec::new(),
            sandbox_profile: None,
        }
    }

    /// Adds one proven fact.
    pub fn with(mut self, fact: ProvenFact) -> Self {
        self.capabilities_proven.insert(fact);
        self
    }

    pub fn proves(&self, fact: &ProvenFact) -> bool {
        self.capabilities_proven.contains(fact)
    }

    /// A shape check; the first problem found.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(v) = &self.installed_version
            && !(!v.is_empty()
                && v.len() <= 64
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_')))
        {
            return Err(format!("installed_version `{v}` is not a version string"));
        }
        if self.exact_commands.len() > MAX_EXACT_COMMANDS {
            return Err("too many exact_commands".to_owned());
        }
        for c in &self.exact_commands {
            if c.is_empty() || c.len() > MAX_EXACT_COMMAND_BYTES || c.chars().any(char::is_control)
            {
                return Err("an exact command is empty, too long or has control characters".into());
            }
        }
        for p in &self.muse_profiles {
            if !id_ok(&p.id) {
                return Err(format!("muse profile id `{}` is not an id", p.id));
            }
        }
        if let Some(p) = &self.sandbox_profile
            && !id_ok(p)
        {
            return Err(format!("sandbox_profile `{p}` is not an id"));
        }
        Ok(())
    }
}
