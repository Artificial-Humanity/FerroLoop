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
                let listed = $name::wire_values();
                for v in $name::ALL {
                    assert!(
                        listed.split(", ").any(|s| s == v.as_wire()),
                        "a refusal would not mention `{}`",
                        v.as_wire()
                    );
                }
                assert_eq!(listed.split(", ").count(), $name::ALL.len());
            }
        }
    };
}

pub(crate) use wire_names;
