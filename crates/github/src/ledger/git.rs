//! The requests the GitHub ledger makes. Each answer is judged here, once,
//! so no caller reads a failure as data.

use super::GithubLedger;
use crate::client::Method;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fl_core::StoreError;
use serde_json::{Map, Value, json};

/// What a path on the branch is at one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Object {
    Tree(Vec<Entry>),
    Blob { oid: String },
}

/// One entry of a directory: its name, the object id, and whether it is a
/// file (`blob`) rather than a nested directory (`tree`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub name: String,
    pub oid: String,
    pub is_blob: bool,
}

const OBJECT_FIELDS: &str =
    "__typename ... on Tree { entries { name oid type } } ... on Blob { oid }";

/// ⚠ Modelled: `Commit.blame(path:)` names the commit that last changed
/// each range of lines. Confirmed by live test
/// `a_hand_edit_is_detected_and_named`.
const BLAME: &str = "query ledgerBlame($owner: String!, $name: String!, \
    $commit: GitObjectID!, $path: String!) { repository(owner: $owner, name: $name) { \
    object(oid: $commit) { ... on Commit { blame(path: $path) { \
    ranges { startingLine endingLine commit { oid } } } } } } }";

fn backend(msg: String) -> StoreError {
    StoreError::Backend(msg)
}

