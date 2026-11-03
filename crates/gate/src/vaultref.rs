//! `vault://NAME[#field]` — a reference to a FerroGate Vault secret that is not a placeholder.
//!
//! A [`VaultRef`] names a secret and, optionally, the one field of it a caller wants. It carries
//! no value and no policy: the Vault decides whether the caller may use it. The grammar is
//! deliberately narrow, so one secret has one spelling and nothing but a name can ride in a
//! reference:
//!
//! ```text
//! vault-ref = "vault://" NAME [ "#" FIELD ]
//! NAME      = 1*64 ( "A".."Z" / "0".."9" / "_" )
//! FIELD     = "pem"
//! ```
//!
//! [`parse_vault_ref`] accepts that and nothing else: no lowercase name (the Vault upper-cases
//! names, but a reference is canonical), no space or control character anywhere, no `/`, `?`, or
//! second `#`, no other scheme, no other field. A [`VaultRefError`] says what kind of mistake it
//! was and echoes none of the input: a reference that holds a pasted secret is never repeated.

use std::fmt;

/// The longest secret name, as in [`crate::normalize_secret_name`].
pub const MAX_NAME_LEN: usize = 64;

const SCHEME: &str = "vault://";

/// The part of a secret a reference asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VaultField {
    /// The secret's value is a PEM-encoded private key, used in place (never returned).
    Pem,
}

impl VaultField {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pem => "pem",
        }
    }
}

/// A parsed `vault://NAME[#field]` reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VaultRef {
    name: String,
    field: Option<VaultField>,
}

impl VaultRef {
    /// The secret's name: upper-case ASCII letters, digits and `_`, 1 to 64 characters.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The requested field, if the reference names one.
    pub fn field(&self) -> Option<VaultField> {
        self.field
    }
}

impl fmt::Display for VaultRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SCHEME}{}", self.name)?;
        if let Some(field) = self.field {
            write!(f, "#{}", field.as_str())?;
        }
        Ok(())
    }
}

/// Why a string is not a [`VaultRef`]. No variant holds any of the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VaultRefError {
    #[error("a vault reference starts with `vault://`")]
    Scheme,
    #[error("a vault reference holds a space, control, or non-ASCII character")]
    Character,
    #[error("a vault reference names no secret")]
    EmptyName,
    #[error("a vault reference name is longer than {MAX_NAME_LEN} characters")]
    NameTooLong,
    #[error("a vault reference name may hold only `A-Z`, `0-9`, and `_`")]
    NameCharacter,
    #[error("a vault reference holds a second `#`")]
    ExtraFragment,
    #[error("a vault reference field is empty or is not one of: pem")]
    Field,
}

/// Parse `raw` as `vault://NAME[#field]`. See the module docs for the grammar.
pub fn parse_vault_ref(raw: &str) -> Result<VaultRef, VaultRefError> {
    // Whitespace, controls and non-ASCII first: they would otherwise hide in a field or a name.
    if raw
        .chars()
        .any(|c| !c.is_ascii() || c.is_ascii_control() || c == ' ')
    {
        return Err(VaultRefError::Character);
    }
    let Some(rest) = raw.strip_prefix(SCHEME) else {
        return Err(VaultRefError::Scheme);
    };
    let (name, field) = match rest.split_once('#') {
        Some((name, field)) => {
            if field.contains('#') {
                return Err(VaultRefError::ExtraFragment);
            }
            let field = match field {
                "pem" => VaultField::Pem,
                _ => return Err(VaultRefError::Field),
            };
            (name, Some(field))
        }
        None => (rest, None),
    };
    if name.is_empty() {
        return Err(VaultRefError::EmptyName);
    }
    if name.len() > MAX_NAME_LEN {
        return Err(VaultRefError::NameTooLong);
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(VaultRefError::NameCharacter);
    }
    Ok(VaultRef {
        name: name.to_string(),
        field,
    })
}

