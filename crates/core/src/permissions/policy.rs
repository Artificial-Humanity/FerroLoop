//! The canonical policy: posture, sandbox choice and typed capability rules.
//!
//! Every input type refuses unknown fields. Pattern grammars are per capability and strict;
//! [`PermissionPolicy::normalized`] validates, canonicalises, de-duplicates and orders the
//! rules, and is the only way a policy reaches the compiler.

use serde::{Deserialize, Serialize};

/// The only policy schema version this build reads and writes.
pub const SCHEMA_VERSION: u32 = 1;
/// Most rules one policy may hold.
pub const MAX_RULES: usize = 256;
/// Longest glob, in bytes.
pub const MAX_PATTERN_BYTES: usize = 512;
/// Most tokens in an argv prefix.
pub const MAX_ARGV_TOKENS: usize = 32;
/// Longest argv token, in bytes.
pub const MAX_TOKEN_BYTES: usize = 256;
/// Most `*` in one shell glob.
pub const MAX_SHELL_GLOB_STARS: usize = 8;
/// Most path segments in a glob.
pub const MAX_PATH_SEGMENTS: usize = 64;

/// How a vendor resolves "ask": a separate axis from rules and sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Posture {
    /// Prompt for what no rule decides.
    Ask,
    /// Edits are accepted without a prompt; everything else follows the rules.
    AcceptEdits,
    /// The vendor's automatic approval path.
    Auto,
    /// Read-only planning.
    Plan,
    /// Checks disabled. Never produced by rules.
    Bypass,
}

/// The sandbox axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxChoice {
    /// No OS sandbox under the rules.
    Off,
    /// Leave the vendor's sandbox as the vendor ships it.
    VendorDefault,
}

/// The eight capabilities a rule can address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    FsReadWorkspace,
    FsReadExternal,
    FsWriteWorkspace,
    FsWriteExternal,
    FsWriteProtected,
    ShellExec,
    NetworkWeb,
    McpToolCall,
}

impl Capability {
    /// Every capability, in wire order.
    pub const ALL: [Capability; 8] = [
        Capability::FsReadWorkspace,
        Capability::FsReadExternal,
        Capability::FsWriteWorkspace,
        Capability::FsWriteExternal,
        Capability::FsWriteProtected,
        Capability::ShellExec,
        Capability::NetworkWeb,
        Capability::McpToolCall,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Capability::FsReadWorkspace => "fs_read_workspace",
            Capability::FsReadExternal => "fs_read_external",
            Capability::FsWriteWorkspace => "fs_write_workspace",
            Capability::FsWriteExternal => "fs_write_external",
            Capability::FsWriteProtected => "fs_write_protected",
            Capability::ShellExec => "shell_exec",
            Capability::NetworkWeb => "network_web",
            Capability::McpToolCall => "mcp_tool_call",
        }
    }

    /// Whether the capability is a filesystem capability.
    pub fn is_fs(self) -> bool {
        matches!(
            self,
            Capability::FsReadWorkspace
                | Capability::FsReadExternal
                | Capability::FsWriteWorkspace
                | Capability::FsWriteExternal
                | Capability::FsWriteProtected
        )
    }
}

/// What a rule decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Deny,
    Ask,
}

impl Effect {
    /// Precedence rank: deny, then ask, then allow.
    pub fn rank(self) -> u8 {
        match self {
            Effect::Deny => 0,
            Effect::Ask => 1,
            Effect::Allow => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Allow => "allow",
            Effect::Deny => "deny",
            Effect::Ask => "ask",
        }
    }
}

/// The canonical effect of a capability no rule mentions. `Ask` means "defer to the
/// posture's approval path"; only protected paths default to `Deny`. Not applied under
/// [`Posture::Bypass`], which disables checks.
pub fn canonical_default(capability: Capability) -> Effect {
    match capability {
        Capability::FsWriteProtected => Effect::Deny,
        _ => Effect::Ask,
    }
}

/// The three protected path classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathClass {
    /// The vendor's settings files.
    Settings,
    /// Version-control metadata.
    Git,
    /// The vendor's tool configuration directory.
    ToolConfig,
}

impl PathClass {
    pub const ALL: [PathClass; 3] = [PathClass::Settings, PathClass::Git, PathClass::ToolConfig];

    pub fn as_str(self) -> &'static str {
        match self {
            PathClass::Settings => "settings",
            PathClass::Git => "git",
            PathClass::ToolConfig => "tool-config",
        }
    }
}

