//! `G7.receipt-integrity`.

use super::{CheckStatus, DoctorCheck, DoctorInputs, ReceiptView, ids, make};

const TITLE: &str = "Install receipt integrity";

pub(super) fn check(inputs: &DoctorInputs<'_>, receipt: &ReceiptView) -> DoctorCheck {
    let shown = &inputs.receipt_shown;
    let (status, detail) = match receipt {
        // Receipt-absent means unmanaged, not uninstalled and not healthy.
        ReceiptView::Absent => (
            CheckStatus::NotInstalled,
            "unmanaged (no install receipt)".to_string(),
        ),
        ReceiptView::Unlocatable(why) => (
            CheckStatus::Unsupported,
            format!("cannot locate the install receipt: {why}"),
        ),
        ReceiptView::Unreadable(why) => (
            CheckStatus::Fail,
            format!(
                "install receipt {shown} cannot be read ({why}); it is not treated as unmanaged"
            ),
        ),
        ReceiptView::Invalid(why) => (
            CheckStatus::Fail,
            format!("install receipt {shown} is invalid ({why}); it is not treated as unmanaged"),
        ),
        ReceiptView::Valid(r) => (
            CheckStatus::Pass,
            format!(
                "{shown} is valid (schema_version {}, release {}, profile {}, {} products, {} owned paths, {} aliases, {} units)",
                r.schema_version,
                r.release.name,
                r.profile,
                r.products.len(),
                r.paths.len(),
                r.aliases.len(),
                r.units.len()
            ),
        ),
    };
    make(ids::RECEIPT_INTEGRITY, TITLE, status, detail)
}
