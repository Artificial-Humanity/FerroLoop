//! `G7.vault-perms`: the vault and its master key are private to the owner. Only `lstat`
//! data reaches the engine; no file's contents are ever read or printed.

use super::{CheckStatus, DoctorCheck, DoctorInputs, NodeMeta, ids, joined, make};

const TITLE: &str = "Vault permissions";
const EXPECTED_FILE_MODE: u32 = 0o600;

pub(super) fn check(inputs: &DoctorInputs<'_>) -> DoctorCheck {
    let v = &inputs.vault;
    let present: Vec<_> = v
        .files
        .iter()
        .filter(|f| f.node != NodeMeta::Absent)
        .collect();
    let (status, detail) = if !v.supported {
        (
            CheckStatus::Unsupported,
            "this platform does not report file modes and owners".to_string(),
        )
    } else if present.is_empty() {
        let mut d = String::from(
            "no vault or master key file exists (created only by an explicit vault action)",
        );
        if v.key_in_env {
            d.push_str("; a master key is set in the environment");
        }
        (CheckStatus::NotInstalled, d)
    } else {
        let mut fails: Vec<String> = Vec::new();
        let mut warns: Vec<String> = Vec::new();
        let mut oks: Vec<String> = Vec::new();
        for f in &present {
            let label = format!("{} {}", f.role, f.path);
            match &f.node {
                NodeMeta::Regular { mode, uid } => {
                    let perm = mode & 0o777;
                    if perm & 0o077 != 0 {
                        let what = if perm & 0o002 != 0 {
                            "world-writable"
                        } else if perm & 0o004 != 0 {
                            "world-readable"
                        } else {
                            "accessible to its group or others"
                        };
                        fails.push(format!(
                            "{label} is {what} (mode {perm:04o}, expected {EXPECTED_FILE_MODE:04o})"
                        ));
                    } else if perm != EXPECTED_FILE_MODE {
                        warns.push(format!(
                            "{label} has mode {perm:04o}, expected {EXPECTED_FILE_MODE:04o}"
                        ));
                    } else {
                        oks.push(format!("{} mode {perm:04o}", f.role));
                    }
                    match v.euid {
                        Some(me) if me != *uid => fails.push(format!(
                            "{label} is owned by uid {uid}, not the running user (uid {me})"
                        )),
                        None => warns.push(format!(
                            "{label}: the running user is unknown, so its owner was not compared"
                        )),
                        _ => {}
                    }
                }
                NodeMeta::Symlink => {
                    fails.push(format!("{label} is a symlink, which FerroGate refuses"))
                }
                NodeMeta::Dir { .. } | NodeMeta::Other => {
                    fails.push(format!("{label} is not a regular file"))
                }
                NodeMeta::Unreadable(why) => {
                    fails.push(format!("{label} cannot be examined ({why})"))
                }
                NodeMeta::Absent => {}
            }
            if let NodeMeta::Dir { mode, uid } = &f.parent {
                let perm = mode & 0o777;
                if perm & 0o022 != 0 {
                    warns.push(format!(
                        "the directory of the {} is writable by its group or others (mode {perm:04o})",
                        f.role
                    ));
                }
                if let Some(me) = v.euid
                    && me != *uid
                {
                    warns.push(format!(
                        "the directory of the {} is owned by uid {uid}, not the running user",
                        f.role
                    ));
                }
            }
        }
        let vault_present = present.iter().any(|f| f.role == "vault");
        let key_present = present.iter().any(|f| f.role == "master key");
        if vault_present && !key_present && !v.key_in_env {
            warns.push(
                "a vault exists but no master key file or environment key was found: opening it would generate a new key"
                    .to_string(),
            );
        }
        if !fails.is_empty() {
            fails.extend(warns);
            (CheckStatus::Fail, joined(&fails, 8))
        } else if !warns.is_empty() {
            (CheckStatus::Warn, joined(&warns, 8))
        } else {
            (
                CheckStatus::Pass,
                format!("{}; owner matches; contents not read", oks.join(", ")),
            )
        }
    };
    make(ids::VAULT_PERMS, TITLE, status, detail)
}
