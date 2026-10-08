//! The JSON file Claude Code and Antigravity read: `mcpServers`, one entry per
//! server. Key order is kept with an `IndexMap` of raw values, never with
//! `serde_json`'s `preserve_order` (see the workspace Cargo.toml): every entry
//! fl does not write, and every other top-level key, is kept byte for byte;
//! only fl's entries and the layout around them are written fresh. A file
//! that is not strict JSON is refused, never rewritten.

use super::{Field, FileRefusal, Rendered};
use indexmap::IndexMap;
use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;
use std::fmt;
use std::path::Path;

const SERVERS: &str = "mcpServers";

/// A server's line in `mcpServers`, two levels in; a rendered entry's own
/// lines are indented from there.
const ENTRY_INDENT: &str = "    ";

const FIX_BY_HAND: &str = "Fix it by hand, then run `fl mcp sync` again";

/// An entry as fl writes it: pretty, two spaces a level, at its depth in the
/// file.
pub(super) fn render(fields: &[(&'static str, Field)]) -> Rendered {
    let mut entry = IndexMap::new();
    for (key, field) in fields {
        let value = match field {
            Field::Text(s) => serde_json::Value::from(s.as_str()),
            Field::List(values) => serde_json::Value::from(values.clone()),
            Field::Map(values) => {
                let map = values
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::from(v.as_str())));
                serde_json::Value::Object(map.collect())
            }
        };
        entry.insert(*key, value);
    }
    let text = serde_json::to_string_pretty(&entry).expect("strings always serialize");
    // A JSON string holds no raw newline, so each one is a line break.
    Rendered {
        text: text.replace('\n', &format!("\n{ENTRY_INDENT}")),
    }
}

/// An object's members in the file's order, each value as its own bytes.
struct Members {
    members: IndexMap<String, Box<RawValue>>,
    /// The first key seen twice: kept apart, since a map keeps only one.
    twice: Option<String>,
}

impl<'de> Deserialize<'de> for Members {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Members, D::Error> {
        d.deserialize_map(MembersVisitor)
    }
}

struct MembersVisitor;

impl<'de> Visitor<'de> for MembersVisitor {
    type Value = Members;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Members, A::Error> {
        let mut members = IndexMap::new();
        let mut twice = None;
        while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
            if members.contains_key(&key) {
                twice.get_or_insert(key);
            } else {
                members.insert(key, value);
            }
        }
        Ok(Members { members, twice })
    }
}

#[derive(Debug, Clone)]
pub(super) struct JsonFile {
    /// The top level; `mcpServers`, when present, holds its old bytes, and
    /// `servers` stands in for it.
    top: IndexMap<String, Box<RawValue>>,
    /// `None` when the file has no `mcpServers` and fl has written nothing.
    servers: Option<IndexMap<String, Box<RawValue>>>,
}

pub(super) fn open(path: &Path, bytes: Option<&[u8]>) -> Result<JsonFile, FileRefusal> {
    let Some(bytes) = bytes else {
        let mut file = JsonFile {
            top: IndexMap::new(),
            servers: None,
        };
        file.servers_mut();
        return Ok(file);
    };
    let refuse = |problem: String, next: &str| FileRefusal {
        path: path.to_path_buf(),
        problem,
        next: next.to_string(),
    };
    let not_strict = |cause: String| {
        refuse(
            format!(
                "it is not strict JSON ({cause}), so fl will not rewrite it and lose its \
                 comments, trailing commas or byte-order mark"
            ),
            "Remove them by hand, then run `fl mcp sync` again",
        )
    };
    let text = std::str::from_utf8(bytes).map_err(|e| not_strict(e.to_string()))?;
    let value: &RawValue = serde_json::from_str(text).map_err(|e| not_strict(e.to_string()))?;
    if !value.get().starts_with('{') {
        return Err(refuse(
            "its top level is not a JSON object".into(),
            FIX_BY_HAND,
        ));
    }
    let top: Members = serde_json::from_str(text).expect("a strict JSON object");
    if let Some(key) = top.twice {
        let problem = format!("the key `{key}` appears twice at the top level");
        return Err(refuse(problem, FIX_BY_HAND));
    }
    let servers = match top.members.get(SERVERS) {
        None => None,
        Some(raw) if !raw.get().starts_with('{') => {
            return Err(refuse(
                format!("`{SERVERS}` is not a JSON object"),
                FIX_BY_HAND,
            ));
        }
        Some(raw) => {
            let servers: Members = serde_json::from_str(raw.get()).expect("a strict JSON object");
            if let Some(name) = servers.twice {
                let problem = format!("the server `{name}` appears twice in `{SERVERS}`");
                return Err(refuse(problem, FIX_BY_HAND));
            }
            Some(servers.members)
        }
    };
    Ok(JsonFile {
        top: top.members,
        servers,
    })
}

