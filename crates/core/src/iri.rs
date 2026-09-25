//! The one id type. Every id FerroLoop stores or prints is an absolute IRI.
//!
//! ⚠ Normalization happens here, once, on the way in — `parse` and the serde
//! `Deserialize` both normalize. After that, ids compare as exact strings, so
//! a comparison can never run on an id that was not normalized (spec §2.3).
//!
//! This is deliberately not a full RFC 3987 parser. It accepts what an id
//! needs to be — `scheme ":" rest`, no whitespace — and refuses the rest by
//! name. Resolving an id to a store is a separate step (spec §2.4).

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Iri(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IriError {
    #[error("an id cannot be empty")]
    Empty,
    #[error("`{0}` contains whitespace, and an id cannot")]
    Whitespace(String),
    #[error(
        "`{0}` is not an absolute IRI: it needs a scheme, a colon, and something after it (such as `urn:uuid:…`)"
    )]
    NotAbsolute(String),
}

impl Iri {
    pub fn parse(input: &str) -> Result<Self, IriError> {
        if input.is_empty() {
            return Err(IriError::Empty);
        }
        if input.chars().any(char::is_whitespace) {
            return Err(IriError::Whitespace(input.to_string()));
        }
        let not_absolute = || IriError::NotAbsolute(input.to_string());
        let (scheme, rest) = input.split_once(':').ok_or_else(not_absolute)?;
        let mut chars = scheme.chars();
        let scheme_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if !scheme_ok || rest.is_empty() {
            return Err(not_absolute());
        }
        if scheme.eq_ignore_ascii_case("urn") {
            // `urn:<nid>:<nss>` needs both parts.
            match rest.split_once(':') {
                Some((nid, nss)) if !nid.is_empty() && !nss.is_empty() => {}
                _ => return Err(not_absolute()),
            }
        }
        Ok(Self(normalize(scheme, rest)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn scheme(&self) -> &str {
        self.0.split_once(':').map(|(s, _)| s).unwrap_or("")
    }
}

/// The scheme is case-insensitive everywhere. For `urn:` the namespace id is
/// too (RFC 8141), and for `urn:uuid:` so is the hex. For `http(s)` the host
/// is. Nothing else is touched: a path is case-sensitive.
fn normalize(scheme: &str, rest: &str) -> String {
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
        "urn" => {
            let (nid, nss) = rest.split_once(':').expect("checked in parse");
            let nid = nid.to_ascii_lowercase();
            let nss = if nid == "uuid" {
                nss.to_ascii_lowercase()
            } else {
                nss.to_string()
            };
            format!("urn:{nid}:{nss}")
        }
        "http" | "https" => match rest.strip_prefix("//") {
            Some(after) => {
                let end = after.find(['/', '?', '#']).unwrap_or(after.len());
                let (authority, tail) = after.split_at(end);
                format!("{scheme}://{}{tail}", authority.to_ascii_lowercase())
            }
            None => format!("{scheme}:{rest}"),
        },
        _ => format!("{scheme}:{rest}"),
    }
}

impl fmt::Display for Iri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for Iri {
    type Err = IriError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Iri {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Iri {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Iri::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_urn_uuid_is_folded_to_lowercase() {
        let a = Iri::parse("URN:UUID:0190A1B2-C3D4-7E5F-8A6B-7C8D9E0F1A2B").unwrap();
        assert_eq!(a.as_str(), "urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b");
    }

    #[test]
    fn normalizing_twice_changes_nothing() {
        for raw in [
            "URN:UUID:0190A1B2-C3D4-7E5F-8A6B-7C8D9E0F1A2B",
            "HTTPS://GitHub.COM/Artificial-Humanity/FerroLoop/issues/41",
            "urn:isbn:0451450523",
        ] {
            let once = Iri::parse(raw).unwrap();
            let twice = Iri::parse(once.as_str()).unwrap();
            assert_eq!(once, twice, "{raw}");
        }
    }

    #[test]
    fn an_https_iri_folds_scheme_and_host_but_not_path() {
        let a = Iri::parse("HTTPS://GitHub.COM/Artificial-Humanity/FerroLoop/issues/41").unwrap();
        assert_eq!(
            a.as_str(),
            "https://github.com/Artificial-Humanity/FerroLoop/issues/41"
        );
    }

    #[test]
    fn another_urn_namespace_keeps_its_nss_case() {
        let a = Iri::parse("URN:Example:MixedCase").unwrap();
        assert_eq!(a.as_str(), "urn:example:MixedCase");
    }

    #[test]
    fn what_is_not_an_absolute_iri_is_refused_by_name() {
        for bad in [
            "",
            "3",
            "3abc",
            "no-colon",
            ":rest",
            "1http:x",
            "urn:",
            "urn:uuid:a b",
        ] {
            let err = Iri::parse(bad).expect_err(bad);
            if !bad.is_empty() {
                assert!(err.to_string().contains(bad), "{bad}: {err}");
            }
        }
    }

    #[test]
    fn the_wire_form_is_a_plain_string_and_is_normalized_on_the_way_in() {
        let a: Iri =
            serde_json::from_str("\"URN:UUID:0190A1B2-C3D4-7E5F-8A6B-7C8D9E0F1A2B\"").unwrap();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            "\"urn:uuid:0190a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b\""
        );
        assert!(serde_json::from_str::<Iri>("\"not an iri\"").is_err());
        assert!(serde_json::from_str::<Iri>("3").is_err());
    }
}
