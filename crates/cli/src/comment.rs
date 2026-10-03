//! Decision comments (GitHub ledger spec §4).

use crate::ctx::{Ctx, Flush};
use fl_core::decision::Outcome;
use fl_core::ids::ProjectId;
use fl_core::iri::Iri;
use fl_core::store::{Catalog, Ledger, StoreError};
use fl_github::GithubLedger;
use fl_github::ledger::disclose::{self, Visibility};
use fl_github::ledger::layout::decision_subject;
use fl_github::ledger::render::{self, Catalogued, DecisionView};
use fl_store::RedbStore;

/// What the local catalog says of `project`: each gate's name, and each
/// transition's gates. A comment names gates by it (spec §4.2).
pub fn catalogued(store: &RedbStore, project: &ProjectId) -> Result<Catalogued, StoreError> {
    let mut cat = Catalogued::default();
    for g in store.list_gates(project)? {
        cat.names.insert(g.id, g.name);
    }
    for t in store.list_transitions(project)? {
        cat.transitions.insert(t.name, t.gates);
    }
    Ok(cat)
}

/// How a person names `item`: its issue number, or its IRI.
fn named(item: &Iri) -> String {
    fl_github::meta::parse_issue_url(item)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| item.to_string())
}

/// The command that posts `item`'s missing comments (spec §4.3).
pub fn recovery(item: &Iri) -> String {
    format!("fl github ledger comment {}", named(item))
}

/// Post the comment of each decision this command flushed (spec §4.1),
/// once its state change is done. `completed`: the command's state change
/// returned without error (§4.2).
///
/// ⚠ A comment that cannot be posted is a warning, never an error: the
/// decision stands, and so does its state change when it made one that
/// completed ([`what_stands`]); the command keeps its own exit code. Each
/// warning names the command that posts the comment later.
pub fn post_after(ctx: &Ctx<'_>, project: &ProjectId, completed: bool) -> Vec<String> {
    let Some(witness) = ctx.witness else {
        return Vec::new();
    };
    let Some(gl) = ctx.github_ledger else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    for flush in witness.take() {
        // A flush that published nothing has nothing to show.
        if flush.flushed.commit.is_none() {
            continue;
        }
        // ⚠ An id fl does not write gets no comment, now or from recovery
        // (`render::markable`): its marker could close the HTML comment
        // early. Someone wrote that line by hand; say what to do about it.
        if !render::markable(&flush.decision.id) {
            warnings.push(format!(
                "warning: decision {} has an id fl does not write, so it gets no comment; \
                 someone wrote its ledger line by hand. Run `fl github ledger verify`, then \
                 `fl github ledger quarantine` on that line",
                flush.decision.id.as_str().escape_debug()
            ));
            continue;
        }
        if let Err(e) = post_one(ctx.store, gl, project, &flush, completed) {
            let item = decision_subject(&flush.decision);
            warnings.push(format!(
                "warning: the decision's comment was not posted on issue {}: {}. {}; run `{}` \
                 to post it",
                named(item),
                fl_core::as_clause(&e),
                what_stands(&flush.decision.outcome, completed),
                recovery(item)
            ));
        }
    }
    warnings
}

/// What a failed comment's warning says stands: a state change only when
/// the decision made one and it completed.
fn what_stands(outcome: &Outcome, completed: bool) -> &'static str {
    let changes_state = matches!(
        outcome,
        Outcome::Move { allowed: true, .. }
            | Outcome::Reproduce { accepted: true, .. }
            | Outcome::Verify { closed: true, .. }
    );
    if changes_state && completed {
        "The decision and its state change stand"
    } else {
        "The decision stands"
    }
}

/// [`post_after`], each warning on stderr.
pub fn after(ctx: &Ctx<'_>, project: &ProjectId, completed: bool) {
    for w in post_after(ctx, project, completed) {
        eprintln!("{w}");
    }
}