/// A typed pattern. Which kind a capability takes is fixed; see [`Rule`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Pattern {
    /// `fs_read_workspace`, `fs_write_workspace`: a glob relative to the workspace.
    PathGlob { glob: String },
    /// `fs_read_external`, `fs_write_external`: an absolute path glob.
    AbsPathGlob { glob: String },
    /// `fs_write_protected`.
    PathClass { class: PathClass },
    /// `shell_exec`: the command starts with these argv tokens, compared exactly.
    ArgvPrefix { argv: Vec<String> },
    /// `shell_exec`: a glob over the command string; `*` is the only wildcard.
    ShellGlob { glob: String },
    /// `network_web`: a host, or `*.host` for its subdomains.
    Domain { domain: String },
    /// `mcp_tool_call`: a server, or one tool of it.
    Mcp {
        server: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
    },
}

impl Pattern {
    /// A one-line canonical rendering, for receipts and errors.
    pub fn describe(&self) -> String {
        match self {
            Pattern::PathGlob { glob } => format!("path:{glob}"),
            Pattern::AbsPathGlob { glob } => format!("abs:{glob}"),
            Pattern::PathClass { class } => format!("class:{}", class.as_str()),
            Pattern::ArgvPrefix { argv } => format!("argv:{}", argv.join(" ")),
            Pattern::ShellGlob { glob } => format!("shell-glob:{glob}"),
            Pattern::Domain { domain } => format!("domain:{domain}"),
            Pattern::Mcp { server, tool: None } => format!("mcp:{server}"),
            Pattern::Mcp {
                server,
                tool: Some(t),
            } => format!("mcp:{server}/{t}"),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Pattern::PathGlob { .. } => "path_glob",
            Pattern::AbsPathGlob { .. } => "abs_path_glob",
            Pattern::PathClass { .. } => "path_class",
            Pattern::ArgvPrefix { .. } => "argv_prefix",
            Pattern::ShellGlob { .. } => "shell_glob",
            Pattern::Domain { .. } => "domain",
            Pattern::Mcp { .. } => "mcp",
        }
    }
}

/// One rule. `pattern: None` addresses the whole capability (for the external capabilities,
/// every path outside the workspace; for `network_web`, every domain).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub capability: Capability,
    pub effect: Effect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<Pattern>,
}

impl Rule {
    pub fn new(capability: Capability, effect: Effect, pattern: Option<Pattern>) -> Self {
        Self {
            capability,
            effect,
            pattern,
        }
    }

    /// `effect capability pattern`, for receipts.
    pub fn describe(&self) -> String {
        match &self.pattern {
            Some(p) => format!(
                "{} {} {}",
                self.effect.as_str(),
                self.capability.as_str(),
                p.describe()
            ),
            None => format!("{} {}", self.effect.as_str(), self.capability.as_str()),
        }
    }
}

/// What a compile does with a canonical default or a requested deny the vendor cannot enforce.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum UnenforcedDefaults {
    /// Refuse the plan, naming the capability and vendor.
    #[default]
    Refuse,
    /// Emit the plan with its `unsupported` receipt and list the capability in
    /// `CompiledPlan::unenforced`. Whoever applies the plan must surface that list; the Owner's
    /// explicit acknowledgement is collected outside the compiler.
    Acknowledge,
}

/// The canonical policy of one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionPolicy {
    pub schema_version: u32,
    pub posture: Posture,
    pub sandbox: SandboxChoice,
    /// Refuse the plan unless every filesystem restriction is enforced by an OS or runtime
    /// boundary (or the facts prove coverage), not by a tool-scoped rule.
    #[serde(default)]
    pub require_fs_enforcement: bool,
    /// Defaults to `refuse`: a canonical default deny the vendor cannot enforce is not
    /// silently left open.
    #[serde(default)]
    pub unenforced_defaults: UnenforcedDefaults,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

