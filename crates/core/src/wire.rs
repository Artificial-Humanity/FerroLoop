//! One place that defines how an enum spells itself on the outside.
//!
//! An enum that crosses the process boundary has exactly one spelling, and
//! that spelling is snake_case (owner, 2026-09-21). "Outside" means all four
//! of these at once: the JSON a command prints, the bytes in the store, the
//! text a human reads, and the value the CLI accepts back.
//!
//! [`wire_names!`] generates `ALL`, `as_wire`, `from_wire` and `wire_values`
//! from a single variant-to-string list, and generates the test that pins
//! them. Two properties follow from that, and both are the point:
//!
//! 1. **A new variant cannot skip the list.** The generated `as_wire` is a
//!    `match` over the list, so a variant the list omits is a non-exhaustive
//!    match — `E0004`, at compile time.
//! 2. **A new variant cannot carry the wrong spelling.** It lands in `ALL`
//!    automatically, and the generated test walks `ALL` asserting that serde's
//!    form and `as_wire` are the same string. A hand-listed test would have
//!    shipped green; this one cannot.

/// Give an enum its wire form. See the module docs.
///
/// `$tests` names the generated test module — a module and a type share one
/// namespace, so it cannot simply reuse the enum's name.
macro_rules! wire_names {
    ($name:ident as $tests:ident { $( $variant:ident => $wire:literal ),+ $(,)? }) => {
        impl $name {
            /// Every variant, in declaration order. Generated, so it cannot
            /// fall behind the enum.
            pub const ALL: &'static [Self] = &[ $( Self::$variant ),+ ];

            /// The one spelling: what this prints, what the store holds, and
            /// what the CLI accepts. Never render one of these with `Debug` —
            /// that spells a Rust identifier, and the two would drift.
            pub fn as_wire(self) -> &'static str {
                match self { $( Self::$variant => $wire, )+ }
            }

            pub fn from_wire(s: &str) -> Option<Self> {
                Some(match s {
                    $( $wire => Self::$variant, )+
                    _ => return None,
                })
            }

            /// The accepted values, for a refusal that tells the reader what
            /// to type instead. Generated from the same list, so a refusal
            /// cannot name a value the parser rejects.
            pub fn wire_values() -> String {
                Self::ALL
                    .iter()
                    .map(|v| v.as_wire())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        }

        #[cfg(test)]
        mod $tests {
            use super::$name;

            #[test]
            fn the_serde_form_and_the_wire_name_are_the_same_string() {
                for v in $name::ALL {
                    assert_eq!(
                        serde_json::to_string(v).expect("serialize"),
                        format!("\"{}\"", v.as_wire()),
                        "{v:?} serializes to a different string than it prints"
                    );
                }
            }

            #[test]
            fn every_wire_name_round_trips_and_is_snake_case() {
                for v in $name::ALL {
                    let w = v.as_wire();
                    assert_eq!($name::from_wire(w), Some(*v), "{w} did not round trip");
                    assert!(
                        !w.is_empty()
                            && w.chars()
                                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                        "`{w}` is not snake_case, so this variant has two spellings"
                    );
                }
                assert_eq!($name::from_wire("no such value"), None);
            }

            #[test]
            fn wire_values_lists_exactly_what_from_wire_accepts() {
                // Reads every value the refusal offers and feeds it back to
                // the parser. A refusal that names something `from_wire`
                // rejects is worse than no refusal — it sends the reader to
                // a value that will fail again.
                let listed = $name::wire_values();
                let offered: Vec<&str> = listed.split(", ").collect();
                for value in &offered {
                    assert!(
                        $name::from_wire(value).is_some(),
                        "a refusal offers `{value}`, which the parser rejects"
                    );
                }
                for v in $name::ALL {
                    assert!(
                        offered.contains(&v.as_wire()),
                        "a refusal would not mention `{}`",
                        v.as_wire()
                    );
                }
                assert_eq!(offered.len(), $name::ALL.len());
            }
        }
    };
}

/// The same guard for an enum that carries data, and so has no `as_wire`.
///
/// Its variants are not values a user types, so there is nothing to parse
/// back — but the serialized **tag** still crosses the boundary, and it is
/// what `gate show` prints. Each arm gives a pattern (which makes the
/// generated match exhaustive, so a new variant is `E0004`), the tag it must
/// serialize under, and a sample value to serialize.
macro_rules! wire_tags {
    ($name:ident as $tests:ident { $( $pat:pat => $wire:literal , $sample:expr );+ $(;)? }) => {
        #[cfg(test)]
        mod $tests {
            #[allow(unused_imports)]
            use super::*;

            /// A variant absent from the list above makes this match
            /// non-exhaustive: `E0004`, at compile time.
            #[allow(dead_code)]
            fn every_variant_is_listed(v: &$name) {
                match v { $( $pat => (), )+ }
            }

            #[test]
            fn every_variant_serializes_under_its_snake_case_tag() {
                $(
                    let value: $name = $sample;
                    let json = serde_json::to_string(&value).expect("serialize");
                    let tag = json
                        .trim_start_matches('{')
                        .split('"')
                        .nth(1)
                        .unwrap_or_else(|| panic!("not an externally tagged enum: {json}"));
                    assert_eq!(tag, $wire, "wrong tag in {json}");
                    assert!(
                        !tag.is_empty()
                            && tag.chars().all(|c| {
                                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
                            }),
                        "`{tag}` is not snake_case, so this variant has two spellings"
                    );
                )+
            }
        }
    };
}

pub(crate) use wire_names;
pub(crate) use wire_tags;
