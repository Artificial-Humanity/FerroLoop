//! Decision comments on GitHub (spec §4.1, §4.3).

use super::GithubLedger;
use super::render;
use crate::client::Method;
use crate::meta::{self, IssueView};
use fl_core::StoreError;
use fl_core::ids::{ProjectId, RecordId};
use fl_core::iri::Iri;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Where an item's issue is now, and the project fl's block there names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueAt {
    /// Its comments: a path under the API, or under the address GitHub
    /// gave for an issue that moved.
    pub comments: String,
    pub project: ProjectId,
    /// Where the issue is now, when it was transferred.
    pub moved_to: Option<Iri>,
}

fn backend(msg: String) -> StoreError {
    StoreError::Backend(msg)
}

impl GithubLedger<'_> {
    /// Who the ledger's lines say wrote them: the credential's identity,
    /// read once per command.
    pub fn by(&self) -> Result<String, StoreError> {
        self.identity()
    }

    /// The number of `item`'s issue in this repository. ⚠ Local: an item
    /// another repository holds, or an IRI that is no issue's URL, is
    /// refused before any request.
    fn number_of(&self, item: &Iri) -> Result<u64, StoreError> {
        let elsewhere = || StoreError::NotOwned {
            id: item.clone(),
            searched: vec![format!("the issues of {}", self.repo.full_name)],
        };
        if !self.owns(&RecordId(item.clone()))? {
            return Err(elsewhere());
        }
        meta::parse_issue_url(item)
            .map(|(_, n)| n)
            .ok_or_else(elsewhere)
    }

    /// Post `body` on `item`'s issue in this repository: the comment a
    /// decision posts once its state change is done (spec §4.1).
    pub fn post_comment(&self, item: &Iri, body: &str) -> Result<(), StoreError> {
        let n = self.number_of(item)?;
        self.post(&self.path(&format!("/issues/{n}/comments")), body)
    }

    /// Where `item`'s issue is now (spec §4.1: "A transferred issue gets it
    /// at its current location"), and the project fl's block there names —
    /// which also proves the issue is an fl item.
    ///
    /// ⚠ A moved issue is followed once, to the address GitHub gives,
    /// which the client refuses unless it is under the API: the credential
    /// goes along. An answer that names no address leaves an empty one,
    /// refused the same way.
    pub fn issue_at(&self, item: &Iri) -> Result<IssueAt, StoreError> {
        let n = self.number_of(item)?;
        let here = self.path(&format!("/issues/{n}"));
        let r = self.client.send(Method::Get, &here, None)?;
        let (r, base, moved) = match r.status {
            200 => (r, here, false),
            301 | 302 | 307 | 308 => {
                let to = r.location.clone().unwrap_or_default();
                let again = self.client.send(Method::Get, &to, None)?;
                if again.status != 200 {
                    return Err(backend(format!(
                        "GitHub answered {} when fl read issue {n} where it moved, {to}",
                        again.status
                    )));
                }
                (again, to, true)
            }
            404 => {
                return Err(StoreError::NotAnFlItem {
                    id: item.clone(),
                    what: "an issue that does not exist".into(),
                });
            }
            410 => return Err(StoreError::Deleted(item.clone())),
            s => return Err(self.read_refused(s, &format!("read issue {n}"))),
        };
        let issue = IssueView::from_json(&r.body)?;
        if issue.is_pull_request {
            return Err(StoreError::NotAnFlItem {
                id: item.clone(),
                what: "a pull request".into(),
            });
        }
        let (_, block) = meta::parse_body(&issue.body).map_err(|e| StoreError::NotAnFlItem {
            id: item.clone(),
            what: format!("an issue without fl's block ({e})"),
        })?;
        Ok(IssueAt {
            comments: format!("{base}/comments"),
            project: block.project,
            moved_to: moved.then_some(issue.url),
        })
    }

    /// Every decision the comments at `at` mark, over every page (spec
    /// §4.3). ⚠ A page that fails fails the read — never "none posted",
    /// which would post every decision again.
    ///
    /// ⚠ Only a trusted author's comment marks a decision: the login fl
    /// posts as (read once per command), or one of `writers` — the `by` of
    /// every decision line filed under the item, each of whom held
    /// Contents: write. Anyone else who can comment could otherwise copy a
    /// decision id from the ledger and stop its comment being recovered.
    /// An edit keeps the author, so a comment someone edits still counts;
    /// its first marker line counts wherever it is.
    pub fn posted(
        &self,
        at: &IssueAt,
        writers: &BTreeSet<String>,
    ) -> Result<BTreeSet<Iri>, StoreError> {
        let mut trusted = writers.clone();
        trusted.insert(self.by()?);
        let mut out = BTreeSet::new();
        for c in self
            .client
            .get_all(&format!("{}?per_page=100", at.comments))?
        {
            let text = match c.get("body") {
                None => {
                    return Err(backend("GitHub listed a comment without a body".into()));
                }
                // An empty comment marks nothing.
                Some(Value::Null) => continue,
                Some(body) => body.as_str().ok_or_else(|| {
                    backend("GitHub listed a comment whose body is not text".into())
                })?,
            };
            // A comment with no author is no one's, and never trusted.
            let author = c.pointer("/user/login").and_then(Value::as_str);
            if !author.is_some_and(|a| trusted.contains(a)) {
                continue;
            }
            if let Some(id) = render::marked(text) {
                out.insert(id);
            }
        }
        Ok(out)
    }

    /// Post `body` where the issue is now.
    pub fn post_at(&self, at: &IssueAt, body: &str) -> Result<(), StoreError> {
        self.post(&at.comments, body)
    }

    fn post(&self, comments: &str, body: &str) -> Result<(), StoreError> {
        let r = self
            .client
            .send(Method::Post, comments, Some(&json!({ "body": body })))?;
        match r.status {
            201 => Ok(()),
            s => Err(backend(format!(
                "GitHub answered {s} when fl posted a decision comment"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;
    use crate::fake::USER_LOGIN;
    use crate::ledger::{GithubLedger, render};
    use crate::tracker::{GithubTracker, Repo};
    use fl_core::MemStore;
    use fl_core::StoreError;
    use fl_core::ids::{ProjectId, seq_iri};
    use fl_core::iri::Iri;
    use fl_core::store::Tracker;
    use serde_json::json;
    use std::collections::BTreeSet;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    fn repo() -> Repo {
        Repo {
            full_name: "acme/widgets".into(),
            node_id: "R_1".into(),
        }
    }

    fn issue(n: u64) -> Iri {
        Iri::parse(&format!("https://github.com/acme/widgets/issues/{n}")).unwrap()
    }

    fn project() -> ProjectId {
        ProjectId(seq_iri(2))
    }

    /// A fake holding one fl record, #1, made by the tracker as fl makes
    /// one.
    fn with_record() -> FakeGithub {
        let fake = FakeGithub::start("acme/widgets");
        let memory = MemStore::default();
        let (t, _) = GithubTracker::open(client(&fake), "acme/widgets", &memory).unwrap();
        t.add_record(&project(), "work").unwrap();
        fake
    }

    #[test]
    fn a_live_comment_is_posted_on_the_items_issue() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        l.post_comment(&issue(1), "hello").unwrap();
        assert_eq!(fake.issue(1).comments, vec!["hello".to_string()]);
    }

    // ⚠ Ownership is local: an item another repository holds, or an IRI
    // that is no issue's URL, is refused before any request.
    #[test]
    fn an_item_this_repository_does_not_hold_is_refused_before_any_request() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        fake.state().requests.clear();
        let other = Iri::parse("https://github.com/acme/other/issues/1").unwrap();
        assert!(matches!(
            l.post_comment(&other, "x"),
            Err(StoreError::NotOwned { .. })
        ));
        assert!(matches!(
            l.issue_at(&seq_iri(5)),
            Err(StoreError::NotOwned { .. })
        ));
        assert!(fake.state().requests.is_empty());
    }

    #[test]
    fn a_comment_github_does_not_take_is_an_error() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        fake.state().fail_comment_next = true;
        let err = l.post_comment(&issue(1), "x").unwrap_err().to_string();
        assert!(err.contains("when fl posted a decision comment"), "{err}");
        assert!(fake.issue(1).comments.is_empty());
    }

    #[test]
    fn issue_at_reads_where_the_issue_is_and_what_its_block_says() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        assert_eq!(
            l.issue_at(&issue(1)).unwrap(),
            IssueAt {
                comments: "/repos/acme/widgets/issues/1/comments".into(),
                project: project(),
                moved_to: None,
            }
        );
    }

    // Spec §4.1: a transferred issue gets its comment where it is now.
    #[test]
    fn a_transferred_issue_is_followed_to_where_it_is_now() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        let before = l.issue_at(&issue(1)).unwrap();
        let to = fake.transfer(1);
        let at = l.issue_at(&issue(1)).unwrap();
        assert_eq!(at.comments, format!("{to}/comments"));
        assert_eq!(
            at.moved_to,
            Some(Iri::parse("https://github.com/elsewhere/transferred/issues/1").unwrap())
        );
        assert_eq!(at.project, project());
        l.post_at(&at, "where it is").unwrap();
        assert_eq!(
            fake.state().transferred[&1].comments,
            vec!["where it is".to_string()]
        );
        assert!(
            fake.issue(1).comments.is_empty(),
            "nothing at the old address"
        );
        assert!(
            l.posted(&at, &none()).unwrap().is_empty(),
            "no marker in a plain comment"
        );
        // The old address answers with where the issue went, never a list.
        assert!(l.posted(&before, &none()).is_err());
        assert!(l.post_comment(&issue(1), "late").is_err());
        // Gone from where it moved: said, naming the move.
        fake.state().transferred.remove(&1);
        let err = l.issue_at(&issue(1)).unwrap_err().to_string();
        assert!(err.contains("when fl read issue 1 where it moved"), "{err}");
    }

    #[test]
    fn issue_at_refuses_what_is_not_an_fl_issue_where_it_is() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        let pull = fake.plain_issue(&["fl:record"], true);
        let plain = fake.plain_issue(&[], false);
        let what = |n: u64| match l.issue_at(&issue(n)) {
            Err(StoreError::NotAnFlItem { what, .. }) => what,
            other => panic!("expected not an fl item, got {other:?}"),
        };
        assert_eq!(what(pull), "a pull request");
        assert!(
            what(plain).starts_with("an issue without fl's block"),
            "{}",
            what(plain)
        );
        assert_eq!(what(99), "an issue that does not exist");
        // ⚠ A move is followed only under the API: the credential goes
        // along.
        fake.state().issues.get_mut(&plain).unwrap().moved_to =
            Some("https://elsewhere.example/repositories/1/issues/1".into());
        let err = l.issue_at(&issue(plain)).unwrap_err().to_string();
        assert!(
            err.contains("refused to send the GitHub credential"),
            "{err}"
        );
        let here = l.issue_at(&issue(1)).unwrap();
        fake.state().issues.get_mut(&1).unwrap().gone = true;
        assert!(matches!(l.issue_at(&issue(1)), Err(StoreError::Deleted(_))));
        assert!(
            l.post_comment(&issue(1), "x").is_err(),
            "nothing posted on a deleted issue"
        );
        assert!(
            l.posted(&here, &none()).is_err(),
            "a deleted issue lists no comments"
        );
    }

    // A server error reading the issue says nothing lasting: transient.
    #[test]
    fn a_server_error_reading_the_issue_is_transient() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        fake.state().body_next.push((
            "/repos/acme/widgets/issues/1".into(),
            502,
            serde_json::Value::Null,
        ));
        let err = l.issue_at(&issue(1)).unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
    }

    fn mark(n: u64) -> String {
        render::marker(&seq_iri(n)).expect("an id fl writes")
    }

    /// No decision writer named: only fl's own login is trusted.
    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    // ⚠ Spec §4.3: every page; each comment fl wrote counts by its first
    // marker line, even one a maintainer's edit pushed down; anyone
    // else's marks nothing. fl's login is read once.
    #[test]
    fn posted_reads_every_page_and_the_first_marker_of_fls_own_comments() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        {
            let mut s = fake.state();
            let i = s.issues.get_mut(&1).unwrap();
            i.comments = vec![
                format!("{}\n\nfirst", mark(10)),
                "thanks!".into(),
                format!("{}\n\nforged", mark(12)),
                format!("A maintainer's note.\n\n{}\n\nsecond", mark(11)),
            ];
            i.comment_authors = vec![
                USER_LOGIN.into(),
                "a-reviewer".into(),
                "a-reviewer".into(),
                USER_LOGIN.into(),
            ];
            s.max_per_page = 1;
        }
        let at = l.issue_at(&issue(1)).unwrap();
        fake.state().requests.clear();
        assert_eq!(
            l.posted(&at, &none()).unwrap(),
            BTreeSet::from([seq_iri(10), seq_iri(11)])
        );
        assert_eq!(l.posted(&at, &none()).unwrap().len(), 2);
        let requests = fake.state().requests.clone();
        let pages = requests
            .iter()
            .filter(|r| r.contains("/issues/1/comments"))
            .count();
        assert_eq!(pages, 8, "every page, twice: {requests:#?}");
        assert_eq!(
            requests.iter().filter(|r| *r == "GET /user").count(),
            1,
            "fl's login, read once: {requests:#?}"
        );
    }

    // What fl posts, fl wrote: its marker counts on the next read.
    #[test]
    fn a_comment_fl_posts_is_one_it_wrote() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        l.post_comment(&issue(1), &format!("{}\n\nbody", mark(10)))
            .unwrap();
        let at = l.issue_at(&issue(1)).unwrap();
        assert_eq!(
            l.posted(&at, &none()).unwrap(),
            BTreeSet::from([seq_iri(10)])
        );
    }

    // ⚠ A comment by another account that wrote a decision under the item
    // counts — a colleague's machine posted it — so recovery does not post
    // it again; the same comment by an account that wrote none does not.
    #[test]
    fn a_marker_by_an_account_that_wrote_a_decision_under_the_item_counts() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        {
            let mut s = fake.state();
            let i = s.issues.get_mut(&1).unwrap();
            i.comments = vec![mark(10)];
            i.comment_authors = vec!["colleague".into()];
        }
        let at = l.issue_at(&issue(1)).unwrap();
        let writers = BTreeSet::from(["colleague".to_string()]);
        assert_eq!(
            l.posted(&at, &writers).unwrap(),
            BTreeSet::from([seq_iri(10)])
        );
        let others = BTreeSet::from(["someone-else".to_string()]);
        assert!(
            l.posted(&at, &others).unwrap().is_empty(),
            "wrote nothing here"
        );
    }

    // ⚠ A comment someone else wrote cannot suppress recovery, whatever it
    // carries; a listed comment with no author is no one's.
    #[test]
    fn a_marker_in_a_comment_fl_did_not_write_does_not_count() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        {
            let mut s = fake.state();
            let i = s.issues.get_mut(&1).unwrap();
            i.comments = vec![mark(10)];
            i.comment_authors = vec!["someone-else".into()];
        }
        let at = l.issue_at(&issue(1)).unwrap();
        assert!(l.posted(&at, &none()).unwrap().is_empty());
        fake.state().issues.get_mut(&1).unwrap().comment_authors = vec![USER_LOGIN.into()];
        assert_eq!(
            l.posted(&at, &none()).unwrap(),
            BTreeSet::from([seq_iri(10)]),
            "fl's own counts"
        );
        fake.state().body_next.push((
            "/issues/1/comments".into(),
            200,
            json!([{"id": 1, "body": mark(10)}]),
        ));
        assert!(
            l.posted(&at, &none()).unwrap().is_empty(),
            "no author, no one's"
        );
    }

    #[test]
    fn a_page_that_cannot_be_read_fails_the_read() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        {
            let mut s = fake.state();
            s.issues.get_mut(&1).unwrap().comments = vec!["a".into(), "b".into(), "c".into()];
            s.max_per_page = 1;
            s.fail_page = Some(("/repos/acme/widgets/issues/1/comments".into(), 2));
        }
        let at = l.issue_at(&issue(1)).unwrap();
        assert!(
            l.posted(&at, &none()).is_err(),
            "a missing page is not an empty one"
        );
    }

    #[test]
    fn a_listed_comment_without_a_body_is_an_error_and_an_empty_one_marks_nothing() {
        let fake = with_record();
        let (c, local) = (client(&fake), MemStore::default());
        let l = GithubLedger::new(&c, repo(), &local);
        let at = l.issue_at(&issue(1)).unwrap();
        fake.state()
            .body_next
            .push(("/issues/1/comments".into(), 200, json!([{"id": 1}])));
        assert!(l.posted(&at, &none()).is_err());
        fake.state().body_next.push((
            "/issues/1/comments".into(),
            200,
            json!([{"id": 1, "body": null}]),
        ));
        assert!(l.posted(&at, &none()).unwrap().is_empty());
        fake.state().body_next.push((
            "/issues/1/comments".into(),
            200,
            json!([{"id": 1, "body": 5, "user": {"login": USER_LOGIN}}]),
        ));
        assert!(l.posted(&at, &none()).is_err(), "a body that is not text");
    }
}