/// Why a policy was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum PolicyError {
    #[error("schema_version {0} is not supported (only 1)")]
    UnsupportedSchemaVersion(u32),
    #[error("{0} rules exceed the limit of {MAX_RULES}")]
    TooManyRules(usize),
    #[error("{capability:?} does not take a `{kind}` pattern")]
    PatternKindMismatch {
        capability: Capability,
        kind: String,
    },
    #[error("invalid pattern for {capability:?}: {reason}")]
    InvalidPattern {
        capability: Capability,
        reason: String,
    },
    #[error("{capability:?} has conflicting effects for the same pattern `{pattern}`")]
    ConflictingRules {
        capability: Capability,
        pattern: String,
    },
    #[error("bypass disables every check, so the {effect:?} rule on {capability:?} cannot hold")]
    BypassWithRestrictions {
        capability: Capability,
        effect: Effect,
    },
    #[error("posture {posture:?} disables the prompt the ask rule on {capability:?} needs")]
    PostureDisablesChecks {
        posture: Posture,
        capability: Capability,
    },
    #[error("require_fs_enforcement cannot hold with {0}")]
    EnforcementWithoutBoundary(String),
}

fn ctl(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

fn validate_segments(glob: &str, absolute: bool) -> Result<(), String> {
    if glob.is_empty() {
        return Err("empty glob".into());
    }
    if glob.len() > MAX_PATTERN_BYTES {
        return Err("glob is too long".into());
    }
    if ctl(glob) {
        return Err("glob has a control character".into());
    }
    if glob.contains('\\') {
        return Err("backslash is not allowed".into());
    }
    if glob.trim() != glob {
        return Err("glob has leading or trailing whitespace".into());
    }
    if glob
        .chars()
        .any(|c| matches!(c, '[' | ']' | '{' | '}' | '(' | ')'))
    {
        return Err("`[ ] { } ( )` are ambiguous across vendors".into());
    }
    if glob.starts_with('!') || glob.starts_with('#') {
        return Err("a leading `!` or `#` is ambiguous across vendors".into());
    }
    if glob.starts_with('~') {
        return Err("`~` is not resolved".into());
    }
    let body = if absolute {
        glob.strip_prefix('/')
            .ok_or_else(|| "an external path glob must be absolute".to_owned())?
    } else {
        if glob.starts_with('/') {
            return Err("a workspace glob is relative to the workspace".into());
        }
        if glob.len() >= 2 && glob.as_bytes()[1] == b':' && glob.as_bytes()[0].is_ascii_alphabetic()
        {
            return Err("a drive prefix is not a workspace-relative path".into());
        }
        glob
    };
    if body.is_empty() {
        return Err("use `/**` for every external path".into());
    }
    let segments: Vec<&str> = body.split('/').collect();
    if segments.len() > MAX_PATH_SEGMENTS {
        return Err("too many path segments".into());
    }
    for seg in segments {
        if seg.contains('?') {
            return Err("`?` is a wildcard in some vendors and a literal in others; it is not allowed in a path glob".into());
        }
        match seg {
            "" => return Err("empty path segment (`//` or trailing `/`)".into()),
            "." => return Err("`.` segment".into()),
            ".." => return Err("`..` escapes are not allowed".into()),
            _ => {}
        }
        if seg.contains('*') && seg != "*" && seg != "**" {
            // `*` inside a name (`a*b`, `*.rs`) is a plain wildcard; `**` must be a whole segment.
            if seg.contains("**") {
                return Err("`**` must be a whole path segment".into());
            }
        }
    }
    Ok(())
}

fn validate_domain(d: &str) -> Result<String, String> {
    if d.is_empty() || d.len() > 253 {
        return Err("domain length".into());
    }
    if !d.is_ascii() {
        return Err("domains must be ASCII (use punycode)".into());
    }
    let lower = d.to_ascii_lowercase();
    let host = lower.strip_prefix("*.").unwrap_or(&lower);
    for label in host.split('.') {
        let ok = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !ok {
            return Err(format!("`{label}` is not a domain label"));
        }
    }
    Ok(lower)
}

fn name_ok(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Validates a pattern for its capability and returns its canonical form.
fn canonical(
    capability: Capability,
    pattern: &Option<Pattern>,
) -> Result<Option<Pattern>, PolicyError> {
    let Some(p) = pattern else {
        return Ok(None);
    };
    let bad = |reason: String| PolicyError::InvalidPattern { capability, reason };
    let mismatch = || PolicyError::PatternKindMismatch {
        capability,
        kind: p.kind().to_owned(),
    };
    match (capability, p) {
        (
            Capability::FsReadWorkspace | Capability::FsWriteWorkspace,
            Pattern::PathGlob { glob },
        ) => {
            validate_segments(glob, false).map_err(bad)?;
            Ok(if glob == "**" { None } else { Some(p.clone()) })
        }
        (
            Capability::FsReadExternal | Capability::FsWriteExternal,
            Pattern::AbsPathGlob { glob },
        ) => {
            validate_segments(glob, true).map_err(bad)?;
            Ok(if glob == "/**" { None } else { Some(p.clone()) })
        }
        (Capability::FsWriteProtected, Pattern::PathClass { .. }) => Ok(Some(p.clone())),
        (Capability::ShellExec, Pattern::ArgvPrefix { argv }) => {
            if argv.is_empty() || argv.len() > MAX_ARGV_TOKENS {
                return Err(bad("argv must hold 1 to 32 tokens".into()));
            }
            for t in argv {
                if t.is_empty() || t.len() > MAX_TOKEN_BYTES || ctl(t) {
                    return Err(bad(
                        "an argv token is empty, too long or has a control character".into(),
                    ));
                }
            }
            if argv[0].starts_with('-') {
                return Err(bad("the command name must not start with `-`".into()));
            }
            Ok(Some(p.clone()))
        }
        (Capability::ShellExec, Pattern::ShellGlob { glob }) => {
            if glob.is_empty() || glob.len() > MAX_PATTERN_BYTES || ctl(glob) {
                return Err(bad(
                    "shell glob is empty, too long or has a control character".into(),
                ));
            }
            if glob.trim() != glob {
                return Err(bad("shell glob has leading or trailing whitespace".into()));
            }
            if glob
                .chars()
                .any(|c| matches!(c, '\\' | '(' | ')' | '?' | '[' | ']' | '{' | '}'))
            {
                return Err(bad(
                    "`\\ ( ) ? [ ] { }` are ambiguous in a shell glob".into()
                ));
            }
            if glob.contains("**") {
                return Err(bad("`**` is ambiguous in a shell glob".into()));
            }
            if glob.ends_with(":*") {
                return Err(bad("a trailing `:*` is the legacy prefix form in Claude and matches more than the glob".into()));
            }
            if glob.matches('*').count() > MAX_SHELL_GLOB_STARS {
                return Err(bad(format!(
                    "more than {MAX_SHELL_GLOB_STARS} `*` in a shell glob"
                )));
            }
            Ok(if glob == "*" { None } else { Some(p.clone()) })
        }
        (Capability::NetworkWeb, Pattern::Domain { domain }) => {
            if domain == "*" {
                return Ok(None);
            }
            let d = validate_domain(domain).map_err(bad)?;
            Ok(Some(Pattern::Domain { domain: d }))
        }
        (Capability::McpToolCall, Pattern::Mcp { server, tool }) => {
            if !name_ok(server, 64)
                || server.contains("__")
                || server.starts_with('_')
                || server.ends_with('_')
                || server.starts_with('-')
            {
                return Err(bad(
                    "server must be 1-64 of [A-Za-z0-9_-], without `__`, edge `_` or leading `-`"
                        .into(),
                ));
            }
            if let Some(t) = tool
                && !name_ok(t, 128)
            {
                return Err(bad("tool must be 1-128 of [A-Za-z0-9_-]".into()));
            }
            Ok(Some(p.clone()))
        }
        _ => Err(mismatch()),
    }
}

impl PermissionPolicy {
    /// A policy with no rules.
    pub fn new(posture: Posture, sandbox: SandboxChoice) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            posture,
            sandbox,
            require_fs_enforcement: false,
            unenforced_defaults: UnenforcedDefaults::Refuse,
            rules: Vec::new(),
        }
    }

    /// Adds a rule.
    pub fn rule(
        mut self,
        capability: Capability,
        effect: Effect,
        pattern: Option<Pattern>,
    ) -> Self {
        self.rules.push(Rule::new(capability, effect, pattern));
        self
    }

    /// Validates, canonicalises, de-duplicates and orders the policy (capability, then deny
    /// before ask before allow, then pattern). The result is what the compiler reads.
    pub fn normalized(&self) -> Result<PermissionPolicy, PolicyError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(PolicyError::UnsupportedSchemaVersion(self.schema_version));
        }
        if self.rules.len() > MAX_RULES {
            return Err(PolicyError::TooManyRules(self.rules.len()));
        }
        let mut rules = Vec::with_capacity(self.rules.len());
        for r in &self.rules {
            rules.push(Rule {
                capability: r.capability,
                effect: r.effect,
                pattern: canonical(r.capability, &r.pattern)?,
            });
        }
        rules.sort_by(|a, b| {
            (a.capability, a.effect.rank(), &a.pattern).cmp(&(
                b.capability,
                b.effect.rank(),
                &b.pattern,
            ))
        });
        rules.dedup();
        for (i, a) in rules.iter().enumerate() {
            for b in &rules[i + 1..] {
                if a.capability == b.capability && a.pattern == b.pattern && a.effect != b.effect {
                    return Err(PolicyError::ConflictingRules {
                        capability: a.capability,
                        pattern: a
                            .pattern
                            .as_ref()
                            .map_or_else(|| "*".to_owned(), Pattern::describe),
                    });
                }
            }
        }
        match self.posture {
            Posture::Bypass => {
                if let Some(r) = rules.iter().find(|r| r.effect != Effect::Allow) {
                    return Err(PolicyError::BypassWithRestrictions {
                        capability: r.capability,
                        effect: r.effect,
                    });
                }
                if self.require_fs_enforcement {
                    return Err(PolicyError::EnforcementWithoutBoundary(
                        "a bypass posture".into(),
                    ));
                }
            }
            Posture::Auto => {
                if let Some(r) = rules.iter().find(|r| r.effect == Effect::Ask) {
                    return Err(PolicyError::PostureDisablesChecks {
                        posture: Posture::Auto,
                        capability: r.capability,
                    });
                }
            }
            Posture::AcceptEdits => {
                if let Some(r) = rules.iter().find(|r| {
                    r.effect == Effect::Ask
                        && matches!(
                            r.capability,
                            Capability::FsWriteWorkspace
                                | Capability::FsWriteExternal
                                | Capability::FsWriteProtected
                        )
                }) {
                    return Err(PolicyError::PostureDisablesChecks {
                        posture: Posture::AcceptEdits,
                        capability: r.capability,
                    });
                }
            }
            Posture::Ask | Posture::Plan => {}
        }
        if self.require_fs_enforcement && self.sandbox == SandboxChoice::Off {
            return Err(PolicyError::EnforcementWithoutBoundary(
                "sandbox off".into(),
            ));
        }
        Ok(PermissionPolicy {
            schema_version: SCHEMA_VERSION,
            posture: self.posture,
            sandbox: self.sandbox,
            require_fs_enforcement: self.require_fs_enforcement,
            unenforced_defaults: self.unenforced_defaults,
            rules,
        })
    }

    /// The normalised rules of one capability, deny first.
    pub fn rules_for(&self, capability: Capability) -> impl Iterator<Item = &Rule> {
        self.rules
            .iter()
            .filter(move |r| r.capability == capability)
    }
}

