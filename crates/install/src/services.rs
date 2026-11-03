//! Service managers, as a SEAM. In this release the installer only PLANS what registering,
//! enabling or starting a unit would run, and lists it as "not executed". The real managers
//! cannot execute anything (they have no process-spawning code at all); the fakes record what
//! they were asked, so a test can prove nothing was.

use fl_core::install::{ActionKind, InstallPlan, Os, PlannedAction};
use std::sync::{Arc, Mutex};

/// One command a manager would run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl ServiceCommand {
    fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The command as a person would type it.
    pub fn render(&self) -> String {
        let mut s = self.program.clone();
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    #[error("running service commands is not implemented in this release")]
    NotImplemented,
}

/// A per-user service manager.
pub trait ServiceManager {
    fn name(&self) -> &'static str;
    /// The commands registering the unit file at `unit_path` would run.
    fn plan_register(&self, unit_path: &str) -> Vec<ServiceCommand>;
    /// The commands enabling `unit` at boot would run.
    fn plan_enable(&self, unit: &str) -> Vec<ServiceCommand>;
    /// The commands starting `unit` would run.
    fn plan_start(&self, unit: &str) -> Vec<ServiceCommand>;
    /// Run one command. The installer of this release never calls it.
    fn execute(&self, cmd: &ServiceCommand) -> Result<(), ServiceError>;
}

fn systemctl_plan(kind: ActionKind, subject: &str, path: &str) -> Vec<ServiceCommand> {
    match kind {
        ActionKind::RegisterService => vec![
            ServiceCommand::new("systemctl", &["--user", "daemon-reload"]),
            // The unit file at `path` is already in the user unit directory by then.
            ServiceCommand::new("systemctl", &["--user", "cat", path]),
        ],
        ActionKind::EnableAtBoot => vec![ServiceCommand::new(
            "systemctl",
            &["--user", "enable", subject],
        )],
        _ => vec![ServiceCommand::new(
            "systemctl",
            &["--user", "start", subject],
        )],
    }
}

/// `systemctl --user`. Plans only.
#[derive(Debug, Default)]
pub struct SystemctlUser;

impl ServiceManager for SystemctlUser {
    fn name(&self) -> &'static str {
        "systemctl --user"
    }
    fn plan_register(&self, unit_path: &str) -> Vec<ServiceCommand> {
        systemctl_plan(ActionKind::RegisterService, "", unit_path)
    }
    fn plan_enable(&self, unit: &str) -> Vec<ServiceCommand> {
        systemctl_plan(ActionKind::EnableAtBoot, unit, "")
    }
    fn plan_start(&self, unit: &str) -> Vec<ServiceCommand> {
        systemctl_plan(ActionKind::StartListener, unit, "")
    }
    fn execute(&self, _cmd: &ServiceCommand) -> Result<(), ServiceError> {
        Err(ServiceError::NotImplemented)
    }
}

/// `launchctl`. Plans only.
#[derive(Debug, Default)]
pub struct Launchctl;

impl ServiceManager for Launchctl {
    fn name(&self) -> &'static str {
        "launchctl"
    }
    fn plan_register(&self, unit_path: &str) -> Vec<ServiceCommand> {
        vec![ServiceCommand::new(
            "launchctl",
            &["bootstrap", "gui/$UID", unit_path],
        )]
    }
    fn plan_enable(&self, unit: &str) -> Vec<ServiceCommand> {
        vec![ServiceCommand::new("launchctl", &["enable", unit])]
    }
    fn plan_start(&self, unit: &str) -> Vec<ServiceCommand> {
        vec![ServiceCommand::new("launchctl", &["kickstart", unit])]
    }
    fn execute(&self, _cmd: &ServiceCommand) -> Result<(), ServiceError> {
        Err(ServiceError::NotImplemented)
    }
}

/// The manager for `os`, if the installer plans for it.
pub fn manager_for(os: Os) -> Option<Box<dyn ServiceManager>> {
    match os {
        Os::Linux => Some(Box::new(SystemctlUser)),
        Os::Macos => Some(Box::new(Launchctl)),
        Os::Windows => None,
    }
}

