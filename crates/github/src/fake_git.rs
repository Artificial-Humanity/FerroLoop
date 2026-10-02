//! The fake's git objects, and the REST endpoints the GitHub ledger uses
//! (GitHub ledger spec §8.1): refs, trees, commits, blobs, compare, and the
//! rules on a branch.
//!
//! ⚠ It proves structure, not integration: object ids are SHA-256 based,
//! not git's, and each answer's shape is fl's reading of GitHub's
//! documentation. A shape no live test confirms yet says so.

use crate::fake::{Answer, State, answer};
use crate::ledger::layout::{BRANCH, FORMAT_FILE, README, README_FILE};
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

/// The ledger's REST endpoints; `None` for every other route. `q` is the
/// request's query parameters (e.g. `recursive` on a tree read).
pub(crate) fn rest(
    s: &mut State,
    method: &str,
    parts: &[&str],
    q: &BTreeMap<String, String>,
    body: &str,
) -> Option<Answer> {
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
            if s.ref_404_next > 0 {
                s.ref_404_next -= 1;
                return Some(not_found());
            }
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
        ("GET", ["git", "trees", sha]) => {
            get_tree(s, sha, q.get("recursive").map(String::as_str) == Some("1"))
        }
        ("GET", ["git", "blobs", sha]) => match s.git.blobs.get(*sha) {
            Some(t) => answer(
                200,
                json!({"sha": sha, "size": t.len(), "encoding": "base64", "content": wrapped(t)}),
            ),
            None => not_found(),
        },
        ("GET", ["compare", spec]) => compare(s, spec),
        ("GET", ["rules", "branches", name @ ..]) => rules(s, &full, &name.join("/"), q),
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
        // Someone else's `init` lands first: the same first commit any
        // `init` makes — `format` and `README.md`, no parent — not an
        // arbitrary orphan.
        let files = BTreeMap::from([
            (FORMAT_FILE.to_string(), "1\n".to_string()),
            (README_FILE.to_string(), README.to_string()),
        ]);
        let tree = s.git.put_tree(&files);
        let theirs = s.git.put_commit(&tree, vec![], "fl: start the ledger");
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
    // ⚠ A parent that is not a string is refused outright — never silently
    // dropped, which would otherwise turn a malformed request into an
    // accidental root commit.
    let mut parents: Vec<String> = Vec::new();
    if let Some(arr) = v["parents"].as_array() {
        for p in arr {
            match p.as_str() {
                Some(sha) => parents.push(sha.to_string()),
                None => return unprocessable("each parent SHA must be a string"),
            }
        }
    }
    if parents.iter().any(|p| !s.git.commits.contains_key(p)) {
        return unprocessable("Parent SHA does not exist");
    }
    let sha = s.git.put_commit(tree, parents, message);
    answer(201, commit_json(&sha, &s.git.commits[&sha]))
}

