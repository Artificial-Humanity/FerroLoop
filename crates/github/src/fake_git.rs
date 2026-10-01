//! The fake's git objects, and the REST endpoints the GitHub ledger uses
//! (GitHub ledger spec §8.1): refs, trees, commits, blobs, compare, and the
//! rules on a branch.
//!
//! ⚠ It proves structure, not integration: object ids are SHA-256 based,
//! not git's, and each answer's shape is fl's reading of GitHub's
//! documentation. A shape no live test confirms yet says so.

use crate::fake::{Answer, State, answer};
use crate::ledger::layout::BRANCH;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// One commit: its tree, its parents (first parent first), its message.
#[derive(Debug, Clone)]
pub struct FakeCommit {
    pub tree: String,
    pub parents: Vec<String>,
    pub message: String,
}

/// A ruleset on one branch. Only an `active` one applies.
#[derive(Debug, Clone)]
pub struct Ruleset {
    pub enforcement: String,
    pub branch: String,
    pub rules: Vec<String>,
}

impl Ruleset {
    /// A ruleset on `fl/ledger`, enforced as `enforcement` (`active`,
    /// `disabled` or `evaluate`), with `rules`.
    pub fn on_ledger(enforcement: &str, rules: &[&str]) -> Ruleset {
        Ruleset {
            enforcement: enforcement.into(),
            branch: BRANCH.into(),
            rules: rules.iter().map(|r| r.to_string()).collect(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Git {
    /// blob id → text.
    pub blobs: BTreeMap<String, String>,
    /// tree id → every file of the tree, path → blob id.
    pub trees: BTreeMap<String, BTreeMap<String, String>>,
    pub commits: BTreeMap<String, FakeCommit>,
    /// `heads/<branch>` → commit id.
    pub refs: BTreeMap<String, String>,
    clock: u64,
}

/// A 40-hex id, shaped like git's, from `parts`.
fn object_id(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    h.finalize()
        .iter()
        .take(20)
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Git {
    pub fn put_blob(&mut self, text: &str) -> String {
        let id = object_id(&["blob", text]);
        self.blobs.insert(id.clone(), text.to_string());
        id
    }

    /// A tree holding `files` (path → text).
    pub fn put_tree(&mut self, files: &BTreeMap<String, String>) -> String {
        let entries: BTreeMap<String, String> = files
            .iter()
            .map(|(p, t)| (p.clone(), self.put_blob(t)))
            .collect();
        let listing: Vec<String> = entries.iter().map(|(p, b)| format!("{p} {b}")).collect();
        let id = object_id(&["tree", &listing.join("\n")]);
        self.trees.insert(id.clone(), entries);
        id
    }

    pub fn put_commit(&mut self, tree: &str, parents: Vec<String>, message: &str) -> String {
        self.clock += 1;
        let id = object_id(&[
            "commit",
            tree,
            &parents.join(" "),
            message,
            &self.clock.to_string(),
        ]);
        self.commits.insert(
            id.clone(),
            FakeCommit {
                tree: tree.to_string(),
                parents,
                message: message.to_string(),
            },
        );
        id
    }

    pub fn head(&self, branch: &str) -> Option<String> {
        self.refs.get(&format!("heads/{branch}")).cloned()
    }

    /// Every file of `commit`, path → text.
    pub fn files_at(&self, commit: &str) -> Option<BTreeMap<String, String>> {
        let tree = self.trees.get(&self.commits.get(commit)?.tree)?;
        Some(
            tree.iter()
                .map(|(p, b)| (p.clone(), self.blobs[b].clone()))
                .collect(),
        )
    }

    /// Whether `a` is `b` or one of its ancestors.
    pub fn is_ancestor(&self, a: &str, b: &str) -> bool {
        let mut todo = vec![b.to_string()];
        let mut seen = BTreeSet::new();
        while let Some(c) = todo.pop() {
            if c == a {
                return true;
            }
            if seen.insert(c.clone())
                && let Some(commit) = self.commits.get(&c)
            {
                todo.extend(commit.parents.iter().cloned());
            }
        }
        false
    }

    /// A commit on top of `branch`'s head writing (`Some`) or removing
    /// (`None`) each path; moves the branch, creating it if absent.
    pub fn commit_on(
        &mut self,
        branch: &str,
        changes: &[(String, Option<String>)],
        message: &str,
    ) -> String {
        let head = self.head(branch);
        let mut files = head
            .as_deref()
            .and_then(|h| self.files_at(h))
            .unwrap_or_default();
        for (path, text) in changes {
            match text {
                Some(t) => {
                    files.insert(path.clone(), t.clone());
                }
                None => {
                    files.remove(path);
                }
            }
        }
        let tree = self.put_tree(&files);
        let id = self.put_commit(&tree, head.into_iter().collect(), message);
        self.refs.insert(format!("heads/{branch}"), id.clone());
        id
    }

    /// The commits reachable from `branch`'s head along first parents,
    /// newest first.
    pub fn first_parents(&self, branch: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut at = self.head(branch);
        while let Some(c) = at {
            at = self
                .commits
                .get(&c)
                .and_then(|k| k.parents.first().cloned());
            out.push(c);
        }
        out
    }
}

/// A first commit holding `files`, with no parent, forced onto `fl/ledger`.
pub fn seed(git: &mut Git, files: &[(&str, &str)]) -> String {
    let files: BTreeMap<String, String> = files
        .iter()
        .map(|(p, t)| (p.to_string(), t.to_string()))
        .collect();
    let tree = git.put_tree(&files);
    let id = git.put_commit(&tree, vec![], "fl: start the ledger");
    git.refs.insert(format!("heads/{BRANCH}"), id.clone());
    id
}

fn not_found() -> Answer {
    answer(404, json!({"message": "Not Found"}))
}

fn unprocessable(message: &str) -> Answer {
    answer(422, json!({"message": message}))
}

fn commit_json(sha: &str, c: &FakeCommit) -> Value {
    json!({
        "sha": sha,
        "tree": {"sha": c.tree},
        "parents": c.parents.iter().map(|p| json!({"sha": p})).collect::<Vec<_>>(),
        "message": c.message,
    })
}

/// Base64 in lines of 60, as GitHub's blob API sends it.
fn wrapped(text: &str) -> String {
    let b64 = STANDARD.encode(text.as_bytes());
    let mut out = String::new();
    for chunk in b64.as_bytes().chunks(60) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push('\n');
    }
    out
}

/// The ledger's REST endpoints; `None` for every other route.
pub(crate) fn rest(s: &mut State, method: &str, parts: &[&str], body: &str) -> Option<Answer> {
    let ["repos", o, r, rest @ ..] = parts else {
        return None;
    };
    if !s.is_bound(o, r) {
        return None;
    }
    let full = format!("{o}/{r}");
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    Some(match (method, rest) {
        ("GET", ["git", "ref", name @ ..]) => {
            let name = name.join("/");
            let head = s.git.refs.get(&name).cloned();
            let lagging = s.ref_behind_next > 0;
            let sha = match head {
                // A replica that has not seen the last write: the commit
                // before the head.
                Some(h) if lagging => {
                    s.ref_behind_next -= 1;
                    s.git
                        .commits
                        .get(&h)
                        .and_then(|c| c.parents.first().cloned())
                        .or(Some(h))
                }
                other => other,
            };
            match sha {
                Some(sha) => answer(
                    200,
                    json!({
                        "ref": format!("refs/{name}"),
                        "object": {"sha": sha, "type": "commit"},
                    }),
                ),
                None => not_found(),
            }
        }
        ("POST", ["git", "refs"]) => create_ref(s, &v),
        ("POST", ["git", "trees"]) => create_tree(s, &v),
        ("POST", ["git", "commits"]) => create_commit(s, &v),
        ("GET", ["git", "commits", sha]) => match s.git.commits.get(*sha) {
            Some(c) => answer(200, commit_json(sha, c)),
            None => not_found(),
        },
        ("GET", ["git", "trees", sha]) => get_tree(s, sha),
        ("GET", ["git", "blobs", sha]) => match s.git.blobs.get(*sha) {
            Some(t) => answer(
                200,
                json!({"sha": sha, "size": t.len(), "encoding": "base64", "content": wrapped(t)}),
            ),
            None => not_found(),
        },
        ("GET", ["compare", spec]) => compare(s, spec),
        ("GET", ["rules", "branches", name @ ..]) => rules(s, &full, &name.join("/")),
        _ => return None,
    })
}

/// ⚠ Modelled: git keeps a branch as a file, so `fl` and `fl/ledger`
/// cannot both exist, and GitHub refuses the second with a 422. Confirmed by
/// live test `init_sets_up_a_ledger_on_a_private_repository`.
fn create_ref(s: &mut State, v: &Value) -> Answer {
    if std::mem::take(&mut s.fail_next_ref_create) {
        return answer(
            500,
            json!({"message": "fake failure before the ref was created"}),
        );
    }
    let (Some(full), Some(sha)) = (v["ref"].as_str(), v["sha"].as_str()) else {
        return unprocessable("ref and sha are required");
    };
    let Some(name) = full.strip_prefix("refs/") else {
        return unprocessable("Reference name must start with 'refs/'");
    };
    if std::mem::take(&mut s.race_next_ref_create) {
        // Someone else creates the same branch a moment earlier.
        let tree = s.git.put_tree(&BTreeMap::new());
        let theirs = s.git.put_commit(&tree, vec![], "someone else's start");
        s.git.refs.insert(name.to_string(), theirs);
    }
    if s.git.refs.contains_key(name) {
        return unprocessable("Reference already exists");
    }
    let collides = s
        .git
        .refs
        .keys()
        .any(|r| r.starts_with(&format!("{name}/")) || name.starts_with(&format!("{r}/")));
    if collides {
        return unprocessable(&format!("'{full}' conflicts with an existing ref"));
    }
    if !s.git.commits.contains_key(sha) {
        return unprocessable("Object does not exist");
    }
    s.git.refs.insert(name.to_string(), sha.to_string());
    answer(
        201,
        json!({"ref": full, "object": {"sha": sha, "type": "commit"}}),
    )
}

fn create_tree(s: &mut State, v: &Value) -> Answer {
    if std::mem::take(&mut s.fail_next_git_create) {
        return answer(500, json!({"message": "fake failure: nothing was created"}));
    }
    if !v["base_tree"].is_null() {
        return unprocessable("the fake builds trees from scratch only");
    }
    let Some(entries) = v["tree"].as_array() else {
        return unprocessable("tree is required");
    };
    let mut files = BTreeMap::new();
    for e in entries {
        match (
            e["path"].as_str(),
            e["type"].as_str(),
            e["content"].as_str(),
        ) {
            (Some(p), Some("blob"), Some(c)) => {
                files.insert(p.to_string(), c.to_string());
            }
            _ => return unprocessable("the fake takes only blobs given by content"),
        }
    }
    let sha = s.git.put_tree(&files);
    answer(201, json!({"sha": sha}))
}

fn create_commit(s: &mut State, v: &Value) -> Answer {
    if std::mem::take(&mut s.fail_next_git_create) {
        return answer(500, json!({"message": "fake failure: nothing was created"}));
    }
    let (Some(tree), Some(message)) = (v["tree"].as_str(), v["message"].as_str()) else {
        return unprocessable("tree and message are required");
    };
    if !s.git.trees.contains_key(tree) {
        return unprocessable("Tree SHA does not exist");
    }
    let parents: Vec<String> = v["parents"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if parents.iter().any(|p| !s.git.commits.contains_key(p)) {
        return unprocessable("Parent SHA does not exist");
    }
    let sha = s.git.put_commit(tree, parents, message);
    answer(201, commit_json(&sha, &s.git.commits[&sha]))
}

fn get_tree(s: &State, sha: &str) -> Answer {
    let Some(files) = s.git.trees.get(sha) else {
        return not_found();
    };
    let mut dirs = BTreeSet::new();
    let mut items = Vec::new();
    for (path, blob) in files {
        let mut at = path.as_str();
        while let Some((parent, _)) = at.rsplit_once('/') {
            dirs.insert(parent.to_string());
            at = parent;
        }
        items.push(json!({
            "path": path, "mode": "100644", "type": "blob", "sha": blob,
            "size": s.git.blobs[blob].len(),
        }));
    }
    for d in dirs {
        items.push(json!({
            "path": d, "mode": "040000", "type": "tree",
            "sha": object_id(&["subtree", sha, &d]),
        }));
    }
    answer(200, json!({"sha": sha, "tree": items, "truncated": false}))
}

/// ⚠ Modelled: `GET /compare/{base}...{head}` names `status` as
/// `identical`, `ahead`, `behind` or `diverged`, and answers 404 for a
/// commit it does not hold. Confirmed by live test
/// `a_hand_edit_is_detected_and_named`.
fn compare(s: &mut State, spec: &str) -> Answer {
    let Some((base, head)) = spec.split_once("...") else {
        return not_found();
    };
    if s.compare_unknown_next > 0 {
        s.compare_unknown_next -= 1;
        return not_found();
    }
    if !s.git.commits.contains_key(base) || !s.git.commits.contains_key(head) {
        return not_found();
    }
    let status = if s.compare_behind_next > 0 {
        s.compare_behind_next -= 1;
        "behind"
    } else if base == head {
        "identical"
    } else if s.git.is_ancestor(base, head) {
        "ahead"
    } else if s.git.is_ancestor(head, base) {
        "behind"
    } else {
        "diverged"
    };
    answer(200, json!({"status": status}))
}

/// ⚠ Modelled: `GET /rules/branches/{branch}` lists the rules IN FORCE on
/// the branch — a disabled or evaluate-only ruleset contributes none — and
/// a plan without rulesets answers 403 with an upgrade message. Confirmed
/// by live tests `rules_on_the_ledger_branch_are_readable` and
/// `a_private_repository_without_a_ruleset_is_detection_only`.
fn rules(s: &mut State, full: &str, branch: &str) -> Answer {
    if std::mem::take(&mut s.fail_rules_next) {
        return answer(500, json!({"message": "fake rules failure"}));
    }
    if s.rules_need_upgrade {
        return answer(
            403,
            json!({"message": "Upgrade to GitHub Pro or make this repository public to enable \
                               this feature."}),
        );
    }
    let items: Vec<Value> = s
        .rulesets
        .iter()
        .enumerate()
        .filter(|(_, r)| r.enforcement == "active" && r.branch == branch)
        .flat_map(|(i, r)| {
            r.rules.iter().map(move |rule| {
                json!({
                    "type": rule, "ruleset_source_type": "Repository",
                    "ruleset_source": full, "ruleset_id": i + 1,
                })
            })
        })
        .collect();
    answer(200, Value::Array(items))
}

#[cfg(test)]
mod tests {
    use super::Ruleset;
    use crate::client::{Client, Method};
    use crate::creds::EnvToken;
    use crate::fake::FakeGithub;
    use fl_core::StoreError;
    use serde_json::json;

    fn client(fake: &FakeGithub) -> Client {
        Client::new(
            &fake.url(),
            Box::new(EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
        )
    }

    const REPO: &str = "/repos/acme/widgets";

    #[test]
    fn a_branch_is_built_from_a_tree_a_commit_and_a_ref_and_read_back() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        let tree = c
            .send(
                Method::Post,
                &format!("{REPO}/git/trees"),
                Some(&json!({"tree": [
                    {"path": "format", "mode": "100644", "type": "blob", "content": "1\n"},
                ]})),
            )
            .unwrap();
        assert_eq!(tree.status, 201);
        let commit = c
            .send(
                Method::Post,
                &format!("{REPO}/git/commits"),
                Some(&json!({"message": "m", "tree": tree.body["sha"], "parents": []})),
            )
            .unwrap();
        let sha = commit.body["sha"].as_str().unwrap().to_string();
        assert_eq!(sha.len(), 40, "shaped like a git id");
        let made = c
            .send(
                Method::Post,
                &format!("{REPO}/git/refs"),
                Some(&json!({"ref": "refs/heads/fl/ledger", "sha": sha})),
            )
            .unwrap();
        assert_eq!(made.status, 201);
        let read = c
            .send(
                Method::Get,
                &format!("{REPO}/git/ref/heads/fl/ledger"),
                None,
            )
            .unwrap();
        assert_eq!(read.body["object"]["sha"], json!(sha));
        assert_eq!(
            fake.ledger_files().get("format").map(String::as_str),
            Some("1\n")
        );
        let got = c
            .send(Method::Get, &format!("{REPO}/git/commits/{sha}"), None)
            .unwrap();
        assert_eq!(got.body["parents"], json!([]));
        assert_eq!(
            c.send(Method::Get, &format!("{REPO}/git/ref/heads/fl"), None)
                .unwrap()
                .status,
            404,
            "an exact name, not a prefix"
        );
    }

    #[test]
    fn a_ref_that_exists_or_collides_with_another_is_refused() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let c = client(&fake);
        let again = c
            .send(
                Method::Post,
                &format!("{REPO}/git/refs"),
                Some(&json!({"ref": "refs/heads/fl/ledger", "sha": root})),
            )
            .unwrap_err();
        assert!(again.to_string().contains("already exists"), "{again}");
        let parent = c
            .send(
                Method::Post,
                &format!("{REPO}/git/refs"),
                Some(&json!({"ref": "refs/heads/fl", "sha": root})),
            )
            .unwrap_err();
        assert!(parent.to_string().contains("conflicts"), "{parent}");
    }

    #[test]
    fn a_blob_comes_back_as_wrapped_base64() {
        use base64::Engine;
        let fake = FakeGithub::start("acme/widgets");
        let text = "x".repeat(200);
        let oid = fake.state().git.put_blob(&text);
        let b = client(&fake)
            .send(Method::Get, &format!("{REPO}/git/blobs/{oid}"), None)
            .unwrap();
        let content = b.body["content"].as_str().unwrap();
        assert!(content.contains('\n'), "wrapped, as GitHub sends it");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(content.replace('\n', ""))
            .unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), text);
    }

    #[test]
    fn a_compare_says_how_two_commits_relate_and_can_lag() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let next = fake.hand_commit(&[("runs/a/1.jsonl", Some("x\n"))]);
        let other = fake.rewrite_ledger(&[("format", "1\n")]);
        let c = client(&fake);
        let status = |base: &str, head: &str| {
            let r = c
                .send(
                    Method::Get,
                    &format!("{REPO}/compare/{base}...{head}"),
                    None,
                )
                .unwrap();
            (
                r.status,
                r.body["status"].as_str().unwrap_or("").to_string(),
            )
        };
        assert_eq!(status(&root, &root), (200, "identical".into()));
        assert_eq!(status(&root, &next), (200, "ahead".into()));
        assert_eq!(status(&next, &root), (200, "behind".into()));
        assert_eq!(status(&next, &other), (200, "diverged".into()));
        assert_eq!(
            status(&root, "0000000000000000000000000000000000000000").0,
            404
        );
        fake.state().compare_behind_next = 1;
        assert_eq!(
            status(&root, &next),
            (200, "behind".into()),
            "a lagging replica"
        );
        assert_eq!(status(&root, &next), (200, "ahead".into()), "and then not");
    }

    // Spec §6.2 and §8.1: rules in force only, and the plan's refusal.
    // Modelled — confirmed by live tests `rules_on_the_ledger_branch_are_readable`
    // and `a_private_repository_without_a_ruleset_is_detection_only`.
    #[test]
    fn the_rules_on_a_branch_are_those_of_active_rulesets() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        let types = || -> Vec<String> {
            let r = c
                .send(
                    Method::Get,
                    &format!("{REPO}/rules/branches/fl/ledger"),
                    None,
                )
                .unwrap();
            r.body
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["type"].as_str().unwrap().to_string())
                .collect()
        };
        assert!(types().is_empty());
        fake.state().rulesets = vec![
            Ruleset::on_ledger("active", &["non_fast_forward"]),
            Ruleset::on_ledger("disabled", &["deletion"]),
            Ruleset::on_ledger("evaluate", &["deletion"]),
        ];
        assert_eq!(types(), vec!["non_fast_forward".to_string()]);
        fake.state().rules_need_upgrade = true;
        let err = c
            .send(
                Method::Get,
                &format!("{REPO}/rules/branches/fl/ledger"),
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("Upgrade to GitHub"), "{err}");
    }

    #[test]
    fn a_tree_or_commit_the_fake_cannot_build_is_refused() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let c = client(&fake);
        let post = |rest: &str, body: serde_json::Value| {
            c.send(Method::Post, &format!("{REPO}/{rest}"), Some(&body))
        };
        for (rest, body, says) in [
            (
                "git/trees",
                json!({"base_tree": root, "tree": []}),
                "from scratch",
            ),
            ("git/trees", json!({}), "tree is required"),
            (
                "git/trees",
                json!({"tree": [{"path": "f", "type": "blob"}]}),
                "only blobs",
            ),
            ("git/commits", json!({"tree": "nope"}), "required"),
            (
                "git/commits",
                json!({"tree": "nope", "message": "m"}),
                "Tree SHA",
            ),
        ] {
            let err = post(rest, body).unwrap_err();
            assert!(err.to_string().contains(says), "{rest} {says}: {err}");
        }
        let tree = post("git/trees", json!({"tree": []})).unwrap().body["sha"].clone();
        let err = post(
            "git/commits",
            json!({"tree": tree, "message": "m", "parents": ["nope"]}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("Parent SHA"), "{err}");
        fake.state().fail_next_git_create = true;
        assert_eq!(post("git/trees", json!({"tree": []})).unwrap().status, 500);
        fake.state().fail_next_git_create = true;
        let made = post("git/commits", json!({"tree": tree, "message": "m"})).unwrap();
        assert_eq!(made.status, 500);
    }

    // Spec §3.5 check 2: a replica that has not seen the last write.
    #[test]
    fn a_lagging_replica_answers_an_older_head_or_knows_no_commit() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let head = fake.hand_commit(&[("runs/a/1.jsonl", Some("x\n"))]);
        let c = client(&fake);
        let read = || {
            c.send(
                Method::Get,
                &format!("{REPO}/git/ref/heads/fl/ledger"),
                None,
            )
            .unwrap()
            .body["object"]["sha"]
                .clone()
        };
        fake.state().ref_behind_next = 1;
        assert_eq!(read(), json!(root), "the commit before the head");
        assert_eq!(read(), json!(head), "and then the head");
        fake.state().compare_unknown_next = 1;
        let compare = || {
            c.send(
                Method::Get,
                &format!("{REPO}/compare/{root}...{head}"),
                None,
            )
            .unwrap()
            .status
        };
        assert_eq!(compare(), 404);
        assert_eq!(compare(), 200);
    }

    #[test]
    fn a_repository_answer_can_name_no_visibility_and_a_rules_read_can_fail() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        fake.state().omit_visibility = true;
        let r = c.send(Method::Get, REPO, None).unwrap();
        assert!(r.body.get("visibility").is_none(), "{}", r.body);
        fake.state().fail_rules_next = true;
        let r = c
            .send(
                Method::Get,
                &format!("{REPO}/rules/branches/fl/ledger"),
                None,
            )
            .unwrap();
        assert_eq!(r.status, 500);
    }

    #[test]
    fn a_fake_that_is_down_is_unreachable() {
        let fake = FakeGithub::start("acme/widgets");
        fake.state().down = true;
        let err = client(&fake)
            .send(Method::Get, "/repos/acme/widgets", None)
            .unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        fake.state().down = false;
        assert_eq!(
            client(&fake)
                .send(Method::Get, "/repos/acme/widgets", None)
                .unwrap()
                .status,
            200
        );
    }

    #[test]
    fn the_branch_helpers_commit_merge_rewrite_and_delete() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        assert_eq!(fake.ledger_commits(), 1);
        fake.hand_commit(&[("runs/a/1.jsonl", Some("x\n"))]);
        assert_eq!(fake.ledger_commits(), 2);
        let merge = fake.hand_merge();
        assert_eq!(fake.state().git.commits[&merge].parents.len(), 2);
        fake.hand_commit(&[("runs/a/1.jsonl", None)]);
        assert!(!fake.ledger_files().contains_key("runs/a/1.jsonl"));
        let new_root = fake.rewrite_ledger(&[("format", "1\n")]);
        assert_ne!(new_root, root);
        assert_eq!(fake.ledger_commits(), 1);
        fake.delete_ledger();
        assert_eq!(fake.ledger_head(), None);
    }
}
