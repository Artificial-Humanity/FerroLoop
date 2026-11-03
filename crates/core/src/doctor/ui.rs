//! `G7.code-ui-agree`.

use super::{CheckStatus, DoctorCheck, DoctorInputs, ids, make, short};
use crate::ui_bundle::{UI_BUILD_INFO_FILE, code_ui_agree};

const TITLE: &str = "Code and UI agree";

pub(super) fn check(inputs: &DoctorInputs<'_>) -> DoctorCheck {
    let ui = &inputs.ui;
    let b = &inputs.binary;
    let (status, detail) = match (&ui.dir, ui.dir_exists) {
        (None, _) => (
            CheckStatus::NotInstalled,
            "no UI bundle is configured: nothing serves one".to_string(),
        ),
        (Some(dir), false) => (CheckStatus::NotInstalled, format!("no UI bundle at {dir}")),
        (Some(dir), true) => match (&ui.info, code_ui_agree(b, ui.info.as_ref())) {
            (None, _) => (
                CheckStatus::Unsupported,
                format!(
                    "{dir} has no valid {UI_BUILD_INFO_FILE}: agreement with the binary cannot be proven"
                ),
            ),
            (Some(_), None) => (
                CheckStatus::Unsupported,
                format!(
                    "this binary's commit is unknown: agreement with the UI at {dir} cannot be proven"
                ),
            ),
            (Some(info), Some(false)) => (
                CheckStatus::Fail,
                format!(
                    "the UI at {dir} was built from {}, this binary from {}",
                    short(info.get("commit").and_then(|c| c.as_str()).unwrap_or("?")),
                    short(&b.commit)
                ),
            ),
            (Some(info), Some(true)) => {
                let ui_dirty = info.get("dirty").and_then(|d| d.as_bool());
                let ui_commit = info.get("commit").and_then(|c| c.as_str()).unwrap_or("?");
                if b.dirty != Some(false) || ui_dirty == Some(true) {
                    (
                        CheckStatus::Warn,
                        format!(
                            "the UI at {dir} and this binary name the same commit ({}), but a dirty or unknown-state tree does not prove the same contents",
                            short(ui_commit)
                        ),
                    )
                } else {
                    (
                        CheckStatus::Pass,
                        format!(
                            "the UI at {dir} was built from {}, this binary's commit",
                            short(ui_commit)
                        ),
                    )
                }
            }
        },
    };
    make(ids::CODE_UI_AGREE, TITLE, status, detail)
}
