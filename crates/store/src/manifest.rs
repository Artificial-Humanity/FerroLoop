//! The committed manifest (GitHub tracker spec §4): a project's gates and
//! transitions, exported from the store that authors them, so another
//! machine can import them and resolve the gate an issue names.
//!
//! ⚠ The hash covers everything but itself. A manifest whose content does
//! not hash to the value it records was edited by hand, and is refused:
//! the store that authors the project is the only place a gate is authored.

use fl_core::ids::{GateId, ProjectId};
use fl_core::model::{GateDef, Transition};
use fl_core::store::{Catalog, StoreError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const MANIFEST_FORMAT: u64 = 1;
pub use fl_core::MANIFEST_PATH;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub commit: String,
    pub exported_at_unix: u64,
}

/// Everything the hash covers. ⚠ No store path and no project root: both
/// are specific to a machine, and the repository can be public (spec §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    pub format_version: u64,
    pub provenance: Provenance,
    pub project: ProjectId,
    /// Sorted by id. Never carries a `last_pass_commit`: a pass mark is
    /// earned on one machine and means nothing on another.
    pub gates: Vec<GateDef>,
    /// Sorted by name.
    pub transitions: Vec<Transition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub body: Body,
    pub content_sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error(
        "the manifest is not valid: {0}. Export it again from the store that authors the project"
    )]
    Parse(String),
    #[error(
        "the manifest is format {found}, and this version of fl reads format \
         {MANIFEST_FORMAT}. Use a version of fl that reads format {found}"
    )]
    Format { found: u64 },
    #[error(
        "the manifest was edited by hand: its content hashes to {found}, but it records \
         {recorded}. Gates are authored in the store that owns the project; export again \
         from there"
    )]
    HandEdited { recorded: String, found: String },
    #[error(
        "the manifest is inconsistent: {0}. Export it again from the store that authors the \
         project"
    )]
    Inconsistent(String),
    #[error("this store authors project {0}, so it cannot import it: it already holds the source")]
    AuthoringStore(ProjectId),
    #[error(
        "this store imported project {0} from a manifest, so it cannot export it. Export \
         from the store that authors it"
    )]
    NotAuthoring(ProjectId),
    #[error(
        "the manifest no longer lists gate `{name}` ({id}), which this store holds. A \
         re-import would remove a neighbour from every later verify, so it is refused. The \
         checked-out manifest may be older than the one this store imported — check out a \
         commit whose manifest lists the gate. If the gate really was removed where the \
         project is authored, restore it there and export again"
    )]
    WouldRemoveGate { id: GateId, name: String },
    #[error(
        "project {other} in this store already uses the root {root}. Import into a separate \
         store with `fl --db <path> manifest import` — but doing that on the machine that \
         authors this project makes its IRIs ambiguous between the two stores — or use the \
         existing project instead of importing"
    )]
    RootTaken { root: String, other: ProjectId },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// How a store's gate compares with the manifest's copy of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Currency {
    Current,
    Differs,
    Absent,
}

