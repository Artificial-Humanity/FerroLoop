//! `fl --version`, `fl --version --json` and `fl version [--json]`: one formatter behind all
//! three, so they cannot disagree.
//!
//! Plain output is the build's one human line (`fl 0.1.0 (abc123456789, dirty)`). The JSON is
//! the build's [`BuildInfo`] plus the protocol, schema and store-format versions this build
//! speaks, each read from the constant the code checks by:
//!
//! - `{"min": a, "max": b}`: a range the code opens (it refuses a value outside it).
//! - `{"current": n}`: a version the code matches EXACTLY (it refuses any other).

use std::ffi::OsString;
use std::sync::OnceLock;

use fl_core::build_info::BuildInfo;
use serde_json::{Value, json};

/// The binary name the report carries.
pub const NAME: &str = "fl";

/// This build's stamp.
pub fn build_info() -> BuildInfo {
    fl_core::build_info!()
}

/// `0.1.0 (abc123456789, dirty)`: clap's `--version` prints `fl ` before it.
pub fn clap_version() -> &'static str {
    static LINE: OnceLock<String> = OnceLock::new();
    LINE.get_or_init(|| {
        let b = build_info();
        format!("{} ({})", b.version, b.note())
    })
}

/// The one-line human report.
pub fn human() -> String {
    build_info().human_line(NAME)
}

/// The versions this build speaks, beyond its own stamp.
fn capabilities() -> (Value, Value, Value) {
    let protocol = json!({
        // The peer handshake refuses any version but this one.
        "ferrowire": { "current": fl_wire::peer::WIRE_PROTOCOL_VERSION },
    });
    let schema = json!({
        // A manifest outside the range is refused.
        "manifest": {
            "min": fl_store::manifest::MANIFEST_FORMAT_WITHOUT_LEDGER,
            "max": fl_store::manifest::MANIFEST_FORMAT,
        },
        // A warrant ledger or a vault file of any other version is refused.
        "warrant_ledger": { "current": fl_gate::warrant::WARRANT_SCHEMA_VERSION },
        "vault": { "current": fl_gate::VAULT_SCHEMA_VERSION },
    });
    (protocol, schema, fl_store::formats::store_formats())
}

/// The full machine-readable report.
pub fn json_report() -> Value {
    let mut v = build_info().to_json(NAME);
    let (protocol, schema, store_formats) = capabilities();
    if let Value::Object(o) = &mut v {
        o.insert("protocol".into(), protocol);
        o.insert("schema".into(), schema);
        o.insert("store_formats".into(), store_formats);
    }
    v
}

/// The text `fl version` / `fl --version` print: the human line, or one JSON object.
pub fn render(json: bool) -> String {
    if json {
        format!("{}\n", json_report())
    } else {
        format!("{}\n", human())
    }
}

/// `Some(json)` when `args` (after the program name) are exactly a version request that needs
/// handling here: `--version --json` (or `-V --json`), in either order. Everything else,
/// including a bare `--version`, goes to clap, which prints the same human line.
pub fn json_flag_request(args: &[OsString]) -> bool {
    let strs: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
    strs.len() == 2
        && args.len() == 2
        && strs.contains(&"--json")
        && (strs.contains(&"--version") || strs.contains(&"-V"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn only_a_json_version_request_is_intercepted() {
        assert!(json_flag_request(&a(&["--version", "--json"])));
        assert!(json_flag_request(&a(&["--json", "--version"])));
        assert!(json_flag_request(&a(&["-V", "--json"])));
        assert!(!json_flag_request(&a(&["--version"])));
        assert!(!json_flag_request(&a(&["-V"])));
        assert!(!json_flag_request(&a(&["--json"])));
        assert!(!json_flag_request(&a(&["--version", "--json", "x"])));
        assert!(!json_flag_request(&a(&["--version", "--version"])));
        assert!(!json_flag_request(&a(&["gate", "--json", "--version"])));
        assert!(!json_flag_request(&[]));
    }

    #[test]
    fn the_report_reads_the_constants_the_code_checks_by() {
        let v = json_report();
        assert_eq!(v["name"], "fl");
        assert_eq!(v["source"], "source-build");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            v["protocol"]["ferrowire"]["current"],
            fl_wire::peer::WIRE_PROTOCOL_VERSION
        );
        assert_eq!(
            v["schema"]["manifest"],
            json!({"min": fl_store::manifest::MANIFEST_FORMAT_WITHOUT_LEDGER,
                   "max": fl_store::manifest::MANIFEST_FORMAT})
        );
        assert_eq!(v["store_formats"], fl_store::formats::store_formats());
    }

    #[test]
    fn plain_and_json_render_the_same_build() {
        let human = render(false);
        assert!(human.starts_with("fl "), "{human}");
        assert!(human.ends_with(")\n"), "{human}");
        let v: Value = serde_json::from_str(&render(true)).unwrap();
        assert!(human.contains(v["commit_short"].as_str().unwrap()));
        // clap prints `fl <clap_version>`; it is the human line.
        assert_eq!(format!("{NAME} {}\n", clap_version()), human);
    }
}