impl JsonFile {
    /// `mcpServers`, added last when the file has none.
    fn servers_mut(&mut self) -> &mut IndexMap<String, Box<RawValue>> {
        if self.servers.is_none() {
            let empty = RawValue::from_string("{}".into()).expect("valid JSON");
            self.top.insert(SERVERS.into(), empty);
        }
        self.servers.get_or_insert_default()
    }

    pub(super) fn names(&self) -> Vec<String> {
        self.servers
            .iter()
            .flatten()
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub(super) fn entry(&self, name: &str) -> Option<Vec<u8>> {
        let entry = self.servers.as_ref()?.get(name)?;
        Some(entry.get().as_bytes().to_vec())
    }

    pub(super) fn set(&mut self, name: &str, entry: &Rendered) {
        let raw = RawValue::from_string(entry.text.clone()).expect("fl renders valid JSON");
        self.servers_mut().insert(name.to_string(), raw);
    }

    pub(super) fn remove(&mut self, name: &str) -> bool {
        let servers = self.servers.as_mut();
        servers.is_some_and(|s| s.shift_remove(name).is_some())
    }

    pub(super) fn to_bytes(&self) -> Vec<u8> {
        let top = self.top.iter().map(|(key, raw)| match &self.servers {
            Some(servers) if key == SERVERS => {
                let servers = servers.iter().map(|(k, v)| (k, v.get().to_string()));
                (key, object(servers, ENTRY_INDENT))
            }
            _ => (key, raw.get().to_string()),
        });
        let mut out = object(top, "  ");
        out.push('\n');
        out.into_bytes()
    }
}

/// `{}`, or one member a line at `indent`, the closing brace two spaces
/// further out.
fn object<'a>(members: impl Iterator<Item = (&'a String, String)>, indent: &str) -> String {
    let lines: Vec<String> = members
        .map(|(key, value)| {
            let key = serde_json::to_string(key).expect("a string always serializes");
            format!("{indent}{key}: {value}")
        })
        .collect();
    if lines.is_empty() {
        return "{}".into();
    }
    let close = &indent[2..];
    format!("{{\n{}\n{close}}}", lines.join(",\n"))
}

#[cfg(test)]
mod tests {
    use crate::vendor::fixtures::*;
    use crate::vendor::{Antigravity, Claude, Vendor};
    use std::path::Path;

