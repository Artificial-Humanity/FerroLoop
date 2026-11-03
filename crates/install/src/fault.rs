//! The crash-injection seam. Production code never builds a [`Faults`] with anything set; the
//! tests do, and the `fl` binary reads one from `FL_INSTALL_FAULT` only when it is built with
//! the `fault-injection` feature (fl-cli's dev-dependency, never a shipped build).

use fl_core::install::Step;
use std::fmt;

/// Before a step's action (its journal line is already written) or after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Before,
    After,
}

/// A place between two steps of the transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultPoint {
    pub step: Step,
    pub when: When,
}

impl FaultPoint {
    pub fn before(step: Step) -> Self {
        Self {
            step,
            when: When::Before,
        }
    }

    pub fn after(step: Step) -> Self {
        Self {
            step,
            when: When::After,
        }
    }

    /// Every point of a transaction that runs `steps`: after `begin`, and before and after
    /// each later step. (Before `begin` nothing is journaled, so there is no transaction.)
    pub fn all(steps: &[Step]) -> Vec<FaultPoint> {
        let mut v = Vec::new();
        for &s in steps {
            if s != Step::Begin {
                v.push(Self::before(s));
            }
            v.push(Self::after(s));
        }
        v
    }

    /// `before:stage`, `after:link_current`.
    pub fn parse(s: &str) -> Option<Self> {
        let (when, step) = s.split_once(':')?;
        let when = match when {
            "before" => When::Before,
            "after" => When::After,
            _ => return None,
        };
        Some(Self {
            step: Step::from_wire(step)?,
            when,
        })
    }
}

impl fmt::Display for FaultPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let w = match self.when {
            When::Before => "before",
            When::After => "after",
        };
        write!(f, "{w}:{}", self.step.as_wire())
    }
}

/// What dying means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Death {
    /// Return [`crate::InstallError::Crashed`] up the stack with no cleanup (in-process tests).
    Return,
    /// End the process at once with this status, running no destructor (the `fl` binary).
    Exit(i32),
}

/// The faults to inject.
#[derive(Debug, Clone, Default)]
pub struct Faults {
    pub die_at: Option<(FaultPoint, Death)>,
    /// Make a step's action fail with this errno (28 = ENOSPC, 13 = EACCES) instead of running.
    pub fail_step: Option<(Step, i32)>,
    /// Make the n-th (0-based) file copied while staging fail with this errno.
    pub fail_stage_file: Option<(usize, i32)>,
    /// Die after the n-th (0-based) undo action of a rollback.
    pub die_in_rollback: Option<(usize, Death)>,
}

impl Faults {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn crash(point: FaultPoint) -> Self {
        Self {
            die_at: Some((point, Death::Return)),
            ..Self::default()
        }
    }

    pub fn fail(step: Step, errno: i32) -> Self {
        Self {
            fail_step: Some((step, errno)),
            ..Self::default()
        }
    }

    /// `die:before:stage`, `exit:after:publish`, `fail:stage:28`, `fail_file:2:28`,
    /// `die_rollback:1`. `die:` returns a crash error, `exit:` ends the process with status 99.
    pub fn parse(spec: &str) -> Option<Self> {
        let (kind, rest) = spec.split_once(':')?;
        match kind {
            "die" | "exit" => {
                let death = if kind == "exit" {
                    Death::Exit(99)
                } else {
                    Death::Return
                };
                Some(Self {
                    die_at: Some((FaultPoint::parse(rest)?, death)),
                    ..Self::default()
                })
            }
            "fail" => {
                let (step, errno) = rest.split_once(':')?;
                Some(Self::fail(Step::from_wire(step)?, errno.parse().ok()?))
            }
            "fail_file" => {
                let (n, errno) = rest.split_once(':')?;
                Some(Self {
                    fail_stage_file: Some((n.parse().ok()?, errno.parse().ok()?)),
                    ..Self::default()
                })
            }
            "exit_rollback" => Some(Self {
                die_in_rollback: Some((rest.parse().ok()?, Death::Exit(99))),
                ..Self::default()
            }),
            _ => None,
        }
    }

    pub(crate) fn hit(&self, point: FaultPoint) -> crate::Result<()> {
        match self.die_at {
            Some((p, death)) if p == point => die(death, point),
            _ => Ok(()),
        }
    }

    pub(crate) fn fail_io(&self, step: Step) -> std::io::Result<()> {
        match self.fail_step {
            Some((s, errno)) if s == step => Err(std::io::Error::from_raw_os_error(errno)),
            _ => Ok(()),
        }
    }

    pub(crate) fn stage_file(&self, index: usize) -> std::io::Result<()> {
        match self.fail_stage_file {
            Some((n, errno)) if n == index => Err(std::io::Error::from_raw_os_error(errno)),
            _ => Ok(()),
        }
    }

    pub(crate) fn rollback_action(&self, index: usize) -> crate::Result<()> {
        match self.die_in_rollback {
            Some((n, death)) if n == index => die(
                death,
                FaultPoint {
                    step: Step::Begin,
                    when: When::After,
                },
            ),
            _ => Ok(()),
        }
    }
}

/// The faults named by `FL_INSTALL_FAULT`, ONLY in a build with the `fault-injection` feature
/// (the test builds of fl-cli); in any other build this is `Faults::none()`, so a shipped
/// binary has no such seam.
pub fn faults_from_env() -> Faults {
    #[cfg(feature = "fault-injection")]
    if let Some(spec) = std::env::var_os("FL_INSTALL_FAULT")
        && let Some(f) = spec.to_str().and_then(Faults::parse)
    {
        return f;
    }
    Faults::none()
}

fn die(death: Death, point: FaultPoint) -> crate::Result<()> {
    match death {
        Death::Return => Err(crate::InstallError::Crashed(point)),
        // SAFETY: _exit ends the process without running destructors or atexit handlers,
        // which is the point: it stands for a kill.
        Death::Exit(code) => unsafe { libc::_exit(code) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_cover_every_gap_between_steps() {
        let steps = [Step::Begin, Step::Stage, Step::Commit];
        let pts = FaultPoint::all(&steps);
        assert_eq!(
            pts.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            [
                "after:begin",
                "before:stage",
                "after:stage",
                "before:commit",
                "after:commit"
            ]
        );
    }

    #[test]
    fn specs_parse() {
        let f = Faults::parse("exit:before:link_current").unwrap();
        assert_eq!(
            f.die_at,
            Some((FaultPoint::before(Step::LinkCurrent), Death::Exit(99)))
        );
        assert_eq!(
            Faults::parse("fail:write_receipt:28").unwrap().fail_step,
            Some((Step::WriteReceipt, 28))
        );
        assert_eq!(
            Faults::parse("fail_file:2:28").unwrap().fail_stage_file,
            Some((2, 28))
        );
        assert!(Faults::parse("nonsense").is_none());
        assert!(Faults::parse("die:sideways:stage").is_none());
    }
}