fn post_one(
    store: &RedbStore,
    gl: &GithubLedger<'_>,
    project: &ProjectId,
    flush: &Flush,
    completed: bool,
) -> Result<(), StoreError> {
    let visibility = gl.visibility()?;
    let view = local_view(store, gl, project, flush, visibility)?;
    let state = render::state_line(&flush.decision.outcome, completed);
    let body = render::render(&view, &gl.repo().full_name, visibility, Some(&state));
    gl.post_comment(decision_subject(&flush.decision), &body)
}

/// The view of a decision this command flushed: the entries it rests on,
/// read from the local store rather than back from GitHub, projected for
/// the repository's visibility exactly as the flush published them
/// (decision 2).
fn local_view(
    store: &RedbStore,
    gl: &GithubLedger<'_>,
    project: &ProjectId,
    flush: &Flush,
    visibility: Visibility,
) -> Result<DecisionView, StoreError> {
    let cat = catalogued(store, project)?;
    let mut runs = Vec::new();
    for g in render::candidate_gates(&flush.decision.outcome, &cat) {
        runs.extend(
            store
                .gate_runs(&g)?
                .iter()
                .map(|r| disclose::run(r, visibility)),
        );
    }
    let attempts = match flush.decision.outcome {
        Outcome::Attempt { .. } => store
            .attempts(project)?
            .iter()
            .map(|a| disclose::attempt(a, visibility))
            .collect(),
        _ => Vec::new(),
    };
    Ok(render::view(
        flush.decision.clone(),
        gl.by()?,
        flush.flushed.commit.clone(),
        &runs,
        &attempts,
        &cat,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::Witness;
    use crate::testing::Flushes;
    use fl_core::at::At;
    use fl_core::decision::{Decision, TransitionOutcome};
    use fl_core::ids::{RecordId, seq_iri};
    use fl_github::fake::FakeGithub;
    use fl_github::{Client, EnvToken, Repo};

    fn issue(n: u64) -> Iri {
        Iri::parse(&format!("https://github.com/acme/widgets/issues/{n}")).unwrap()
    }

    // A state change is said to stand only when there was one and it
    // completed.
    #[test]
    fn a_failed_comment_says_what_stands() {
        let moved = |allowed| Outcome::Move {
            from: fl_core::model::State::Todo,
            to: fl_core::model::State::Doing,
            transitions: vec![],
            allowed,
        };
        let state = "The decision and its state change stand";
        let only = "The decision stands";
        assert_eq!(what_stands(&moved(true), true), state);
        assert_eq!(what_stands(&moved(true), false), only);
        assert_eq!(what_stands(&moved(false), true), only);
        let gate = fl_core::ids::GateId(seq_iri(1));
        assert_eq!(
            what_stands(
                &Outcome::Reproduce {
                    gate: gate.clone(),
                    accepted: true
                },
                true
            ),
            state
        );
        assert_eq!(
            what_stands(
                &Outcome::Reproduce {
                    gate: gate.clone(),
                    accepted: false
                },
                true
            ),
            only
        );
        let verify = |closed| Outcome::Verify {
            reproduction: gate.clone(),
            reproduction_passed: closed,
            regressions: vec![],
            closed,
        };
        assert_eq!(what_stands(&verify(true), true), state);
        assert_eq!(what_stands(&verify(false), true), only);
        let check = Outcome::Check {
            transition: TransitionOutcome {
                transition: "launch".into(),
                passed: true,
            },
        };
        assert_eq!(what_stands(&check, true), only);
        let attempt = Outcome::Attempt {
            status: fl_core::log::AttemptStatus::Completed,
        };
        assert_eq!(what_stands(&attempt, true), only);
    }

    // ⚠ A verify's comment reads every gate in `names` as a neighbour
    // (`render::candidate_gates`), so another project's gate must never be
    // in it.
    #[test]
    fn the_catalog_a_comment_reads_holds_only_the_projects_own_gates() {
        use fl_core::model::{CommandSpec, GateKind, PopulationDelivery, Selector};
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("s.redb")).unwrap();
        let gate = |p: &ProjectId, name: &str| {
            let kind = GateKind::Command(CommandSpec {
                program: "true".into(),
                args: vec![],
                delivery: PopulationDelivery::Args,
                timeout_secs: 5,
                pass_codes: vec![0],
            });
            let sel = Selector::Glob {
                pattern: "**/*".into(),
            };
            store.add_gate(p, name, kind, sel, 1, "c", "o").unwrap()
        };
        let ours = store.add_project("/ours").unwrap();
        let theirs = store.add_project("/theirs").unwrap();
        let own = gate(&ours, "own");
        gate(&theirs, "foreign");
        let cat = catalogued(&store, &ours).unwrap();
        assert_eq!(cat.names.keys().collect::<Vec<_>>(), vec![&own], "{cat:?}");
    }

    #[test]
    fn the_recovery_command_names_the_issue_by_its_number() {
        assert_eq!(recovery(&issue(3)), "fl github ledger comment 3");
        assert_eq!(
            recovery(&seq_iri(5)),
            format!("fl github ledger comment {}", seq_iri(5))
        );
    }

    // A comment points at the commit that holds its decision: a flush that
    // published nothing gets no comment. One that did gets one — so this
    // test sees a comment when there should be one.
    #[test]
    fn a_flush_that_published_nothing_gets_no_comment() {
        let fake = FakeGithub::start("acme/widgets");
        fake.plain_issue(&[], false);
        let client = Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        );
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("s.redb")).unwrap();
        let project = store.add_project("/r").unwrap();
        let repo = Repo {
            full_name: "acme/widgets".into(),
            node_id: "R_1".into(),
        };
        let gl = GithubLedger::new(&client, repo, &store);
        for (flushes, comments) in [(Flushes::publishing_nothing(), 0), (Flushes::default(), 1)] {
            let witness = Witness::new(&flushes);
            let ctx = Ctx {
                store: &store,
                tracker: &store,
                ledger: &witness,
                handles: &store,
                github: None,
                github_ledger: Some(&gl),
                witness: Some(&witness),
                tracker_label: String::new(),
            };
            witness
                .flush(Decision {
                    id: seq_iri(9),
                    at: At::from_unix_millis(1),
                    record: RecordId(issue(1)),
                    finding: None,
                    outcome: Outcome::Check {
                        transition: TransitionOutcome {
                            transition: "launch".into(),
                            passed: true,
                        },
                    },
                    rests_on: vec![],
                })
                .unwrap();
            let warnings = post_after(&ctx, &project, true);
            assert!(warnings.is_empty(), "{warnings:?}");
            assert_eq!(fake.issue(1).comments.len(), comments);
        }
    }

    // ⚠ A decision whose id fl does not write gets no comment — a marker
    // carrying it could close the HTML comment early — and a warning.
    #[test]
    fn a_decision_whose_id_fl_does_not_write_gets_a_warning_and_no_comment() {
        let fake = FakeGithub::start("acme/widgets");
        fake.plain_issue(&[], false);
        let client = Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        );
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(&dir.path().join("s.redb")).unwrap();
        let project = store.add_project("/r").unwrap();
        let repo = Repo {
            full_name: "acme/widgets".into(),
            node_id: "R_1".into(),
        };
        let gl = GithubLedger::new(&client, repo, &store);
        let flushes = Flushes::default();
        let witness = Witness::new(&flushes);
        let ctx = Ctx {
            store: &store,
            tracker: &store,
            ledger: &witness,
            handles: &store,
            github: None,
            github_ledger: Some(&gl),
            witness: Some(&witness),
            tracker_label: String::new(),
        };
        witness
            .flush(Decision {
                id: Iri::parse("urn:x:a--><b>").unwrap(),
                at: At::from_unix_millis(1),
                record: RecordId(issue(1)),
                finding: None,
                outcome: Outcome::Check {
                    transition: TransitionOutcome {
                        transition: "launch".into(),
                        passed: true,
                    },
                },
                rests_on: vec![],
            })
            .unwrap();
        let warnings = post_after(&ctx, &project, true);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("has an id fl does not write"),
            "{warnings:?}"
        );
        assert!(
            warnings[0].contains("fl github ledger verify"),
            "{warnings:?}"
        );
        assert!(
            !warnings[0].contains("fl github ledger comment"),
            "recovery skips it too"
        );
        assert!(fake.issue(1).comments.is_empty());
    }
}