/// SHA-256 of the body's compact JSON, as lowercase hex. serde_json writes
/// struct fields in declaration order and the body holds no maps, so equal
/// bodies always produce equal bytes.
pub fn content_sha256(body: &Body) -> Result<String, ManifestError> {
    let bytes = serde_json::to_vec(body).map_err(|e| ManifestError::Parse(e.to_string()))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Every gate and transition of `project`, with pass marks cleared.
pub fn export(
    catalog: &dyn Catalog,
    project: &ProjectId,
    commit: &str,
    exported_at_unix: u64,
) -> Result<Manifest, ManifestError> {
    if catalog.get_project(project)?.is_none() {
        return Err(ManifestError::Inconsistent(format!(
            "{project} is held by this store, but it is not a project"
        )));
    }
    let mut gates = catalog.list_gates(project)?;
    for g in &mut gates {
        g.last_pass_commit = None;
    }
    gates.sort_by(|a, b| a.id.cmp(&b.id));
    let mut transitions = catalog.list_transitions(project)?;
    transitions.sort_by(|a, b| a.name.cmp(&b.name));
    let body = Body {
        format_version: MANIFEST_FORMAT,
        provenance: Provenance {
            commit: commit.to_string(),
            exported_at_unix,
        },
        project: project.clone(),
        gates,
        transitions,
    };
    let content_sha256 = content_sha256(&body)?;
    let m = Manifest {
        body,
        content_sha256,
    };
    // An export never writes a file its own parse would refuse — for
    // example a transition naming a gate of another project.
    m.check_consistent()?;
    Ok(m)
}

impl Manifest {
    /// Pretty JSON with a trailing newline, for a file a person reviews in a
    /// diff. The hash is over the compact form of the body, so layout is free.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("a manifest always serializes");
        s.push('\n');
        s
    }

    /// Parse, then refuse anything that is not exactly what an export wrote.
    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        // The format is read first, loosely, so a future format is named as
        // one rather than reported as whatever field it renamed.
        let loose: serde_json::Value =
            serde_json::from_str(text).map_err(|e| ManifestError::Parse(e.to_string()))?;
        match loose
            .pointer("/body/format_version")
            .and_then(|v| v.as_u64())
        {
            Some(MANIFEST_FORMAT) => {}
            Some(found) => return Err(ManifestError::Format { found }),
            None => {
                return Err(ManifestError::Parse(
                    "it has no `body.format_version`".into(),
                ));
            }
        }
        let m: Manifest =
            serde_json::from_value(loose).map_err(|e| ManifestError::Parse(e.to_string()))?;
        m.verify()?;
        Ok(m)
    }

    /// The hash and the internal consistency. `parse` calls it, and so does
    /// the store's import: a `Manifest` can be built by hand (its fields are
    /// public), and the store must not trust one it did not check.
    pub fn verify(&self) -> Result<(), ManifestError> {
        let found = content_sha256(&self.body)?;
        if found != self.content_sha256 {
            return Err(ManifestError::HandEdited {
                recorded: self.content_sha256.clone(),
                found,
            });
        }
        self.check_consistent()
    }

    fn check_consistent(&self) -> Result<(), ManifestError> {
        let p = &self.body.project;
        let mut ids = BTreeSet::new();
        for g in &self.body.gates {
            if g.project != *p {
                return Err(ManifestError::Inconsistent(format!(
                    "gate {} belongs to project {}, not {p}",
                    g.id, g.project
                )));
            }
            if g.last_pass_commit.is_some() {
                return Err(ManifestError::Inconsistent(format!(
                    "gate {} carries a pass mark; pass marks are local to each machine and \
                     are never exported",
                    g.id
                )));
            }
            if !ids.insert(g.id.clone()) {
                return Err(ManifestError::Inconsistent(format!(
                    "gate {} is listed twice",
                    g.id
                )));
            }
        }
        let mut names = BTreeSet::new();
        for t in &self.body.transitions {
            if t.project != *p {
                return Err(ManifestError::Inconsistent(format!(
                    "transition `{}` belongs to project {}, not {p}",
                    t.name, t.project
                )));
            }
            if !names.insert(t.name.clone()) {
                return Err(ManifestError::Inconsistent(format!(
                    "transition `{}` is listed twice",
                    t.name
                )));
            }
            for g in &t.gates {
                if !ids.contains(g) {
                    return Err(ManifestError::Inconsistent(format!(
                        "transition `{}` names gate {g}, which the manifest does not list",
                        t.name
                    )));
                }
            }
        }
        Ok(())
    }

    /// Compare a store's gate with the manifest's copy, ignoring the pass
    /// mark (spec §4.3).
    pub fn currency_of(&self, def: &GateDef) -> Currency {
        match self.body.gates.iter().find(|g| g.id == def.id) {
            None => Currency::Absent,
            Some(g) => {
                let mut bare = def.clone();
                bare.last_pass_commit = None;
                if *g == bare {
                    Currency::Current
                } else {
                    Currency::Differs
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::MemStore;
    use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Regret, Selector, State};

    fn kind(program: &str) -> GateKind {
        GateKind::Command(CommandSpec {
            program: program.into(),
            args: vec![],
            delivery: PopulationDelivery::Args,
            timeout_secs: 5,
            pass_codes: vec![0],
        })
    }

    fn glob() -> Selector {
        Selector::Glob {
            pattern: "**/*.rs".into(),
        }
    }

    /// Two projects in one store; `p` has two gates and a transition.
    fn store() -> (MemStore, ProjectId, GateId, GateId) {
        let s = MemStore::default();
        let p = s.add_project("/p").unwrap();
        let other = s.add_project("/q").unwrap();
        s.add_gate(&other, "other", kind("true"), glob(), 1, "c0", "o")
            .unwrap();
        let g1 = s
            .add_gate(&p, "fmt", kind("true"), glob(), 1, "c1", "o")
            .unwrap();
        let g2 = s
            .add_gate(&p, "lint", kind("/opt/tools/lint"), glob(), 1, "c1", "o")
            .unwrap();
        s.add_transition(Transition {
            project: p.clone(),
            name: "ship".into(),
            from: State::Review,
            to: State::Done,
            regret: Regret::High,
            gates: vec![g1.clone(), g2.clone()],
        })
        .unwrap();
        (s, p, g1, g2)
    }

    #[test]
    fn an_export_round_trips_through_its_file_form() {
        let (s, p, _, _) = store();
        let m = export(&s, &p, "abc", 7).unwrap();
        assert_eq!(Manifest::parse(&m.to_json()).unwrap(), m);
    }

    #[test]
    fn an_export_carries_only_the_named_project() {
        let (s, p, g1, g2) = store();
        let m = export(&s, &p, "abc", 7).unwrap();
        let ids: Vec<_> = m.body.gates.iter().map(|g| g.id.clone()).collect();
        assert_eq!(ids, vec![g1, g2]);
        assert_eq!(m.body.transitions.len(), 1);
    }

    #[test]
    fn a_pass_mark_is_never_exported() {
        let (s, p, g1, _) = store();
        let mut def = s.get_gate(&g1).unwrap().unwrap();
        def.last_pass_commit = Some("abc".into());
        s.update_gate(&def).unwrap();
        let m = export(&s, &p, "abc", 7).unwrap();
        assert!(m.body.gates.iter().all(|g| g.last_pass_commit.is_none()));
    }

    #[test]
    fn a_hand_edit_is_refused() {
        let (s, p, _, _) = store();
        let text = export(&s, &p, "abc", 7)
            .unwrap()
            .to_json()
            .replace("\"name\": \"fmt\"", "\"name\": \"fmt2\"");
        assert!(text.contains("fmt2"), "the edit must have landed");
        let err = Manifest::parse(&text).unwrap_err();
        assert!(matches!(err, ManifestError::HandEdited { .. }), "{err}");
    }

    #[test]
    fn a_future_format_is_named_as_one() {
        let (s, p, _, _) = store();
        let text = export(&s, &p, "abc", 7)
            .unwrap()
            .to_json()
            .replace("\"format_version\": 1", "\"format_version\": 9");
        let err = Manifest::parse(&text).unwrap_err();
        assert!(matches!(err, ManifestError::Format { found: 9 }), "{err}");
    }

    #[test]
    fn a_transition_naming_an_unlisted_gate_is_refused_even_with_a_correct_hash() {
        let (s, p, _, g2) = store();
        let mut m = export(&s, &p, "abc", 7).unwrap();
        m.body.gates.retain(|g| g.id != g2);
        m.content_sha256 = content_sha256(&m.body).unwrap();
        let err = Manifest::parse(&m.to_json()).unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");
    }

    /// Re-hash after a hand change, so only `check_consistent` can refuse.
    fn rehashed(mut m: Manifest) -> Manifest {
        m.content_sha256 = content_sha256(&m.body).unwrap();
        m
    }

    #[test]
    fn a_gate_of_another_project_is_refused() {
        let (s, p, g1, _) = store();
        let mut m = export(&s, &p, "abc", 7).unwrap();
        let g = m.body.gates.iter_mut().find(|g| g.id == g1).unwrap();
        g.project = ProjectId(fl_core::ids::seq_iri(999));
        let err = rehashed(m).verify().unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");
    }

    #[test]
    fn a_pass_mark_in_the_file_is_refused() {
        let (s, p, _, _) = store();
        let mut m = export(&s, &p, "abc", 7).unwrap();
        m.body.gates[0].last_pass_commit = Some("abc".into());
        let err = rehashed(m).verify().unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");
    }

    #[test]
    fn a_gate_or_transition_listed_twice_is_refused() {
        let (s, p, _, _) = store();
        let mut m = export(&s, &p, "abc", 7).unwrap();
        let dup = m.body.gates[0].clone();
        m.body.gates.push(dup);
        let err = rehashed(m).verify().unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");

        let mut m = export(&s, &p, "abc", 7).unwrap();
        let dup = m.body.transitions[0].clone();
        m.body.transitions.push(dup);
        let err = rehashed(m).verify().unwrap_err();
        assert!(matches!(err, ManifestError::Inconsistent(_)), "{err}");
    }

    #[test]
    fn currency_ignores_the_pass_mark_and_sees_every_other_change() {
        let (s, p, g1, _) = store();
        let m = export(&s, &p, "abc", 7).unwrap();
        let mut def = s.get_gate(&g1).unwrap().unwrap();
        def.last_pass_commit = Some("zzz".into());
        assert_eq!(m.currency_of(&def), Currency::Current);
        def.authored_at_commit = "c2".into();
        assert_eq!(m.currency_of(&def), Currency::Differs);
        def.id = GateId(fl_core::ids::seq_iri(999));
        assert_eq!(m.currency_of(&def), Currency::Absent);
    }
}