impl GithubLedger<'_> {
    pub(crate) fn path(&self, rest: &str) -> String {
        format!("/repos/{}{rest}", self.repo.full_name)
    }

    fn owner_and_name(&self) -> (&str, &str) {
        self.repo
            .full_name
            .split_once('/')
            .unwrap_or((self.repo.full_name.as_str(), ""))
    }

    /// The commit `branch` points at; `None` when there is no such branch.
    /// ⚠ An exact name: `fl` is not `fl/ledger`.
    pub(crate) fn branch_head(&self, branch: &str) -> Result<Option<String>, StoreError> {
        let r = self.client.send(
            Method::Get,
            &self.path(&format!("/git/ref/heads/{branch}")),
            None,
        )?;
        match r.status {
            200 => r
                .body
                .pointer("/object/sha")
                .and_then(Value::as_str)
                .map(|s| Some(s.to_string()))
                .ok_or_else(|| {
                    backend(format!(
                        "GitHub answered a read of the branch `{branch}` with no commit"
                    ))
                }),
            404 => Ok(None),
            s => Err(backend(format!(
                "GitHub answered {s} when fl read the branch `{branch}`; retry"
            ))),
        }
    }

    /// How `head` relates to `base` as GitHub's compare names it
    /// (`identical`, `ahead`, `behind`, `diverged`); `None` when GitHub
    /// knows one of the two commits not at all.
    pub(crate) fn compare(&self, base: &str, head: &str) -> Result<Option<String>, StoreError> {
        let r = self.client.send(
            Method::Get,
            &self.path(&format!("/compare/{base}...{head}")),
            None,
        )?;
        match r.status {
            200 => r
                .body
                .get("status")
                .and_then(Value::as_str)
                .map(|s| Some(s.to_string()))
                .ok_or_else(|| {
                    backend("GitHub compared two ledger commits but named no status".into())
                }),
            404 => Ok(None),
            s => Err(backend(format!(
                "GitHub answered {s} when fl compared two commits of the ledger; retry"
            ))),
        }
    }

    /// What each of `paths` is at commit `head`, in one request.
    ///
    /// ⚠ Ruling 24: the query also asks GraphQL whether it knows `head`
    /// itself, separately from each path. `check_head` already confirmed
    /// `head` is a real, non-rewritten commit over REST — but GitHub's
    /// GraphQL endpoint can be served by a replica that has not caught up
    /// with that commit yet, above all right after this machine's own
    /// write. A GraphQL answer that does not know `head` is therefore
    /// ambiguous, not evidence any path is missing: it is read again up to
    /// `lag_reads` times before a path's absence counts.
    pub(crate) fn objects(
        &self,
        head: &str,
        paths: &[String],
    ) -> Result<Vec<Option<Object>>, StoreError> {
        let (owner, name) = self.owner_and_name();
        let mut reads = 0u32;
        loop {
            let mut vars = Map::new();
            vars.insert("owner".into(), json!(owner));
            vars.insert("name".into(), json!(name));
            vars.insert("head".into(), json!(head));
            let mut declared = String::from("$owner: String!, $name: String!, $head: GitObjectID!");
            let mut fields = String::from(" head: object(oid: $head) { oid }");
            for (i, p) in paths.iter().enumerate() {
                declared.push_str(&format!(", $e{i}: String!"));
                fields.push_str(&format!(
                    " e{i}: object(expression: $e{i}) {{ {OBJECT_FIELDS} }}"
                ));
                vars.insert(format!("e{i}"), json!(format!("{head}:{p}")));
            }
            let query = format!(
                "query ledgerObjects({declared}) {{ repository(owner: $owner, name: $name) \
                 {{{fields} }} }}"
            );
            let data = self.client.graphql(&query, Value::Object(vars))?;
            let repo = data
                .get("repository")
                .filter(|r| !r.is_null())
                .ok_or_else(|| {
                    backend(format!(
                        "GitHub did not find the repository {} when fl read its ledger",
                        self.repo.full_name
                    ))
                })?;
            let commit_known = repo.get("head").is_some_and(|v| !v.is_null());
            if !commit_known {
                if reads < self.lag_reads {
                    reads += 1;
                    std::thread::sleep(self.lag_pause);
                    continue;
                }
                return Err(backend(format!(
                    "GitHub's GraphQL still does not know commit {head} of the ledger, still \
                     after {reads} reads again; this machine's own write may not have reached \
                     every replica yet. Retry"
                )));
            }
            return paths
                .iter()
                .enumerate()
                .map(|(i, p)| parse_object(&repo[format!("e{i}").as_str()], p))
                .collect();
        }
    }

    /// A blob's text, downloaded.
    ///
    /// ⚠ Lossy on purpose: bytes that are not UTF-8 become U+FFFD, so a
    /// damaged file fails the line and growth checks by name instead of
    /// failing here without one.
    pub(crate) fn blob_text(&self, oid: &str) -> Result<String, StoreError> {
        let r = self
            .client
            .send(Method::Get, &self.path(&format!("/git/blobs/{oid}")), None)?;
        if r.status != 200 {
            return Err(backend(format!(
                "GitHub answered {} when fl read ledger blob {oid}; retry",
                r.status
            )));
        }
        // GitHub wraps the base64 in lines.
        let content: String = r
            .body
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| backend(format!("GitHub answered ledger blob {oid} with no content")))?
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let bytes = STANDARD.decode(content).map_err(|e| {
            backend(format!(
                "GitHub answered ledger blob {oid} with content that is not base64 ({e})"
            ))
        })?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A ledger commit's parents, first parent first.
    pub(crate) fn parents_of(&self, sha: &str) -> Result<Vec<String>, StoreError> {
        let r = self.client.send(
            Method::Get,
            &self.path(&format!("/git/commits/{sha}")),
            None,
        )?;
        if r.status != 200 {
            return Err(backend(format!(
                "GitHub answered {} when fl read ledger commit {sha}; retry",
                r.status
            )));
        }
        let parents = r
            .body
            .get("parents")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                backend(format!(
                    "GitHub answered ledger commit {sha} with no parents"
                ))
            })?;
        Ok(parents
            .iter()
            .filter_map(|p| p.get("sha").and_then(Value::as_str).map(str::to_string))
            .collect())
    }

    /// The commit that last changed `line` of `path` at `commit`, for a
    /// message. ⚠ Never an error: a message that cannot name the commit
    /// says so, and still names the file and the line.
    pub(crate) fn blame(&self, commit: &str, path: &str, line: u64) -> String {
        let (owner, name) = self.owner_and_name();
        let found = self
            .client
            .graphql(
                BLAME,
                json!({"owner": owner, "name": name, "commit": commit, "path": path}),
            )
            .ok()
            .and_then(|d| {
                d.pointer("/repository/object/blame/ranges")?
                    .as_array()?
                    .iter()
                    .find(|r| {
                        let from = r["startingLine"].as_u64().unwrap_or(0);
                        let to = r["endingLine"].as_u64().unwrap_or(0);
                        from <= line && line <= to
                    })?
                    .pointer("/commit/oid")?
                    .as_str()
                    .map(str::to_string)
            });
        found.unwrap_or_else(|| format!("unknown (GitHub's blame of `{path}` did not name it)"))
    }
}