impl std::str::FromStr for VaultRef {
    type Err = VaultRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_vault_ref(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_formed_references_parse_and_round_trip() {
        for (raw, name, field) in [
            ("vault://A", "A", None),
            ("vault://GITHUB_APP_KEY", "GITHUB_APP_KEY", None),
            (
                "vault://GITHUB_APP_KEY#pem",
                "GITHUB_APP_KEY",
                Some(VaultField::Pem),
            ),
            ("vault://_", "_", None),
            ("vault://0", "0", None),
            ("vault://K9_9#pem", "K9_9", Some(VaultField::Pem)),
        ] {
            let parsed = parse_vault_ref(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(parsed.name(), name, "{raw}");
            assert_eq!(parsed.field(), field, "{raw}");
            assert_eq!(parsed.to_string(), raw, "Display must round-trip {raw}");
            assert_eq!(raw.parse::<VaultRef>().unwrap(), parsed);
        }
        let longest = format!("vault://{}", "A".repeat(MAX_NAME_LEN));
        assert_eq!(parse_vault_ref(&longest).unwrap().to_string(), longest);
    }

    #[test]
    fn malformed_references_are_refused_with_the_right_kind() {
        let too_long = format!("vault://{}", "A".repeat(MAX_NAME_LEN + 1));
        let cases: Vec<(&str, VaultRefError)> = vec![
            ("", VaultRefError::Scheme),
            ("vault:/NAME", VaultRefError::Scheme),
            ("vault:NAME", VaultRefError::Scheme),
            ("VAULT://NAME", VaultRefError::Scheme),
            ("Vault://NAME", VaultRefError::Scheme),
            ("http://NAME", VaultRefError::Scheme),
            ("file://NAME", VaultRefError::Scheme),
            ("NAME", VaultRefError::Scheme),
            ("xvault://NAME", VaultRefError::Scheme),
            ("vault://", VaultRefError::EmptyName),
            ("vault://#pem", VaultRefError::EmptyName),
            ("vault://name", VaultRefError::NameCharacter),
            ("vault://Name", VaultRefError::NameCharacter),
            ("vault://NAME/", VaultRefError::NameCharacter),
            ("vault://NAME/EXTRA", VaultRefError::NameCharacter),
            ("vault:///NAME", VaultRefError::NameCharacter),
            ("vault://NAME?x=1", VaultRefError::NameCharacter),
            ("vault://NAME?", VaultRefError::NameCharacter),
            ("vault://NA-ME", VaultRefError::NameCharacter),
            ("vault://NA.ME", VaultRefError::NameCharacter),
            ("vault://NAME:80", VaultRefError::NameCharacter),
            ("vault://user@NAME", VaultRefError::NameCharacter),
            ("vault://NAME%41", VaultRefError::NameCharacter),
            ("vault://NAME#", VaultRefError::Field),
            ("vault://NAME#PEM", VaultRefError::Field),
            ("vault://NAME#Pem", VaultRefError::Field),
            ("vault://NAME#key", VaultRefError::Field),
            ("vault://NAME#pem/x", VaultRefError::Field),
            ("vault://NAME#pem?x", VaultRefError::Field),
            ("vault://NAME#pem#pem", VaultRefError::ExtraFragment),
            ("vault://NAME##pem", VaultRefError::ExtraFragment),
            ("vault://NAME#a#b", VaultRefError::ExtraFragment),
            (" vault://NAME", VaultRefError::Character),
            ("vault://NAME ", VaultRefError::Character),
            ("vault://NAME\n", VaultRefError::Character),
            ("vault://NAME\r\n", VaultRefError::Character),
            ("vault://NAME\t", VaultRefError::Character),
            ("vault://NA ME", VaultRefError::Character),
            ("vault://NAME#pem ", VaultRefError::Character),
            ("vault://NAME\0", VaultRefError::Character),
            ("vault://NAME\u{7f}", VaultRefError::Character),
            ("vault://N\u{00c4}ME", VaultRefError::Character),
            ("vault://NAME\u{200b}", VaultRefError::Character),
            ("vault://\u{ff21}", VaultRefError::Character),
            (too_long.as_str(), VaultRefError::NameTooLong),
        ];
        for (raw, want) in cases {
            assert_eq!(parse_vault_ref(raw), Err(want), "{raw:?}");
        }
    }

    #[test]
    fn an_error_never_echoes_the_input() {
        let secret = "ghp_SuperSecretTokenValue1234567890";
        for raw in [
            format!("vault://{secret}"),
            format!("vault://NAME#{secret}"),
            secret.to_string(),
            format!("vault://NAME/{secret}"),
            format!("vault://NAME #{secret}"),
        ] {
            let err = parse_vault_ref(&raw).unwrap_err();
            for text in [err.to_string(), format!("{err:?}")] {
                assert!(!text.contains(secret), "{text}");
                assert!(!text.contains("SuperSecret"), "{text}");
            }
        }
    }

    #[test]
    fn arbitrary_short_strings_never_panic_and_only_the_grammar_parses() {
        // Every string over a small alphabet up to length 9, wrapped after the scheme: whatever
        // parses must re-display to the same string and hold only grammar characters.
        let alphabet = ['A', 'z', '0', '_', '#', '/', '?', ' ', '\n', '\u{e9}', 'p'];
        let mut stack: Vec<String> = vec![String::new()];
        let mut parsed = 0usize;
        while let Some(tail) = stack.pop() {
            let raw = format!("vault://{tail}");
            if let Ok(r) = parse_vault_ref(&raw) {
                parsed += 1;
                assert_eq!(r.to_string(), raw);
                assert!(
                    r.name()
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                );
                assert!(!r.name().is_empty());
            }
            if tail.chars().count() < 4 {
                for c in alphabet {
                    stack.push(format!("{tail}{c}"));
                }
            }
        }
        assert!(
            parsed > 10,
            "the sweep must reach valid references: {parsed}"
        );
    }
}