    const FOREIGN: &str = r#"{
  "$schema": "https://example.com/schema.json",
  "mcpServers": {
    "docs": {"type": "http", "url": "https://old.example.com/mcp"},
    "theirs":   {
        "command" : "their-tool",   "args": [ "--fast" ]
      },
    "files": {"command": "old"}
  },
  "zeta": [1, 2,
     3]
}"#;

    #[test]
    fn foreign_entries_and_key_order_survive_a_rewrite() {
        let mut file = Claude
            .open(Path::new("f"), Some(FOREIGN.as_bytes()))
            .unwrap();
        let theirs = file.entry("theirs").unwrap();
        assert_eq!(
            String::from_utf8(theirs.clone()).unwrap(),
            "{\n        \"command\" : \"their-tool\",   \"args\": [ \"--fast\" ]\n      }"
        );
        file.set("docs", &Claude.render("docs", &server(WHOLE)).unwrap());
        assert!(file.remove("files"));
        file.set("events", &Claude.render("events", &server(SSE)).unwrap());
        let written = String::from_utf8(file.to_bytes()).unwrap();
        assert_eq!(
            written,
            r#"{
  "$schema": "https://example.com/schema.json",
  "mcpServers": {
    "docs": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": {
        "X-Api-Key": "${DOCS_KEY}"
      }
    },
    "theirs": {
        "command" : "their-tool",   "args": [ "--fast" ]
      },
    "events": {
      "type": "sse",
      "url": "https://events.example.com/sse"
    }
  },
  "zeta": [1, 2,
     3]
}
"#
        );
        let mut file = Claude
            .open(Path::new("f"), Some(written.as_bytes()))
            .unwrap();
        assert_eq!(file.entry("theirs").unwrap(), theirs);
        assert_eq!(file.names(), ["docs", "theirs", "events"]);
        assert!(file.remove("docs"));
        assert_eq!(file.names(), ["theirs", "events"]);
        assert!(file.remove("events"));
        assert_eq!(
            String::from_utf8(file.to_bytes()).unwrap(),
            r#"{
  "$schema": "https://example.com/schema.json",
  "mcpServers": {
    "theirs": {
        "command" : "their-tool",   "args": [ "--fast" ]
      }
  },
  "zeta": [1, 2,
     3]
}
"#
        );
    }

    #[test]
    fn a_jsonc_file_is_refused_not_rewritten() {
        let cases = [
            ("{\n  // mine\n  \"mcpServers\": {}\n}", "line 2 column 3"),
            (
                "{\"mcpServers\": {\"a\": {\"command\": \"x\"},}}",
                "line 1 column 39",
            ),
            ("\u{feff}{\"mcpServers\": {}}", "line 1 column 1"),
        ];
        for vendor in [&Claude as &dyn Vendor, &Antigravity] {
            for (text, at) in cases {
                let err = vendor.open(Path::new("p/f.json"), Some(text.as_bytes()));
                let err = err.unwrap_err().to_string();
                assert!(
                    err.starts_with("p/f.json: it is not strict JSON ("),
                    "{err}"
                );
                assert!(err.contains(at), "{err}");
                assert!(
                    err.ends_with(
                        "), so fl will not rewrite it and lose its comments, trailing commas or \
                         byte-order mark. Remove them by hand, then run `fl mcp sync` again"
                    ),
                    "{err}"
                );
            }
        }
        let err = Claude
            .open(Path::new("p/f.json"), Some(b"{\"a\": \"\xff\"}"))
            .unwrap_err();
        assert!(err.problem.starts_with("it is not strict JSON ("), "{err}");
    }

    #[test]
    fn a_json_file_that_is_not_an_object_of_servers_is_refused() {
        let cases = [
            ("[]", "its top level is not a JSON object"),
            ("{\"mcpServers\": []}", "`mcpServers` is not a JSON object"),
            (
                "{\"a\": 1, \"a\": 2}",
                "the key `a` appears twice at the top level",
            ),
            (
                "{\"mcpServers\": {\"x\": {}, \"x\": {}}}",
                "the server `x` appears twice in `mcpServers`",
            ),
        ];
        for (text, problem) in cases {
            let err = Claude
                .open(Path::new("p/f.json"), Some(text.as_bytes()))
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("p/f.json: {problem}. Fix it by hand, then run `fl mcp sync` again")
            );
        }
    }

    #[test]
    fn a_missing_file_starts_with_empty_mcp_servers() {
        let file = Claude.open(Path::new("f"), None).unwrap();
        assert!(file.names().is_empty());
        assert_eq!(file.to_bytes(), b"{\n  \"mcpServers\": {}\n}\n");
    }

    #[test]
    fn a_file_without_mcp_servers_gains_it_only_when_an_entry_is_added() {
        let text = "{\"theme\": \"dark\"}";
        let mut file = Claude.open(Path::new("f"), Some(text.as_bytes())).unwrap();
        assert!(file.names().is_empty());
        assert!(!file.remove("files"));
        assert_eq!(file.to_bytes(), b"{\n  \"theme\": \"dark\"\n}\n");
        file.set(
            "files",
            &Claude.render("files", &server(ONLY_SECRET)).unwrap(),
        );
        assert_eq!(
            String::from_utf8(file.to_bytes()).unwrap(),
            r#"{
  "theme": "dark",
  "mcpServers": {
    "files": {
      "command": "files-mcp",
      "env": {
        "FILES_TOKEN": "${FILES_TOKEN}"
      }
    }
  }
}
"#
        );
        let empty = Claude.open(Path::new("f"), Some(b"{}")).unwrap();
        assert_eq!(empty.to_bytes(), b"{}\n");
    }
}
