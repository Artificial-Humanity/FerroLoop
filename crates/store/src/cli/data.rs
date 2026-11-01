//! `get`, `put` and `query`: one document read or written, or one query run,
//! as JSON on stdout, on a store named by a path (embedded) or a URI (a
//! client of a `serve`d store).

use super::{Error, Output, Result, Wording, line, present};
use crate::{BackendKind, ConnectOptions, FerroStore, physical_path};
use serde::Serialize;
use serde::ser::SerializeMap;
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The default number of hits a BM25 or vector query returns.
pub const DEFAULT_K: usize = 10;

/// The store a command works on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A store path: the `.fls` it names (`x.redb` names `x.fls`), opened
    /// embedded.
    Path(PathBuf),
    /// A `unix://`, `ws://` or `wss://` URI of a served store.
    Uri(String),
}

impl Target {
    /// A URI when `raw` starts `unix://`, `ws://` or `wss://`; any other
    /// `scheme://` is refused; anything else is a path.
    pub fn parse(raw: &str) -> Result<Self> {
        if ["unix://", "ws://", "wss://"]
            .iter()
            .any(|scheme| raw.starts_with(scheme))
        {
            return Ok(Self::Uri(raw.to_string()));
        }
        if let Some((scheme, _)) = raw.split_once("://")
            && !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        {
            refuse!(
                "`{raw}` is not a store this command opens: give a path, or a unix://, ws:// \
                 or wss:// URI"
            );
        }
        Ok(Self::Path(PathBuf::from(raw)))
    }
}

/// How a client authenticates to, and checks, a served store: the files and
/// names of [`ConnectOptions`]. Every field is for a URI; a path takes none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    /// The PSK file (64+ hex digits, mode 0600), with `server_id`.
    pub psk_file: Option<PathBuf>,
    /// The server's name, as it was set with its PSK.
    pub server_id: Option<String>,
    /// A bearer token file (the token on its first line, mode 0600).
    pub token_file: Option<PathBuf>,
    /// `wss://`: trust only the CAs in this PEM file.
    pub ca_file: Option<PathBuf>,
    /// `wss://`: trust the one server key with this SPKI SHA-256.
    pub pin_spki: Option<String>,
    /// Send a credential over plain `ws://` to an address that is not
    /// loopback.
    pub allow_plaintext: bool,
}

impl Credentials {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The [`ConnectOptions`] these name. A PSK needs its server id, and one
    /// method at most is named.
    pub fn connect_options(&self) -> Result<ConnectOptions> {
        let base = match (&self.psk_file, &self.server_id, &self.token_file) {
            (Some(_), _, Some(_)) => {
                refuse!("--psk-file and --token-file name two credentials: give one")
            }
            (Some(psk), Some(id), None) => ConnectOptions::psk_file(id.clone(), psk.clone()),
            (Some(_), None, None) => {
                refuse!("--psk-file needs --server-id, the name the server was given with its PSK")
            }
            (None, Some(_), _) => refuse!("--server-id goes with --psk-file"),
            (None, None, Some(token)) => ConnectOptions::token_file(token.clone()),
            (None, None, None) => ConnectOptions::new(),
        };
        let mut options = base.allow_plaintext(self.allow_plaintext);
        if let Some(ca) = &self.ca_file {
            options = options.ca_file(ca.clone());
        }
        if let Some(pin) = &self.pin_spki {
            options = options.pin_spki_sha256(pin.clone());
        }
        Ok(options)
    }
}

/// Open the FerroStore `target` names. A path is opened embedded, on the
/// ferro engine whatever the environment says: a lone `.redb` is refused
/// (never migrated), and a missing `.fls` is created only when `create`
/// (else refused, naming `what`, the command). A URI connects with
/// `credentials`, which a path refuses.
pub fn open_target(
    target: &Target,
    credentials: &Credentials,
    create: bool,
    what: &str,
    w: &Wording,
) -> Result<FerroStore> {
    match target {
        Target::Path(path) => {
            if !credentials.is_empty() {
                refuse!(
                    "--psk-file, --token-file, --server-id, --ca-file, --pin-spki and \
                     --allow-plaintext are for a unix://, ws:// or wss:// URI, not a path"
                );
            }
            open_local(path, create, what, w)
        }
        Target::Uri(uri) => {
            let options = credentials.connect_options()?;
            FerroStore::connect_with(uri, &options)
                .map_err(|e| Error::context(format!("could not connect to {uri}"), e))
        }
    }
}

