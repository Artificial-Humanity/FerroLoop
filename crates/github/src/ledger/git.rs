//! The requests the GitHub ledger makes. Each answer is judged here, once,
//! so no caller reads a failure as data.

use super::GithubLedger;
use crate::client::Method;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fl_core::StoreError;
use serde_json::{Map, Value, json};

/// What a path on the branch is at one commit. A directory's entries are
/// read where a directory is listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Object {
    Tree,
    Blob { oid: String },
}

const OBJECT_FIELDS: &str =
    "__typename ... on Tree { entries { name oid type } } ... on Blob { oid }";

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
    pub(crate) fn objects(
        &self,
        head: &str,
        paths: &[String],
    ) -> Result<Vec<Option<Object>>, StoreError> {
        let (owner, name) = self.owner_and_name();
        let mut vars = Map::new();
        vars.insert("owner".into(), json!(owner));
        vars.insert("name".into(), json!(name));
        let mut declared = String::from("$owner: String!, $name: String!");
        let mut fields = String::new();
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
        paths
            .iter()
            .enumerate()
            .map(|(i, p)| parse_object(&repo[format!("e{i}").as_str()], p))
            .collect()
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
        Some("Tree") => Ok(Some(Object::Tree)),
        other => Err(backend(format!(
            "GitHub answered `{path}` on the ledger branch as {other:?}, which is neither a \
             file nor a directory"
        ))),
    }
}