/// ⚠ Modelled: without `recursive=1`, GitHub lists only the tree's own
/// level (direct blobs, and one `tree` entry per immediate subdirectory);
/// `recursive=1` lists every blob at its full path plus every intermediate
/// directory. Unmeasured; no live test provokes either shape.
fn get_tree(s: &State, sha: &str, recursive: bool) -> Answer {
    let Some(files) = s.git.trees.get(sha) else {
        return not_found();
    };
    let mut items = Vec::new();
    if recursive {
        let mut dirs = BTreeSet::new();
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
    } else {
        let mut top_dirs = BTreeSet::new();
        for (path, blob) in files {
            match path.split_once('/') {
                None => items.push(json!({
                    "path": path, "mode": "100644", "type": "blob", "sha": blob,
                    "size": s.git.blobs[blob].len(),
                })),
                Some((top, _)) => {
                    top_dirs.insert(top.to_string());
                }
            }
        }
        for d in top_dirs {
            items.push(json!({
                "path": d, "mode": "040000", "type": "tree",
                "sha": object_id(&["subtree", sha, &d]),
            }));
        }
    }
    answer(
        200,
        json!({"sha": sha, "tree": items, "truncated": s.truncate_trees}),
    )
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
/// the branch — a disabled or evaluate-only ruleset contributes none —
/// paged like every list. Confirmed by live test
/// `rules_on_the_ledger_branch_are_readable`. With no ruleset it answers
/// `200 []`: measured on 2026-10-02 for a private repository on GitHub
/// Free, and confirmed by live test
/// `a_private_repository_without_a_ruleset_is_detection_only`. The 403
/// behind `rules_need_upgrade` is defensive and unmeasured.
fn rules(s: &mut State, full: &str, branch: &str, q: &BTreeMap<String, String>) -> Answer {
    if std::mem::take(&mut s.fail_rules_next) {
        return answer(500, json!({"message": "fake rules failure"}));
    }
    if s.rules_need_upgrade {
        let mut a = answer(
            403,
            json!({"message": "Upgrade to GitHub Pro or make this repository public to enable \
                               this feature."}),
        );
        // ⚠ Modelled: `x-accepted-github-permissions` can ride along on this
        // 403 too, even though rulesets are unavailable for the plan, not
        // for want of a permission. Unmeasured; no live test provokes it.
        a.headers.push((
            "x-accepted-github-permissions".into(),
            "administration=write".into(),
        ));
        return a;
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
    s.page(&format!("/repos/{full}/rules/branches/{branch}"), q, items)
}

/// The ledger's GraphQL operations, told apart by operation name; `None`
/// for every other query.
pub(crate) fn graphql(s: &mut State, v: &Value) -> Option<Answer> {
    let query = v["query"].as_str().unwrap_or("");
    let vars = &v["variables"];
    if query.starts_with("query ledgerObjects(") {
        return Some(objects(s, vars));
    }
    if query.starts_with("query ledgerBlame(") {
        return Some(blame(s, vars));
    }
    if query.starts_with("mutation ledgerAppend(") {
        return Some(append(s, vars));
    }
    None
}

fn repository_known(s: &State, vars: &Value) -> bool {
    match (vars["owner"].as_str(), vars["name"].as_str()) {
        (Some(o), Some(n)) => s.is_bound(o, n),
        _ => false,
    }
}

/// ⚠ Modelled: `object(expression: "<commit>:<path>")` answers a tree's
/// entries, a blob's id, or `null` — no error — for a path the commit does
/// not hold. Confirmed by live test `init_sets_up_a_ledger_on_a_private_repository`.
///
/// When the caller also asks for `head` (the commit itself, by `oid`):
/// `graphql_commit_unknown_next` makes this replica answer as though it has
/// not learned of that commit yet — `head` AND every `eN` come back null,
/// even for a commit and paths that are real, since a replica that has not
/// replicated a commit could not resolve expressions against it either
/// (spec §3.5 / ruling 24).
fn objects(s: &mut State, vars: &Value) -> Answer {
    if !repository_known(s, vars) {
        return answer(
            200,
            json!({"data": {"repository": null}, "errors": [{"type": "NOT_FOUND"}]}),
        );
    }
    let head = vars.get("head").and_then(Value::as_str);
    let lagging = head.is_some() && s.graphql_commit_unknown_next > 0;
    if lagging {
        s.graphql_commit_unknown_next -= 1;
    }
    let mut repo = serde_json::Map::new();
    if let Some(head) = head {
        // ⚠ Modelled: `object(oid:)` on a commit GitHub does not hold at
        // all (never existed — not merely "not yet replicated") answers
        // `null` with no error, the same shape `graphql_commit_unknown_next`
        // simulates for a lag — unmeasured; no live test confirms it.
        let known = s.git.commits.contains_key(head) && !lagging;
        repo.insert(
            "head".into(),
            if known {
                json!({"oid": head})
            } else {
                Value::Null
            },
        );
    }
    if let Some(all) = vars.as_object() {
        for (k, e) in all {
            if k.starts_with('e') && k[1..].parse::<u32>().is_ok() {
                let v = if lagging {
                    Value::Null
                } else {
                    object_at(&s.git, e.as_str().unwrap_or(""))
                };
                repo.insert(k.clone(), v);
            }
        }
    }
    answer(200, json!({"data": {"repository": repo}}))
}

fn object_at(git: &Git, expression: &str) -> Value {
    let Some((commit, path)) = expression.split_once(':') else {
        return Value::Null;
    };
    let Some(files) = git.commits.get(commit).and_then(|c| git.trees.get(&c.tree)) else {
        return Value::Null;
    };
    if let Some(blob) = files.get(path) {
        return json!({"__typename": "Blob", "oid": blob, "byteSize": git.blobs[blob].len()});
    }
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    };
    let mut entries: BTreeMap<String, Value> = BTreeMap::new();
    for (p, blob) in files {
        let Some(rest) = p.strip_prefix(prefix.as_str()) else {
            continue;
        };
        match rest.split_once('/') {
            Some((dir, _)) => {
                entries.entry(dir.to_string()).or_insert_with(|| {
                    json!({
                        "name": dir, "type": "tree", "mode": 0o040000,
                        "oid": object_id(&["subtree", commit, &format!("{prefix}{dir}")]),
                    })
                });
            }
            None => {
                entries.insert(
                    rest.to_string(),
                    json!({"name": rest, "oid": blob, "type": "blob", "mode": 0o100644}),
                );
            }
        }
    }
    if entries.is_empty() {
        return Value::Null;
    }
    json!({"__typename": "Tree", "entries": entries.into_values().collect::<Vec<_>>()})
}

/// ⚠ Modelled: `Commit.blame(path:)` answers ranges of lines, each with the
/// commit that last changed them. The fake follows first parents and
/// compares line by line. Confirmed by live test
/// `a_hand_edit_is_detected_and_named`.
fn blame(s: &State, vars: &Value) -> Answer {
    let nothing = || answer(200, json!({"data": {"repository": {"object": null}}}));
    let (Some(commit), Some(path)) = (vars["commit"].as_str(), vars["path"].as_str()) else {
        return nothing();
    };
    if !repository_known(s, vars) || !s.git.commits.contains_key(commit) {
        return nothing();
    }
    let mut chain = Vec::new();
    let mut at = Some(commit.to_string());
    while let Some(c) = at {
        at = s.git.commits[&c].parents.first().cloned();
        chain.push(c);
    }
    chain.reverse();
    let mut owners: Vec<String> = Vec::new();
    let mut before: Vec<String> = Vec::new();
    for c in &chain {
        let text = s
            .git
            .files_at(c)
            .and_then(|f| f.get(path).cloned())
            .unwrap_or_default();
        let now: Vec<String> = text.lines().map(str::to_string).collect();
        owners.truncate(now.len());
        for (i, line) in now.iter().enumerate() {
            if before.get(i) != Some(line) {
                if i < owners.len() {
                    owners[i] = c.clone();
                } else {
                    owners.push(c.clone());
                }
            }
        }
        before = now;
    }
    let mut ranges = Vec::new();
    let mut start = 0usize;
    for i in 1..=owners.len() {
        if i == owners.len() || owners[i] != owners[start] {
            ranges.push(json!({
                "startingLine": start + 1, "endingLine": i,
                "commit": {"oid": owners[start]},
            }));
            start = i;
        }
    }
    answer(
        200,
        json!({"data": {"repository": {"object": {"blame": {"ranges": ranges}}}}}),
    )
}

/// ⚠ Modelled from GitHub's documentation: `createCommitOnBranch` refuses a
/// stale `expectedHeadOid` with an error of type `STALE_DATA` whose message
/// says where the branch was expected to point, and lands nothing.
/// Confirmed by live test `create_commit_on_branch_is_refused_when_the_head_moved`.
fn append(s: &mut State, vars: &Value) -> Answer {
    let input = &vars["input"];
    let branch = input
        .pointer("/branch/branchName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let named = input
        .pointer("/branch/repositoryNameWithOwner")
        .and_then(Value::as_str)
        .unwrap_or("");
    let bound = named.split_once('/').is_some_and(|(o, r)| s.is_bound(o, r));
    // ⚠ Modelled: an unbound repository and a branch GitHub cannot resolve
    // both answer `createCommitOnBranch: null` with a NOT_FOUND error — the
    // same shape either way. Unmeasured; no live test provokes either case
    // yet.
    if !bound || s.git.head(&branch).is_none() {
        return answer(
            200,
            json!({
                "data": {"createCommitOnBranch": null},
                "errors": [{
                    "type": "NOT_FOUND",
                    "message": format!("Could not resolve to a ref named `{branch}`"),
                }],
            }),
        );
    }
    if std::mem::take(&mut s.rate_limit_next_commit) {
        return answer(
            200,
            json!({"data": null, "errors": [{"type": "RATE_LIMITED"}]}),
        );
    }
    if let Some(needs) = s.refuse_next_commit_for.take() {
        let mut a = answer(
            403,
            json!({"message": "Resource not accessible by integration"}),
        );
        a.headers
            .push(("x-accepted-github-permissions".into(), needs));
        return a;
    }
    // ⚠ Modelled — confirmed by live test
    // `create_commit_on_branch_without_contents_write_is_refused`: live
    // GraphQL can refuse a missing permission as a 200 carrying a FORBIDDEN
    // error instead of the 403 above. Both shapes are a refusal: nothing
    // lands, and the branch head does not move.
    if std::mem::take(&mut s.refuse_next_commit_as_forbidden) {
        return answer(
            200,
            json!({
                "data": null,
                "errors": [{
                    "type": "FORBIDDEN",
                    "message": "Resource not accessible by integration",
                }],
            }),
        );
    }
    if !s.foreign_appends.is_empty() {
        let (path, line) = s.foreign_appends.remove(0);
        let head = s.git.head(&branch).expect("checked above");
        let mut text = s
            .git
            .files_at(&head)
            .and_then(|f| f.get(&path).cloned())
            .unwrap_or_default();
        text.push_str(&line);
        text.push('\n');
        s.git
            .commit_on(&branch, &[(path, Some(text))], "another machine's append");
    }
    let head = s.git.head(&branch).expect("checked above");
    let expected = input["expectedHeadOid"].as_str().unwrap_or("");
    if expected != head {
        return answer(
            200,
            json!({
                "data": {"createCommitOnBranch": null},
                "errors": [{
                    "type": "STALE_DATA", "path": ["createCommitOnBranch"],
                    "message": format!(
                        "Expected branch to point to \"{expected}\" but it did not. Pull and \
                         try again."
                    ),
                }],
            }),
        );
    }
    if s.fail_commits > 0 {
        s.fail_commits -= 1;
        return answer(502, json!({"message": "fake: the commit did not land"}));
    }
    // The ledger only ever appends (global constraints §3.6: "nothing is
    // ever removed"), so fl never sends a deletion — but the fake must
    // refuse one outright rather than silently drop it, the same way it
    // refuses a malformed addition below.
    let deletions = input
        .pointer("/fileChanges/deletions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !deletions.is_empty() {
        return answer(
            200,
            json!({"errors": [{
                "type": "UNPROCESSABLE",
                "message": "the fake does not support fileChanges.deletions",
            }]}),
        );
    }
    let mut changes = Vec::new();
    let additions = input
        .pointer("/fileChanges/additions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for add in additions {
        let text = add["contents"]
            .as_str()
            .and_then(|c| STANDARD.decode(c).ok())
            .and_then(|b| String::from_utf8(b).ok());
        let (Some(path), Some(text)) = (add["path"].as_str(), text) else {
            return answer(
                200,
                json!({"errors": [{
                    "type": "UNPROCESSABLE",
                    "message": "an addition needs a path and base64 contents",
                }]}),
            );
        };
        changes.push((path.to_string(), Some(text)));
    }
    let headline = input
        .pointer("/message/headline")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let oid = s.git.commit_on(&branch, &changes, &headline);
    if std::mem::take(&mut s.hang_up_after_next_commit) {
        let mut a = answer(200, Value::Null);
        a.hang_up = true;
        return a;
    }
    if std::mem::take(&mut s.garble_next_commit_answer) {
        let mut a = answer(200, Value::Null);
        a.raw_body = Some("<html>fake: not JSON</html>".into());
        return a;
    }
    if std::mem::take(&mut s.break_next_commit_answer) {
        let mut a = answer(200, Value::Null);
        a.break_body = true;
        return a;
    }
    answer(
        200,
        json!({"data": {"createCommitOnBranch": {"commit": {"oid": oid}}}}),
    )
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

    // The create_ref body checks the brief's other tests never provoke: a
    // missing field, a non-`refs/` name, a `sha` naming no commit, and the
    // collision direction `a_ref_that_exists_or_collides_with_another_is_refused`
    // never exercises — a NEW name lying under an EXISTING ref.
    #[test]
    fn create_ref_is_refused_for_a_malformed_body_a_missing_object_or_a_name_under_an_existing_ref()
    {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        // An existing ref that is a PARENT path of a name about to be made —
        // the direction `name.starts_with("{r}/")`, never hit above (there,
        // the new name was the parent, not the child).
        fake.state()
            .git
            .refs
            .insert("heads/fl".into(), root.clone());
        let c = client(&fake);
        let post = |body: serde_json::Value| {
            c.send(Method::Post, &format!("{REPO}/git/refs"), Some(&body))
        };
        for (body, says) in [
            (json!({}), "ref and sha are required"),
            (
                json!({"ref": "heads/x", "sha": root.clone()}),
                "must start with 'refs/'",
            ),
            (
                json!({"ref": "refs/heads/x", "sha": "0".repeat(40)}),
                "Object does not exist",
            ),
            (
                json!({"ref": "refs/heads/fl/other", "sha": root.clone()}),
                "conflicts",
            ),
        ] {
            let err = post(body).unwrap_err();
            assert!(err.to_string().contains(says), "{says}: {err}");
        }
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
            404,
            "an unknown head"
        );
        assert_eq!(
            status("0000000000000000000000000000000000000000", &root).0,
            404,
            "an unknown base"
        );
        fake.state().compare_behind_next = 1;
        assert_eq!(
            status(&root, &next),
            (200, "behind".into()),
            "a lagging replica"
        );
        assert_eq!(status(&root, &next), (200, "ahead".into()), "and then not");
    }

    // Spec §6.2 and §8.1: rules in force only; the 403 is defensive.
    // Modelled — confirmed by live test `rules_on_the_ledger_branch_are_readable`.
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
            // Active, but on another branch entirely — must not be reported
            // under `fl/ledger`.
            Ruleset {
                enforcement: "active".into(),
                branch: "main".into(),
                rules: vec!["deletion".into()],
            },
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
        // GitHub's own message is still readable even though the fake now
        // also attaches `x-accepted-github-permissions` to this 403 (the
        // header is not a promise that a permission was actually missing).
        assert!(err.to_string().contains("Upgrade to GitHub"), "{err}");
        assert!(err.to_string().contains("GitHub says this needs"), "{err}");
    }

    // Measured on 2026-10-02: a private repository on GitHub Free, with no
    // ruleset, answers `200 []`. Confirmed by live test
    // `a_private_repository_without_a_ruleset_is_detection_only`.
    #[test]
    fn a_branch_with_no_ruleset_answers_an_empty_list() {
        let fake = FakeGithub::start("acme/widgets");
        let r = client(&fake)
            .send(
                Method::Get,
                &format!("{REPO}/rules/branches/fl/ledger"),
                None,
            )
            .unwrap();
        assert_eq!((r.status, r.body), (200, json!([])));
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

    // A parent that is not a string must be refused, not silently dropped
    // into an accidental root commit (GitHub refuses a malformed array
    // element; it does not quietly ignore it).
    #[test]
    fn a_parent_that_is_not_a_string_is_refused_not_dropped() {
        let fake = FakeGithub::start("acme/widgets");
        let c = client(&fake);
        let tree = c
            .send(
                Method::Post,
                &format!("{REPO}/git/trees"),
                Some(&json!({"tree": []})),
            )
            .unwrap()
            .body["sha"]
            .clone();
        let err = c
            .send(
                Method::Post,
                &format!("{REPO}/git/commits"),
                Some(&json!({"tree": tree, "message": "m", "parents": [123]})),
            )
            .unwrap_err();
        assert!(err.to_string().contains("must be a string"), "{err}");
    }

    // The ledger's git endpoints only answer for the BOUND repository
    // (spec §8.1): another repository the fake also knows about (GitHub
    // ledger spec: repositories `acme/widgets`/`acme/other`) must not reach
    // the same git state through its own name.
    #[test]
    fn a_git_endpoint_is_served_only_for_the_bound_repository() {
        let fake = FakeGithub::start("acme/widgets");
        fake.seed_ledger();
        fake.reuse_name("acme/other");
        let c = client(&fake);
        let r = c
            .send(
                Method::Get,
                "/repos/acme/other/git/ref/heads/fl/ledger",
                None,
            )
            .unwrap();
        assert_eq!(r.status, 404, "acme/other is not the bound repository");
    }

    // GitHub's tree read defaults to one level; `recursive=1` asks for the
    // whole tree.
    #[test]
    fn a_tree_read_is_one_level_deep_unless_recursive_is_asked() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger_with(&[("format", "1\n"), ("runs/a/1.jsonl", "x\n")]);
        let tree_sha = fake.state().git.commits[&root].tree.clone();
        let c = client(&fake);
        let paths = |recursive: bool| -> Vec<String> {
            let url = if recursive {
                format!("{REPO}/git/trees/{tree_sha}?recursive=1")
            } else {
                format!("{REPO}/git/trees/{tree_sha}")
            };
            c.send(Method::Get, &url, None).unwrap().body["tree"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["path"].as_str().unwrap().to_string())
                .collect()
        };
        let one_level = paths(false);
        assert!(one_level.contains(&"format".to_string()), "{one_level:?}");
        assert!(one_level.contains(&"runs".to_string()), "{one_level:?}");
        assert!(
            !one_level.iter().any(|p| p.contains('/')),
            "one level only: {one_level:?}"
        );
        let full = paths(true);
        assert!(full.contains(&"runs/a/1.jsonl".to_string()), "{full:?}");
        assert!(full.contains(&"runs/a".to_string()), "{full:?}");
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

    // The fake tells the operations apart by name and reads the variables;
    // the fields asked for do not change its answer.
    const OBJECTS: &str = "query ledgerObjects($owner: String!, $name: String!, \
        $e0: String!, $e1: String!, $e2: String!, $e3: String!) { \
        repository(owner: $owner, name: $name) { e0: object(expression: $e0) { __typename } } }";
    const APPEND: &str = "mutation ledgerAppend($input: CreateCommitOnBranchInput!) { \
        createCommitOnBranch(input: $input) { commit { oid } } }";
    const BLAME: &str = "query ledgerBlame($owner: String!, $name: String!, \
        $commit: GitObjectID!, $path: String!) { \
        repository(owner: $owner, name: $name) { object(oid: $commit) { __typename } } }";

    fn append_input(head: &str, path: &str, text: &str) -> serde_json::Value {
        use base64::Engine;
        json!({"input": {
            "branch": {"repositoryNameWithOwner": "acme/widgets", "branchName": "fl/ledger"},
            "message": {"headline": "fl: a test append"},
            "expectedHeadOid": head,
            "fileChanges": {"additions": [{
                "path": path,
                "contents": base64::engine::general_purpose::STANDARD.encode(text),
            }]},
        }})
    }

    // ⚠ Modelled: `object(expression: "<commit>:<path>")` answers a tree's
    // entries, a blob's id, or `null` for a path the commit does not hold.
    #[test]
    fn the_objects_at_a_commit_are_listed_by_expression() {
        let fake = FakeGithub::start("acme/widgets");
        fake.seed_ledger();
        let head = fake.hand_commit(&[
            ("runs/k/1.jsonl", Some("a\n")),
            ("runs/k/2.jsonl", Some("b\n")),
        ]);
        let a = client(&fake)
            .graphql_answer(
                OBJECTS,
                json!({
                    "owner": "acme", "name": "widgets",
                    "e0": format!("{head}:format"), "e1": format!("{head}:runs/k"),
                    "e2": format!("{head}:runs"), "e3": format!("{head}:nothing"),
                }),
            )
            .unwrap();
        let repo = &a.data.unwrap()["repository"];
        assert_eq!(repo["e0"]["__typename"], "Blob");
        assert_eq!(repo["e0"]["oid"].as_str().map(str::len), Some(40));
        let names: Vec<&str> = repo["e1"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["1.jsonl", "2.jsonl"]);
        assert_eq!(repo["e2"]["entries"][0]["type"], "tree");
        // GraphQL's `TreeEntry.mode` is an `Int`: the octal mode's value.
        assert_eq!(repo["e1"]["entries"][0]["mode"], 0o100644);
        assert_eq!(repo["e2"]["entries"][0]["mode"], 0o040000);
        assert!(repo["e3"].is_null());
    }

    // The git endpoints are served only for the
    // bound repository (as REST's `a_git_endpoint_is_served_only_for_the_bound_repository`
    // already checks); `ledgerObjects` must do the same.
    #[test]
    fn ledger_objects_against_an_unbound_repository_is_not_found() {
        let fake = FakeGithub::start("acme/widgets");
        let head = fake.seed_ledger();
        fake.reuse_name("acme/other");
        let a = client(&fake)
            .graphql_answer(
                OBJECTS,
                json!({
                    "owner": "acme", "name": "other",
                    "e0": format!("{head}:format"), "e1": format!("{head}:format"),
                    "e2": format!("{head}:format"), "e3": format!("{head}:format"),
                }),
            )
            .unwrap();
        assert!(a.data.unwrap()["repository"].is_null());
        assert_eq!(a.errors[0]["type"], "NOT_FOUND");
    }

    #[test]
    fn an_append_lands_on_the_head_it_expects_and_is_refused_on_any_other() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let c = client(&fake);
        let a = c
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\n"))
            .unwrap();
        let oid = a.data.unwrap()["createCommitOnBranch"]["commit"]["oid"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(fake.ledger_head(), Some(oid.clone()));
        assert_eq!(fake.ledger_files()["runs/k/1.jsonl"], "a\n");
        // ⚠ Modelled: a stale expectedHeadOid is refused with STALE_DATA.
        // Confirmed by live test `create_commit_on_branch_is_refused_when_the_head_moved`.
        let stale = c
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\nb\n"))
            .unwrap();
        assert_eq!(stale.errors[0]["type"], "STALE_DATA");
        assert_eq!(
            fake.ledger_files()["runs/k/1.jsonl"],
            "a\n",
            "nothing landed"
        );
        assert_eq!(
            fake.ledger_head(),
            Some(oid),
            "a refused commit leaves the branch head unchanged"
        );
    }

    #[test]
    fn another_machines_append_can_land_first() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.state()
            .foreign_appends
            .push(("runs/k/1.jsonl".into(), "theirs".into()));
        let a = client(&fake)
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "mine\n"))
            .unwrap();
        assert_eq!(a.errors[0]["type"], "STALE_DATA");
        assert_eq!(fake.ledger_files()["runs/k/1.jsonl"], "theirs\n");
        assert_eq!(
            fake.ledger_commits(),
            2,
            "the foreign commit landed; the refused one did not move the head further"
        );
    }

    #[test]
    fn a_commit_that_did_not_land_answers_502_and_one_whose_answer_was_lost_landed() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let c = client(&fake);
        fake.state().fail_commits = 1;
        let a = c
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\n"))
            .unwrap();
        assert_eq!(a.status, 502);
        assert_eq!(fake.ledger_commits(), 1, "nothing landed");
        assert_eq!(
            fake.ledger_head(),
            Some(root.clone()),
            "a refused commit leaves the branch head unchanged"
        );
        fake.state().hang_up_after_next_commit = true;
        let err = c
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\n"))
            .unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert_eq!(
            fake.ledger_commits(),
            2,
            "it landed; only the answer was lost"
        );
    }

    // The two answers a commit that landed can come back with that are not
    // an answer at all: a 200 whose body is not JSON, and one whose body
    // breaks off. The plain client reads each as an error; it landed.
    #[test]
    fn a_commit_that_landed_can_be_answered_with_a_garbled_or_broken_body() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let c = client(&fake);
        fake.state().garble_next_commit_answer = true;
        let err = c
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\n"))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(ref m) if m.contains("not JSON")),
            "{err:?}"
        );
        assert_eq!(fake.ledger_commits(), 2, "it landed");
        let head = fake.ledger_head().unwrap();
        fake.state().break_next_commit_answer = true;
        let err = c
            .graphql_answer(APPEND, append_input(&head, "runs/k/1.jsonl", "a\nb\n"))
            .unwrap_err();
        assert!(matches!(err, StoreError::Unreachable { .. }), "{err:?}");
        assert_eq!(fake.ledger_commits(), 3, "it landed");
        let a = c
            .graphql_answer(
                APPEND,
                append_input(&fake.ledger_head().unwrap(), "runs/k/1.jsonl", "a\nb\nc\n"),
            )
            .unwrap();
        assert!(a.data.is_some(), "both knobs are one-shot");
    }

    #[test]
    fn a_commit_answered_with_a_spent_rate_limit_lands_nothing() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.state().rate_limit_next_commit = true;
        let err = client(&fake)
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\n"))
            .unwrap_err();
        assert!(matches!(err, StoreError::RateLimited { .. }), "{err:?}");
        assert_eq!(fake.ledger_commits(), 1);
        assert_eq!(
            fake.ledger_head(),
            Some(root),
            "a refused commit leaves the branch head unchanged"
        );
    }

    #[test]
    fn a_commit_refused_for_want_of_a_permission_names_it() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.state().refuse_next_commit_for = Some("contents=write".into());
        let err = client(&fake)
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\n"))
            .unwrap_err();
        assert!(err.to_string().contains("contents=write"), "{err}");
        assert_eq!(
            fake.ledger_head(),
            Some(root),
            "a refused commit leaves the branch head unchanged"
        );
    }

    // ⚠ Modelled — confirmed by live test
    // `create_commit_on_branch_without_contents_write_is_refused`: live
    // GraphQL can refuse a missing permission as an HTTP 200 carrying an
    // error of type FORBIDDEN, rather than an HTTP 403 with
    // `x-accepted-github-permissions` (as
    // `a_commit_refused_for_want_of_a_permission_names_it` models above).
    #[test]
    fn a_commit_can_also_be_refused_as_an_http_200_with_a_forbidden_error() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.state().refuse_next_commit_as_forbidden = true;
        let a = client(&fake)
            .graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\n"))
            .unwrap();
        assert_eq!(a.status, 200);
        assert_eq!(a.errors[0]["type"], "FORBIDDEN");
        assert_eq!(
            a.errors[0]["message"],
            "Resource not accessible by integration"
        );
        assert_eq!(
            fake.ledger_head(),
            Some(root),
            "a refused commit leaves the branch head unchanged"
        );
    }

    #[test]
    fn an_append_to_a_branch_that_is_not_there_is_not_found() {
        let fake = FakeGithub::start("acme/widgets");
        let a = client(&fake)
            .graphql_answer(APPEND, append_input("0000", "runs/k/1.jsonl", "a\n"))
            .unwrap();
        assert_eq!(a.errors[0]["type"], "NOT_FOUND");
    }

    // `an_append_to_a_branch_that_is_not_there_is_not_found`
    // above exercises the branch-missing half of the NOT_FOUND check; this
    // covers the unbound-repository half, with the ledger actually seeded
    // (so a bug that checked only the branch's existence would miss it).
    #[test]
    fn an_append_naming_another_known_repository_is_not_found() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.reuse_name("acme/other");
        let mut input = append_input(&root, "runs/k/1.jsonl", "a\n");
        input["input"]["branch"]["repositoryNameWithOwner"] = json!("acme/other");
        let a = client(&fake).graphql_answer(APPEND, input).unwrap();
        assert_eq!(a.errors[0]["type"], "NOT_FOUND");
        assert_eq!(
            fake.ledger_head(),
            Some(root),
            "a refused commit leaves the branch head unchanged"
        );
    }

    // An addition the fake cannot read is
    // refused, not silently coerced into an empty path or empty content.
    #[test]
    fn an_addition_with_no_path_or_unreadable_base64_is_unprocessable() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let c = client(&fake);
        let unprocessable = |additions: serde_json::Value| {
            let mut input = append_input(&root, "unused", "unused");
            input["input"]["fileChanges"]["additions"] = additions;
            c.graphql_answer(APPEND, input).unwrap()
        };
        let bad_base64 = unprocessable(json!([{"path": "f", "contents": "not base64 !!!"}]));
        assert_eq!(bad_base64.errors[0]["type"], "UNPROCESSABLE");
        let no_path = unprocessable(json!([{"contents": "eA=="}]));
        assert_eq!(no_path.errors[0]["type"], "UNPROCESSABLE");
        assert_eq!(
            fake.ledger_head(),
            Some(root),
            "a refused commit leaves the branch head unchanged"
        );
    }

    // The fake must refuse a deletion
    // outright (fl never asks for one — the ledger only ever appends) and
    // not silently ignore it.
    #[test]
    fn an_append_carrying_a_deletion_is_refused_the_fake_does_not_support_it() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        let mut input = append_input(&root, "runs/k/1.jsonl", "a\n");
        input["input"]["fileChanges"]["deletions"] = json!([{"path": "format"}]);
        let a = client(&fake).graphql_answer(APPEND, input).unwrap();
        assert_eq!(a.errors[0]["type"], "UNPROCESSABLE");
        assert_eq!(
            fake.ledger_head(),
            Some(root),
            "a refused commit leaves the branch head unchanged"
        );
    }

    /// An append that must succeed, returning the new head — used by
    /// `each_one_shot_refusal_knob_resets_after_firing` to show a knob does
    /// not go on refusing past the one commit it names.
    fn land(c: &Client, head: &str, path: &str, text: &str) -> String {
        let a = c
            .graphql_answer(APPEND, append_input(head, path, text))
            .unwrap();
        a.data.unwrap()["createCommitOnBranch"]["commit"]["oid"]
            .as_str()
            .expect("the commit landed")
            .to_string()
    }

    // Every refusal knob above is one-shot —
    // after it fires once, the NEXT append on the current head must still
    // land, not go on being refused forever.
    #[test]
    fn each_one_shot_refusal_knob_resets_after_firing() {
        let fake = FakeGithub::start("acme/widgets");
        let mut root = fake.seed_ledger();
        let c = client(&fake);

        fake.state().rate_limit_next_commit = true;
        c.graphql_answer(APPEND, append_input(&root, "runs/k/1.jsonl", "a\n"))
            .unwrap_err();
        root = land(&c, &root, "runs/k/1.jsonl", "a\n");

        fake.state().refuse_next_commit_for = Some("contents=write".into());
        c.graphql_answer(APPEND, append_input(&root, "runs/k/2.jsonl", "b\n"))
            .unwrap_err();
        root = land(&c, &root, "runs/k/2.jsonl", "b\n");

        fake.state().refuse_next_commit_as_forbidden = true;
        let forbidden = c
            .graphql_answer(APPEND, append_input(&root, "runs/k/3.jsonl", "c\n"))
            .unwrap();
        assert_eq!(forbidden.errors[0]["type"], "FORBIDDEN");
        root = land(&c, &root, "runs/k/3.jsonl", "c\n");

        fake.state().hang_up_after_next_commit = true;
        c.graphql_answer(APPEND, append_input(&root, "runs/k/4.jsonl", "d\n"))
            .unwrap_err();
        // The hung-up commit landed — only its answer was lost — so the
        // next append must use the head it actually left behind, not the
        // one it was asked for.
        root = fake.ledger_head().expect("the hung-up commit landed");
        land(&c, &root, "runs/k/5.jsonl", "e\n");
    }

    // ⚠ Modelled: blame names, for each line, the commit that last changed
    // it. Confirmed by live test `a_hand_edit_is_detected_and_named`.
    #[test]
    fn blame_names_the_commit_that_last_changed_each_line() {
        let fake = FakeGithub::start("acme/widgets");
        fake.seed_ledger();
        let first = fake.hand_commit(&[("f.jsonl", Some("a\nb\n"))]);
        let second = fake.hand_commit(&[("f.jsonl", Some("a\nB\nc\n"))]);
        let a = client(&fake)
            .graphql_answer(
                BLAME,
                json!({"owner": "acme", "name": "widgets", "commit": second, "path": "f.jsonl"}),
            )
            .unwrap();
        let ranges = a.data.unwrap()["repository"]["object"]["blame"]["ranges"].clone();
        assert_eq!(
            ranges,
            json!([
                {"startingLine": 1, "endingLine": 1, "commit": {"oid": first}},
                {"startingLine": 2, "endingLine": 3, "commit": {"oid": second}},
            ])
        );
    }

    // Both halves of blame's guard are needed —
    // not just the bound-repository check: without the commit-existence
    // check, `s.git.commits[&c]` would panic in the server thread the
    // moment it tried to walk an unknown commit's first-parent chain.
    #[test]
    fn ledger_blame_against_an_unbound_repository_or_an_unknown_commit_answers_null() {
        let fake = FakeGithub::start("acme/widgets");
        let root = fake.seed_ledger();
        fake.reuse_name("acme/other");
        let c = client(&fake);
        // A commit that DOES exist, so only the repository-binding half of
        // the guard can be what rejects this — not the commit check.
        let unbound = c
            .graphql_answer(
                BLAME,
                json!({"owner": "acme", "name": "other", "commit": root, "path": "f"}),
            )
            .unwrap();
        assert!(unbound.data.unwrap()["repository"]["object"].is_null());
        let unknown_commit = c
            .graphql_answer(
                BLAME,
                json!({
                    "owner": "acme", "name": "widgets",
                    "commit": "0".repeat(40), "path": "f",
                }),
            )
            .unwrap();
        assert!(unknown_commit.data.unwrap()["repository"]["object"].is_null());
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