/// The `.fls` that `path` names, opened embedded. See [`open_target`].
pub(crate) fn open_local(path: &Path, create: bool, what: &str, w: &Wording) -> Result<FerroStore> {
    let physical = physical_path(path, BackendKind::Ferro);
    if !present(&physical) {
        if path != physical && path.exists() {
            refuse!(
                "{} holds a redb store and {} does not exist yet: migrate it first (`{} migrate`)",
                path.display(),
                physical.display(),
                w.command
            );
        }
        if !create {
            refuse!(
                "no store at {}; `{} {what}` does not create one",
                physical.display(),
                w.command
            );
        }
    }
    Ok(FerroStore::open(&physical)?)
}

/// Refuse a reserved (`__`) collection, as the store refuses a write to one.
fn refuse_reserved(collection: &str, what: &str, w: &Wording) -> Result<()> {
    if collection.starts_with("__") {
        refuse!(
            "`{} {what}` refused: collection `{collection}` is reserved (a `__` collection \
             belongs to the store itself)",
            w.command
        );
    }
    Ok(())
}

// ---- get ----

/// What `get` found.
#[derive(Debug, Clone, PartialEq)]
pub struct GetReport {
    pub collection: String,
    pub id: String,
    pub doc: Option<Value>,
}

/// The document `(collection, id)`.
pub fn get(store: &FerroStore, collection: &str, id: &str, w: &Wording) -> Result<GetReport> {
    refuse_reserved(collection, "get", w)?;
    Ok(GetReport {
        collection: collection.to_string(),
        id: id.to_string(),
        doc: store.get_doc(collection, id)?,
    })
}

impl GetReport {
    /// The document as pretty JSON on stdout (exit 0), or a line on stderr
    /// when there is none (exit 1).
    pub fn output(&self) -> Result<(Output, i32)> {
        let mut out = Output::default();
        match &self.doc {
            Some(doc) => {
                line(&mut out.stdout, super::to_json(doc)?);
                Ok((out, 0))
            }
            None => {
                line(
                    &mut out.stderr,
                    format!(
                        "no document `{}` in collection `{}`",
                        self.id, self.collection
                    ),
                );
                Ok((out, 1))
            }
        }
    }
}

// ---- put ----

/// The document a `put` argument gives: the JSON text itself, or `-` for
/// the JSON on `stdin`.
pub fn read_doc(arg: &str, stdin: impl Read) -> Result<Value> {
    let text = if arg == "-" {
        let mut text = String::new();
        let mut stdin = stdin;
        stdin
            .read_to_string(&mut text)
            .map_err(|e| Error::context("could not read the document from stdin", e))?;
        text
    } else {
        arg.to_string()
    };
    serde_json::from_str(&text).map_err(|e| Error::context("the document is not JSON", e))
}

/// What `put` wrote. JSON: `{"collection", "id", "put": true}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PutReport {
    pub collection: String,
    pub id: String,
    pub put: bool,
}

/// Put `doc` at `(collection, id)`.
pub fn put(
    store: &FerroStore,
    collection: &str,
    id: &str,
    doc: &Value,
    w: &Wording,
) -> Result<PutReport> {
    refuse_reserved(collection, "put", w)?;
    store.put_doc(collection, id, doc)?;
    Ok(PutReport {
        collection: collection.to_string(),
        id: id.to_string(),
        put: true,
    })
}

// ---- query ----

