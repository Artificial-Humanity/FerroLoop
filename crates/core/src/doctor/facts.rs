//! The facts a caller hands the engine. Plain data: nothing here reads the file system.

use serde_json::{Map, Value};

/// What was found where the install receipt belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptSource {
    /// No file: the install is unmanaged.
    Absent,
    /// The path could not be worked out (no `$HOME`).
    Unlocatable(String),
    /// A file that could not be read (not a regular file, too large, I/O error).
    Unreadable(String),
    /// The file's text.
    Text(String),
}

/// What a store verification found. Produced by a read-only open that never migrates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreOutcome {
    /// There is no store at the configured path.
    Absent,
    /// Opened read-only and verified.
    Verified {
        /// `fls` or `redb`.
        engine: String,
        /// The schema format recorded (`None`: a file from before versioning).
        format: Option<u64>,
        /// The newest format this build reads.
        newest: u64,
        /// A `.redb` sits beside the `.fls`: the backup a migration left, not current data.
        backup_beside: bool,
    },
    /// A lone `.redb` that a normal open would migrate to `.fls`.
    WouldMigrate { format: Option<u64> },
    /// Held by a writer; the wait was bounded.
    Locked,
    /// A format or container this build does not read (a newer fl wrote it, or it is off the
    /// supported ladder).
    Unsupported { reason: String },
    /// Corrupt, not initialised, needs repair, or unreadable.
    Invalid { reason: String },
}

/// One configured store and what was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreFact {
    pub path: String,
    pub outcome: StoreOutcome,
}

/// Every store the caller checked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreFacts {
    pub stores: Vec<StoreFact>,
    /// Why the list may be incomplete (an unreadable config, a capped list). Any note keeps
    /// the check from passing.
    pub notes: Vec<String>,
}

/// An `lstat` of one path: no content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeMeta {
    Absent,
    Regular { mode: u32, uid: u32 },
    Dir { mode: u32, uid: u32 },
    Symlink,
    Other,
    Unreadable(String),
}

/// A vault file (or the master key file), its `lstat` and its parent directory's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultNode {
    /// `vault` or `master key`.
    pub role: String,
    pub path: String,
    pub node: NodeMeta,
    pub parent: NodeMeta,
}

/// The vault's files. Only `lstat` data: the files' contents are never read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultFacts {
    /// Whether this platform reports a file's mode and owner.
    pub supported: bool,
    /// The running user.
    pub euid: Option<u32>,
    /// `FL_VAULT_MASTER_KEY` is set (whether it is, never its value).
    pub key_in_env: bool,
    pub files: Vec<VaultNode>,
}

impl Default for VaultFacts {
    fn default() -> Self {
        Self {
            supported: true,
            euid: None,
            key_in_env: false,
            files: Vec::new(),
        }
    }
}

/// The UI bundle the binary serves or would serve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiFacts {
    /// Where the bundle is, as displayed; `None`: no bundle is configured.
    pub dir: Option<String>,
    pub dir_exists: bool,
    /// The validated `build-info.json` (see `ui_bundle::read_ui_build_info`).
    pub info: Option<Map<String, Value>>,
}
