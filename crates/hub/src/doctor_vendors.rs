//! Shared vendor checks for the local CLI and the Owner-only Hub report.

use std::path::Path;

use fl_core::doctor::{CheckStatus, DoctorCheck, DoctorReport, ids};
use fl_core::{
    VendorDoctorContext, VendorInstalledVersion, VendorRuntimeFacts, VendorUpdateStatus,
    VendorVersionRegistry,
};

use crate::lifecycle::launch_versions::read_local_vendor_baselines;

/// Append passive local vendor observations. The only history read is a keyed snapshot of
/// the selected store. Unknown or unavailable evidence cannot fail a report.
pub fn with_vendor_doctor_checks(
    report: DoctorReport,
    registry: Option<&VendorVersionRegistry>,
    context: &VendorDoctorContext<'_>,
    store: Option<&Path>,
    backend: Option<fl_store::BackendKind>,
) -> DoctorReport {
    let facts = registry
        .map(|registry| registry.doctor_facts(context))
        .unwrap_or_default();
    let baselines =
        read_local_vendor_baselines(store, backend, facts.iter().map(|fact| fact.vendor));
    let mut versions = Observations::default();
    let mut updates = Observations::default();
    for (fact, (cli_baseline, daemon_baseline)) in facts.into_iter().zip(baselines) {
        versions.version(fact.vendor, "CLI", &fact.cli, cli_baseline.as_deref());
        updates.policy(fact.vendor, "CLI", &fact.cli, Some(fact.hosted_cli_updates));
        if let Some(daemon) = fact.daemon {
            versions.version(fact.vendor, "daemon", &daemon, daemon_baseline.as_deref());
            updates.policy(fact.vendor, "daemon", &daemon, None);
        }
    }
    report.with_checks([
        versions.check(ids::VENDOR_VERSION_DRIFT, "Hosted vendor version drift"),
        updates.check(ids::VENDOR_UPDATE_POLICY, "Vendor update policy"),
    ])
}

#[derive(Default)]
struct Observations {
    rows: Vec<(CheckStatus, String)>,
}

impl Observations {
    fn version(
        &mut self,
        vendor: &str,
        component: &str,
        facts: &VendorRuntimeFacts,
        baseline: Option<&str>,
    ) {
        use CheckStatus::*;
        use VendorInstalledVersion::*;

        let (installed, status, finding) = match (&facts.installed_version, baseline) {
            (Known(version), Some(hosted)) if version == hosted => (version.as_str(), Pass, ""),
            (Known(version), Some(_)) => (version.as_str(), Warn, " (changed)"),
            (Known(version), None) => (version.as_str(), Unsupported, ""),
            (Unknown, _) => ("unknown", Unsupported, ""),
            (Absent, _) => ("absent", NotInstalled, ""),
        };
        let hosted = baseline.unwrap_or("unknown");
        self.rows.push((
            status,
            format!("{vendor} {component}: installed {installed}, last hosted {hosted}{finding}"),
        ));
    }

    fn policy(
        &mut self,
        vendor: &str,
        component: &str,
        facts: &VendorRuntimeFacts,
        hosted: Option<VendorUpdateStatus>,
    ) {
        use CheckStatus::*;
        use VendorUpdateStatus::*;

        let status = if facts.installed_version == VendorInstalledVersion::Absent {
            NotInstalled
        } else {
            match facts.configured_updates {
                Enabled => Warn,
                Disabled => Pass,
                Unknown => Unsupported,
            }
        };
        let configured = update_status(facts.configured_updates);
        let detail = match hosted {
            Some(policy) => format!(
                "configured {configured}, hosted launch {}",
                update_status(policy)
            ),
            None => format!("configured {configured} (shared daemon)"),
        };
        self.rows
            .push((status, format!("{vendor} {component}: {detail}")));
    }

    fn check(self, id: &str, title: &str) -> DoctorCheck {
        use CheckStatus::*;

        let status = [Warn, Unsupported, Pass, NotInstalled]
            .into_iter()
            .find(|status| self.rows.iter().any(|(candidate, _)| candidate == status))
            .unwrap_or(Unsupported);
        let detail = if self.rows.is_empty() {
            "unknown (vendor adapters unavailable)".into()
        } else {
            self.rows
                .into_iter()
                .map(|(_, text)| text)
                .collect::<Vec<_>>()
                .join(" | ")
        };
        DoctorCheck {
            id: id.into(),
            title: title.into(),
            status,
            detail,
        }
    }
}

fn update_status(status: VendorUpdateStatus) -> &'static str {
    match status {
        VendorUpdateStatus::Enabled => "enabled",
        VendorUpdateStatus::Disabled => "disabled",
        VendorUpdateStatus::Unknown => "unknown",
    }
}
