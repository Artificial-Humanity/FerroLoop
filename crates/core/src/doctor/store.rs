//! `G7.store-health`: stores opened read-only and never migrated.

use super::{CheckStatus, DoctorCheck, DoctorInputs, StoreOutcome, ids, make};

const TITLE: &str = "Store health (read-only)";

pub(super) fn check(inputs: &DoctorInputs<'_>) -> DoctorCheck {
    let facts = &inputs.stores;
    let mut fails = 0usize;
    let mut warns = 0usize;
    let mut verified = 0usize;
    let mut lines: Vec<String> = Vec::new();
    for s in &facts.stores {
        let p = &s.path;
        match &s.outcome {
            StoreOutcome::Absent => lines.push(format!("{p}: absent")),
            StoreOutcome::Verified {
                engine,
                format,
                newest,
                backup_beside,
            } => {
                verified += 1;
                let fmt = match format {
                    Some(v) => format!("format {v} (this build reads up to {newest})"),
                    None => "no schema version recorded (written before versioning)".to_string(),
                };
                let mut line = format!("{p}: {engine} {fmt}, opened read-only");
                if *backup_beside {
                    line.push_str(
                        "; the .redb beside it is the backup a migration left, not current data",
                    );
                }
                lines.push(line);
            }
            StoreOutcome::WouldMigrate { format } => {
                warns += 1;
                let fmt = format.map_or(String::new(), |v| format!(" (format {v})"));
                lines.push(format!(
                    "{p}: a .redb{fmt} that a normal open would migrate to .fls; doctor does not migrate"
                ));
            }
            StoreOutcome::Locked => {
                warns += 1;
                lines.push(format!(
                    "{p}: held by a writer; doctor waited briefly and took no lock"
                ));
            }
            StoreOutcome::NeedsRecovery => {
                warns += 1;
                lines.push(format!(
                    "{p}: a .redb that was not closed cleanly; it needs recovery by a normal open (doctor does not repair)"
                ));
            }
            StoreOutcome::Unsupported { reason } => {
                fails += 1;
                lines.push(format!("{p}: {reason}"));
            }
            StoreOutcome::Invalid { reason } => {
                fails += 1;
                lines.push(format!("{p}: cannot be verified read-only ({reason})"));
            }
        }
    }
    for n in &facts.notes {
        lines.push(n.clone());
    }
    let status = if fails > 0 {
        CheckStatus::Fail
    } else if warns > 0 || !facts.notes.is_empty() {
        CheckStatus::Warn
    } else if verified > 0 {
        CheckStatus::Pass
    } else {
        CheckStatus::NotInstalled
    };
    let detail = if lines.is_empty() {
        "no store is configured".to_string()
    } else if status == CheckStatus::NotInstalled {
        format!("no store exists yet ({})", lines.join("; "))
    } else {
        lines.join("; ")
    };
    make(ids::STORE_HEALTH, TITLE, status, detail)
}