fn parse_object(v: &Value, path: &str) -> Result<Option<Object>, StoreError> {
    if v.is_null() {
        return Ok(None);
    }
    match v.get("__typename").and_then(Value::as_str) {
        Some("Blob") => {
            let oid = v.get("oid").and_then(Value::as_str).ok_or_else(|| {
                backend(format!(
                    "GitHub answered `{path}` on the ledger branch with no id"
                ))
            })?;
            Ok(Some(Object::Blob {
                oid: oid.to_string(),
            }))
        }
        Some("Tree") => {
            let entries = v.get("entries").and_then(Value::as_array).ok_or_else(|| {
                backend(format!(
                    "GitHub answered the directory `{path}` with no entries"
                ))
            })?;
            let mut out = Vec::new();
            for e in entries {
                let (Some(name), Some(oid)) = (e["name"].as_str(), e["oid"].as_str()) else {
                    return Err(backend(format!(
                        "GitHub answered an entry of `{path}` with no name or no id"
                    )));
                };
                out.push(Entry {
                    name: name.to_string(),
                    oid: oid.to_string(),
                    is_blob: e["type"].as_str() == Some("blob"),
                });
            }
            Ok(Some(Object::Tree(out)))
        }
        other => Err(backend(format!(
            "GitHub answered `{path}` on the ledger branch as {other:?}, which is neither a \
             file nor a directory"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;
    use crate::ledger::layout::{BRANCH, FORMAT_FILE};
    use crate::tracker::Repo;
    use fl_core::MemStore;
    use std::time::Duration;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    fn repo(name: &str) -> Repo {
        Repo {
            full_name: name.into(),
            node_id: "R_1".into(),
        }
    }

    /// No lag budget: every test here either needs none, or sets up its own
    /// knob count explicitly, so a stray retry never hides a red result.
    fn open<'a>(c: &'a Client, local: &'a MemStore) -> GithubLedger<'a> {
        GithubLedger::new(c, repo("acme/widgets"), local).with_lag(0, Duration::ZERO)
    }

    fn body_next(fake: &FakeGithub, frag: &str, status: u16, body: Value) {
        fake.state().body_next = Some((frag.into(), status, body));
    }

    // A 502 must not read as "the branch does not exist" (404's meaning) —
    // the two are different facts fl must tell apart.
    #[test]
    fn branch_head_treats_a_server_error_as_an_error_not_as_no_such_branch() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        fake.state().html_502_next = true;
        let err = open(&c, &local).branch_head(BRANCH).unwrap_err();
        assert!(err.to_string().contains("502"), "{err}");
    }

    #[test]
    fn compare_treats_a_server_error_as_an_error_not_as_an_unknown_commit() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        fake.state().html_502_next = true;
        let err = open(&c, &local).compare("a", "b").unwrap_err();
        assert!(err.to_string().contains("502"), "{err}");
    }

    #[test]
    fn blob_text_treats_a_server_error_as_an_error() {
        let fake = FakeGithub::start("acme/widgets");
        let local = MemStore::default();
        let c = client(&fake);
        fake.state().html_502_next = true;
        let err = open(&c, &local).blob_text("deadbeef").unwrap_err();
        assert!(err.to_string().contains("502"), "{err}");
    }

    // An unbound repository is an error fl must not mistake for "the format
    // file is missing" — the two are different facts, and conflating them
    // would turn "fl cannot see this repository at all" into "upgrade or
    // tamper" advice.
    #[test]
    fn objects_of_an_unbound_repository_is_an_error_not_a_missing_file() {
        let fake = FakeGithub::start("acme/widgets");
        let head = fake.seed_ledger();
        let local = MemStore::default();
        let c = client(&fake);
        let elsewhere =
            GithubLedger::new(&c, repo("acme/other"), &local).with_lag(0, Duration::ZERO);
        let err = elsewhere
            .objects(&head, &[FORMAT_FILE.to_string()])
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(ref m) if m.contains("acme/other")),
            "{err:?}"
        );
    }

    #[test]
    fn branch_head_refuses_a_200_with_no_commit() {
        let fake = FakeGithub::start("acme/widgets");
        fake.seed_ledger();
        let local = MemStore::default();
        let c = client(&fake);
        body_next(&fake, "/git/ref/heads/fl/ledger", 200, json!({}));
        let err = open(&c, &local).branch_head(BRANCH).unwrap_err();
        assert!(err.to_string().contains("no commit"), "{err}");
    }

    #[test]
    fn compare_refuses_a_200_with_no_status() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let local = MemStore::default();
        let c = client(&fake);
        body_next(&fake, "/compare/", 200, json!({}));
        let err = open(&c, &local).compare(&root, &root).unwrap_err();
        assert!(err.to_string().contains("no status"), "{err}");
    }

    #[test]
    fn blob_text_refuses_a_200_with_no_content() {
        let fake = FakeGithub::start("acme/widgets");
        let oid = fake.state().git.put_blob("x");
        let local = MemStore::default();
        let c = client(&fake);
        body_next(&fake, "/git/blobs/", 200, json!({"sha": oid}));
        let err = open(&c, &local).blob_text(&oid).unwrap_err();
        assert!(err.to_string().contains("no content"), "{err}");
    }

    #[test]
    fn blob_text_refuses_content_that_is_not_base64() {
        let fake = FakeGithub::start("acme/widgets");
        let oid = fake.state().git.put_blob("x");
        let local = MemStore::default();
        let c = client(&fake);
        body_next(
            &fake,
            "/git/blobs/",
            200,
            json!({"content": "not base64 !!!"}),
        );
        let err = open(&c, &local).blob_text(&oid).unwrap_err();
        assert!(err.to_string().contains("not base64"), "{err}");
    }

    #[test]
    fn objects_refuses_a_blob_with_no_oid() {
        let fake = FakeGithub::start("acme/widgets");
        let head = fake.seed_ledger();
        let local = MemStore::default();
        let c = client(&fake);
        body_next(
            &fake,
            "/graphql",
            200,
            json!({"data": {"repository": {
                "head": {"oid": head}, "e0": {"__typename": "Blob"},
            }}}),
        );
        let err = open(&c, &local)
            .objects(&head, &[FORMAT_FILE.to_string()])
            .unwrap_err();
        assert!(err.to_string().contains("no id"), "{err}");
    }

    // ⚠ A malformed directory answer must never read as an EMPTY directory:
    // "unreachable is not empty" (spec §2.5) applies here too — a directory
    // GitHub cannot describe properly is an error, not zero segments.
    #[test]
    fn objects_refuses_a_tree_with_no_entries() {
        let fake = FakeGithub::start("acme/widgets");
        let head = fake.seed_ledger();
        let local = MemStore::default();
        let c = client(&fake);
        body_next(
            &fake,
            "/graphql",
            200,
            json!({"data": {"repository": {
                "head": {"oid": head}, "e0": {"__typename": "Tree"},
            }}}),
        );
        let err = open(&c, &local)
            .objects(&head, &["runs/k".to_string()])
            .unwrap_err();
        assert!(err.to_string().contains("no entries"), "{err}");
    }

    #[test]
    fn objects_refuses_a_tree_entry_with_no_name_or_no_oid() {
        let fake = FakeGithub::start("acme/widgets");
        let head = fake.seed_ledger();
        let local = MemStore::default();
        let c = client(&fake);
        body_next(
            &fake,
            "/graphql",
            200,
            json!({"data": {"repository": {
                "head": {"oid": head},
                "e0": {
                    "__typename": "Tree",
                    "entries": [{"name": "1.jsonl", "type": "blob"}],
                },
            }}}),
        );
        let err = open(&c, &local)
            .objects(&head, &["runs/k".to_string()])
            .unwrap_err();
        assert!(err.to_string().contains("no name or no id"), "{err}");
    }

    #[test]
    fn objects_refuses_an_unknown_object_type() {
        let fake = FakeGithub::start("acme/widgets");
        let head = fake.seed_ledger();
        let local = MemStore::default();
        let c = client(&fake);
        body_next(
            &fake,
            "/graphql",
            200,
            json!({"data": {"repository": {
                "head": {"oid": head}, "e0": {"__typename": "Submodule"},
            }}}),
        );
        let err = open(&c, &local)
            .objects(&head, &[FORMAT_FILE.to_string()])
            .unwrap_err();
        assert!(
            err.to_string().contains("neither a file nor a directory"),
            "{err}"
        );
    }
}
