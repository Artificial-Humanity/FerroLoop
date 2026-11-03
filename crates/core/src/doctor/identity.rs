//! `G7.release-identity`.

use super::{CheckStatus, DoctorCheck, DoctorInputs, ReceiptView, ids, make, short};

const TITLE: &str = "Release identity";

pub(super) fn check(inputs: &DoctorInputs<'_>, receipt: &ReceiptView) -> DoctorCheck {
    let b = &inputs.binary;
    let mut status = CheckStatus::Pass;
    let mut notes: Vec<String> = Vec::new();
    let mut warn = |status: &mut CheckStatus, note: String| {
        *status = CheckStatus::Warn;
        notes.push(note);
    };

    if !b.commit_known() {
        warn(
            &mut status,
            "the commit is unknown, so the build's identity cannot be proven".into(),
        );
    } else {
        match b.dirty {
            Some(true) => warn(
                &mut status,
                "built from a dirty working tree: the commit does not describe this binary".into(),
            ),
            None => warn(&mut status, "whether the tree was dirty is unknown".into()),
            Some(false) => {}
        }
    }

    if let ReceiptView::Valid(r) = receipt {
        let installed = &r.release;
        match &installed.commit {
            Some(rc) if b.commit_known() && rc != &b.commit => warn(
                &mut status,
                format!(
                    "the installed release {} was built from {}, but this binary is {}",
                    installed.name,
                    short(rc),
                    short(&b.commit)
                ),
            ),
            None if b.commit_known() => warn(
                &mut status,
                "the receipt records no commit to compare".into(),
            ),
            _ => {}
        }
        if installed.version != b.version {
            warn(
                &mut status,
                format!(
                    "this binary is version {}, the install receipt records {}",
                    b.version, installed.version
                ),
            );
        }
        if let Some(id) = &b.release_id
            && id != &installed.name
        {
            warn(
                &mut status,
                format!(
                    "this binary's release id is {id}, the install receipt records {}",
                    installed.name
                ),
            );
        }
    }

    let mut detail = format!("{}; source: {}", b.human_line(inputs.binary_name), b.source);
    if b.release_id.is_none() {
        detail.push_str("; no release id (a source build is not an official release)");
    }
    if !notes.is_empty() {
        detail.push_str("; ");
        detail.push_str(&notes.join("; "));
    }
    make(ids::RELEASE_IDENTITY, TITLE, status, detail)
}