/// One query.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    /// The ids whose top-level `field` matches `value` (`query_by_field`).
    Field { field: String, value: String },
    /// The `k` best BM25 hits for `text`.
    Bm25 { text: String, k: usize },
    /// The `k` nearest vectors to `values`.
    Vector { values: Vec<f32>, k: usize },
}

impl Query {
    /// The query a command line names: `FIELD VALUE`; `--bm25 TEXT`; or
    /// `--vector --values JSON`, `-k` for the last two.
    pub fn from_args(
        bm25: bool,
        vector: bool,
        terms: &[String],
        values: Option<&str>,
        k: Option<usize>,
        w: &Wording,
    ) -> Result<Self> {
        let cmd = w.command;
        match (bm25, vector) {
            (true, true) => refuse!("--bm25 and --vector are two queries: give one"),
            (true, false) => {
                if values.is_some() {
                    refuse!("--values is for --vector");
                }
                let [text] = terms else {
                    refuse!("`{cmd} query --bm25 COLLECTION TEXT` takes one text (quote it)");
                };
                Ok(Self::Bm25 {
                    text: text.clone(),
                    k: k.unwrap_or(DEFAULT_K),
                })
            }
            (false, true) => {
                if !terms.is_empty() {
                    refuse!(
                        "`{cmd} query --vector COLLECTION --values JSON` takes the vector in \
                         --values, nothing after the collection"
                    );
                }
                let Some(raw) = values else {
                    refuse!("--vector needs --values '[1.0, 0.5, …]'");
                };
                Ok(Self::Vector {
                    values: parse_values(raw)?,
                    k: k.unwrap_or(DEFAULT_K),
                })
            }
            (false, false) => {
                if values.is_some() || k.is_some() {
                    refuse!("--values and -k are for --vector and --bm25");
                }
                let [field, value] = terms else {
                    refuse!("`{cmd} query COLLECTION FIELD VALUE` takes a field and a value");
                };
                Ok(Self::Field {
                    field: field.clone(),
                    value: value.clone(),
                })
            }
        }
    }
}

/// A JSON array of numbers, as `f32`.
fn parse_values(raw: &str) -> Result<Vec<f32>> {
    let parsed: Vec<f64> = serde_json::from_str(raw)
        .map_err(|e| Error::context("--values is not a JSON array of numbers", e))?;
    Ok(parsed.into_iter().map(|v| v as f32).collect())
}

/// What `query` found.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryReport {
    pub collection: String,
    pub query: Query,
    /// The ids of a field query, in ascending order.
    pub ids: Vec<String>,
    /// The `(id, score)` hits of a BM25 or vector query, best first.
    pub hits: Vec<(String, f64)>,
}

/// Run `query` on `collection`.
pub fn query(
    store: &FerroStore,
    collection: &str,
    query: &Query,
    w: &Wording,
) -> Result<QueryReport> {
    refuse_reserved(collection, "query", w)?;
    let (ids, hits) = match query {
        Query::Field { field, value } => {
            (store.query_by_field(collection, field, value)?, Vec::new())
        }
        Query::Bm25 { text, k } => (Vec::new(), store.search_bm25(collection, text, *k)?),
        Query::Vector { values, k } => (
            Vec::new(),
            store.search_vectors(collection, values, *k, None)?,
        ),
    };
    Ok(QueryReport {
        collection: collection.to_string(),
        query: query.clone(),
        ids,
        hits,
    })
}

#[derive(Serialize)]
struct Hit<'a> {
    id: &'a str,
    score: f64,
}