/// The four presets of the spin-up form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// Plan posture. Workspace reads allowed; workspace writes and the web denied. Shell,
    /// MCP and external access keep the canonical ask default.
    ReadOnly,
    /// Ask posture. Workspace reads and writes allowed; the web denied.
    Workspace,
    /// Ask posture. Workspace reads and writes and the web allowed.
    WorkspaceWeb,
    /// Ask posture and no rules: every capability at its canonical default, ready to edit.
    Custom,
}

impl Preset {
    pub const ALL: [Preset; 4] = [
        Preset::ReadOnly,
        Preset::Workspace,
        Preset::WorkspaceWeb,
        Preset::Custom,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Preset::ReadOnly => "read-only",
            Preset::Workspace => "workspace",
            Preset::WorkspaceWeb => "workspace-web",
            Preset::Custom => "custom",
        }
    }

    /// The preset's policy, sandbox at the vendor default.
    pub fn policy(self) -> PermissionPolicy {
        use Capability::*;
        use Effect::*;
        let base = |p| PermissionPolicy::new(p, SandboxChoice::VendorDefault);
        match self {
            Preset::ReadOnly => base(Posture::Plan)
                .rule(FsReadWorkspace, Allow, None)
                .rule(FsWriteWorkspace, Deny, None)
                .rule(NetworkWeb, Deny, None),
            Preset::Workspace => base(Posture::Ask)
                .rule(FsReadWorkspace, Allow, None)
                .rule(FsWriteWorkspace, Allow, None)
                .rule(NetworkWeb, Deny, None),
            Preset::WorkspaceWeb => base(Posture::Ask)
                .rule(FsReadWorkspace, Allow, None)
                .rule(FsWriteWorkspace, Allow, None)
                .rule(NetworkWeb, Allow, None),
            Preset::Custom => base(Posture::Ask),
        }
    }
}