/// A manager that records every call, for tests. `executed()` stays empty unless something
/// calls `execute`.
#[derive(Debug, Default, Clone)]
pub struct FakeManager {
    label: &'static str,
    planned: Arc<Mutex<Vec<String>>>,
    executed: Arc<Mutex<Vec<ServiceCommand>>>,
}

pub type FakeSystemctl = FakeManager;
pub type FakeLaunchctl = FakeManager;

impl FakeManager {
    pub fn systemctl() -> Self {
        Self {
            label: "fake systemctl --user",
            ..Self::default()
        }
    }

    pub fn launchctl() -> Self {
        Self {
            label: "fake launchctl",
            ..Self::default()
        }
    }

    pub fn planned(&self) -> Vec<String> {
        self.planned.lock().unwrap().clone()
    }

    pub fn executed(&self) -> Vec<ServiceCommand> {
        self.executed.lock().unwrap().clone()
    }

    fn note(&self, what: &str, arg: &str) {
        self.planned.lock().unwrap().push(format!("{what} {arg}"));
    }
}

impl ServiceManager for FakeManager {
    fn name(&self) -> &'static str {
        self.label
    }
    fn plan_register(&self, unit_path: &str) -> Vec<ServiceCommand> {
        self.note("register", unit_path);
        vec![ServiceCommand::new("fake", &["register", unit_path])]
    }
    fn plan_enable(&self, unit: &str) -> Vec<ServiceCommand> {
        self.note("enable", unit);
        vec![ServiceCommand::new("fake", &["enable", unit])]
    }
    fn plan_start(&self, unit: &str) -> Vec<ServiceCommand> {
        self.note("start", unit);
        vec![ServiceCommand::new("fake", &["start", unit])]
    }
    fn execute(&self, cmd: &ServiceCommand) -> Result<(), ServiceError> {
        self.executed.lock().unwrap().push(cmd.clone());
        Ok(())
    }
}

/// The commands `action` would run under `mgr`.
fn commands(mgr: &dyn ServiceManager, a: &PlannedAction) -> Vec<ServiceCommand> {
    match a.kind {
        ActionKind::RegisterService => mgr.plan_register(a.path.as_deref().unwrap_or(&a.subject)),
        ActionKind::EnableAtBoot => mgr.plan_enable(&a.subject),
        ActionKind::StartListener => mgr.plan_start(&a.subject),
        _ => Vec::new(),
    }
}

/// One line per planned action of the plan, all of them "not executed": a plain install
/// performs none of them (spec §2).
pub fn not_executed(plan: &InstallPlan, mgr: Option<&dyn ServiceManager>) -> Vec<String> {
    plan.actions
        .iter()
        .map(|a| {
            let mut s = format!("not executed: {} {}", a.kind.as_wire(), a.subject);
            if let Some(m) = mgr {
                let cmds = commands(m, a);
                if !cmds.is_empty() {
                    let c: Vec<String> = cmds.iter().map(ServiceCommand::render).collect();
                    s.push_str(&format!(" (would run: {})", c.join("; ")));
                }
            }
            s.push_str(" [not implemented in this release]");
            s
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_managers_plan_but_never_execute() {
        for m in [
            Box::new(SystemctlUser) as Box<dyn ServiceManager>,
            Box::new(Launchctl),
        ] {
            let cmds = m.plan_register("/u/x.service");
            assert!(!cmds.is_empty());
            assert_eq!(m.execute(&cmds[0]), Err(ServiceError::NotImplemented));
        }
        assert!(manager_for(Os::Windows).is_none());
        assert_eq!(manager_for(Os::Linux).unwrap().name(), "systemctl --user");
    }

    #[test]
    fn the_fake_records_what_it_is_asked() {
        let f = FakeManager::systemctl();
        f.plan_enable("a.service");
        assert_eq!(f.planned(), ["enable a.service"]);
        assert!(f.executed().is_empty());
    }

    #[test]
    fn systemd_plans_render_as_typed() {
        let c = SystemctlUser.plan_enable("ferrowire.service");
        assert_eq!(c[0].render(), "systemctl --user enable ferrowire.service");
    }
}