/// JSON: `collection` and `mode` (`field`, `bm25` or `vector`); then
/// `field`, `value` and `ids`; or `query` (BM25), `k` and `hits`
/// (`[{"id", "score"}]`).
impl Serialize for QueryReport {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("collection", &self.collection)?;
        let hits = || {
            self.hits
                .iter()
                .map(|(id, score)| Hit { id, score: *score })
                .collect::<Vec<_>>()
        };
        match &self.query {
            Query::Field { field, value } => {
                m.serialize_entry("mode", "field")?;
                m.serialize_entry("field", field)?;
                m.serialize_entry("value", value)?;
                m.serialize_entry("ids", &self.ids)?;
            }
            Query::Bm25 { text, k } => {
                m.serialize_entry("mode", "bm25")?;
                m.serialize_entry("query", text)?;
                m.serialize_entry("k", k)?;
                m.serialize_entry("hits", &hits())?;
            }
            Query::Vector { k, .. } => {
                m.serialize_entry("mode", "vector")?;
                m.serialize_entry("k", k)?;
                m.serialize_entry("hits", &hits())?;
            }
        }
        m.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Wording<'static> = Wording {
        command: "t",
        redb_path_hint: "h",
    };

    #[test]
    fn a_target_is_a_path_or_one_of_three_uris() {
        assert_eq!(
            Target::parse("/d/s.fls").unwrap(),
            Target::Path("/d/s.fls".into())
        );
        assert_eq!(
            Target::parse("unix:///d/s.sock").unwrap(),
            Target::Uri("unix:///d/s.sock".into())
        );
        assert_eq!(
            Target::parse("wss://h:1").unwrap(),
            Target::Uri("wss://h:1".into())
        );
        assert!(Target::parse("file:///d/s.fls").is_err());
        assert!(Target::parse("mem://").is_err());
    }

    #[test]
    fn credentials_name_one_method_and_a_psk_names_its_server() {
        let psk = Credentials {
            psk_file: Some("/k".into()),
            ..Credentials::default()
        };
        assert!(psk.connect_options().is_err());
        let both = Credentials {
            server_id: Some("s".into()),
            token_file: Some("/t".into()),
            ..psk.clone()
        };
        assert!(both.connect_options().is_err());
        let ok = Credentials {
            server_id: Some("s".into()),
            ..psk
        };
        assert!(ok.connect_options().is_ok());
    }

    #[test]
    fn a_query_is_read_from_its_arguments() {
        let terms = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            Query::from_args(false, false, &terms(&["kind", "a"]), None, None, &W).unwrap(),
            Query::Field {
                field: "kind".into(),
                value: "a".into()
            }
        );
        assert_eq!(
            Query::from_args(true, false, &terms(&["red fox"]), None, Some(3), &W).unwrap(),
            Query::Bm25 {
                text: "red fox".into(),
                k: 3
            }
        );
        assert_eq!(
            Query::from_args(false, true, &[], Some("[1, 0.5]"), None, &W).unwrap(),
            Query::Vector {
                values: vec![1.0, 0.5],
                k: DEFAULT_K
            }
        );
        assert!(Query::from_args(true, true, &[], None, None, &W).is_err());
        assert!(Query::from_args(false, false, &terms(&["only"]), None, None, &W).is_err());
        assert!(Query::from_args(false, true, &[], None, None, &W).is_err());
        assert!(Query::from_args(false, true, &[], Some("{}"), None, &W).is_err());
    }

    #[test]
    fn a_document_comes_from_the_argument_or_stdin() {
        assert_eq!(read_doc(r#"{"a":1}"#, std::io::empty()).unwrap()["a"], 1);
        assert_eq!(read_doc("-", &b"[2]"[..]).unwrap()[0], 2);
        assert!(read_doc("not json", std::io::empty()).is_err());
    }

    #[test]
    fn reserved_collections_are_refused_and_nothing_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.fls");
        let Err(err) = open_local(&path, false, "get", &W) else {
            panic!("get created a store");
        };
        let err = err.to_string();
        assert!(err.contains("does not create one"), "{err}");
        assert!(!path.exists());
        let store = open_local(&path, true, "put", &W).unwrap();
        for what in ["get", "put", "query"] {
            let err = refuse_reserved("__meta", what, &W).unwrap_err().to_string();
            assert!(err.contains("reserved"), "{err}");
        }
        assert!(get(&store, "__meta", "schema_version", &W).is_err());
        assert!(put(&store, "__meta", "x", &Value::Null, &W).is_err());
    }
}
