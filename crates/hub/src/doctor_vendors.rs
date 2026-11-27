//! Shared vendor checks for the local CLI and the Owner-only Hub report.

use std::path::Path;

use fl_core::doctor::{CheckStatus, DoctorCheck, DoctorReport, ids};
use fl_core::{
    VendorDoctorContext, VendorInstalledVersion, VendorRuntimeFacts, VendorUpdateStatus,
    VendorVersionRegistry,
};

use crate::lifecycle::launch_versions::read_local_vendor_baselines;

/// Append passive local vendor observations using one keyed snapshot per selected store.
/// Unknown or unavailable evidence cannot fail a report.
pub fn with_vendor_doctor_checks<'a>(
    report: DoctorReport,
    registry: Option<&VendorVersionRegistry>,
    context: &VendorDoctorContext<'_>,
    stores: impl IntoIterator<Item = &'a Path>,
    backend: Option<fl_store::BackendKind>,
) -> DoctorReport {
    let facts = registry
        .map(|registry| registry.doctor_facts(context))
        .unwrap_or_default();
    let baselines =
        read_local_vendor_baselines(stores, backend, facts.iter().map(|fact| fact.vendor));
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
                Unknown(_) => Unsupported,
            }
        };
        let detail = match (hosted, facts.configured_updates) {
            (Some(policy), configured) => format!(
                "configured {}, hosted launch {}",
                update_status(configured),
                update_status(policy)
            ),
            (None, Unknown(reason)) => format!("configured unknown (shared daemon; {reason})"),
            (None, configured) => {
                format!("configured {} (shared daemon)", update_status(configured))
            }
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

/// The status word, with the fixed non-sensitive reason when it is `unknown`.
fn update_status(status: VendorUpdateStatus) -> String {
    match status {
        VendorUpdateStatus::Enabled => "enabled".into(),
        VendorUpdateStatus::Disabled => "disabled".into(),
        VendorUpdateStatus::Unknown(reason) => format!("unknown ({reason})"),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use fl_core::doctor::{DoctorReport, Summary};
    use fl_core::{VendorDoctorContext, VendorRuntimeFacts, VendorVersionAdapter};

    use super::*;

    #[derive(Debug)]
    struct Stub;

    impl VendorVersionAdapter for Stub {
        fn vendor_id(&self) -> &'static str {
            "stub"
        }

        fn auto_update_disable_env(&self) -> Option<(&'static str, &'static str)> {
            Some(("STUB_NO_UPDATE", "1"))
        }

        fn configured_cli_updates(&self, _: &VendorDoctorContext<'_>) -> VendorUpdateStatus {
            VendorUpdateStatus::Unknown("config.toml unreadable")
        }

        fn doctor_daemon_facts(&self, _: &VendorDoctorContext<'_>) -> Option<VendorRuntimeFacts> {
            Some(VendorRuntimeFacts {
                installed_version: VendorInstalledVersion::Known("1.0.0".into()),
                configured_updates: VendorUpdateStatus::Unknown("settings.json failed validation"),
            })
        }

        fn version_from_executable(&self, _: &Path) -> Option<String> {
            None
        }
    }

    #[test]
    fn unknown_update_settings_name_a_short_reason() {
        let mut registry = VendorVersionRegistry::default();
        registry.register(Stub);
        let env = |_: &str| -> Option<OsString> { None };
        let report = DoctorReport {
            schema_version: 1,
            overall: CheckStatus::Pass,
            summary: Summary::default(),
            checks: Vec::new(),
        };
        let report = with_vendor_doctor_checks(
            report,
            Some(&registry),
            &VendorDoctorContext {
                home: None,
                path: None,
                env: &env,
                system_root: None,
            },
            [],
            None,
        );
        let policy = report.check(ids::VENDOR_UPDATE_POLICY).unwrap();
        assert_eq!(policy.status, CheckStatus::Unsupported);
        assert_eq!(
            policy.detail,
            "stub CLI: configured unknown (config.toml unreadable), hosted launch disabled \
             | stub daemon: configured unknown (shared daemon; settings.json failed validation)"
        );
    }
}
