//! The environment a spawned vendor CLI sees.
//!
//! A vendor agent CLI (`claude`, `codex`, `agy`, `muse`, `grok`) runs code that fl does not
//! control, and so does every tool it starts. The hub process holds secrets in its environment
//! (`FL_VAULT_MASTER_KEY`, `FL_HUB_*`, `FL_GITHUB_*`, `GITHUB_TOKEN`, other vendors' API keys,
//! `FERROGATE_*`), so a child must never inherit it. [`child_env`] builds the child's whole
//! environment from an explicit allow-list; the spawn site calls `env_clear()` and then applies
//! exactly that list. Nothing else gets through.
//!
//! # What passes
//!
//! A variable reaches the child only if its name is in one of these three sets, and only if
//! no denied prefix matches it.
//!
//! 1. **Baseline** ([`BASELINE_EXACT`], [`BASELINE_PREFIXES`]), the same for every dialect:
//!    - process basics: `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `TERM`,
//!      `COLORTERM`, `NO_COLOR`, `TZ`;
//!    - locale: `LANG`, `LANGUAGE`, `LC_*`;
//!    - the vendors' own config homes: `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`,
//!      `XDG_CACHE_HOME`, `XDG_STATE_HOME`;
//!    - the agent's identity on the wire, which `agent-msg` inside the child needs:
//!      `AGENT_PERSONA`, `AGENT_NAME` (a name, not a secret);
//!    - toolchain and certificate settings that are not secrets and that a build or a test run
//!      inside the child needs: `CARGO_HOME`, `CARGO_TARGET_DIR`, `RUSTUP_HOME`, `NVM_DIR`,
//!      `PNPM_HOME`, `BUN_INSTALL`, `GOPATH`, `GOROOT`, `JAVA_HOME`, `PYENV_ROOT`,
//!      `VIRTUAL_ENV`, `PYTHONPATH`, `PYTHONHOME`, `NODE_PATH`, `NODE_OPTIONS`,
//!      `NPM_CONFIG_PREFIX`, `NPM_CONFIG_CACHE`, `LD_LIBRARY_PATH`, `NIX_PATH`, `NIX_PROFILES`,
//!      `NIX_SSL_CERT_FILE`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, `NODE_EXTRA_CA_CERTS`,
//!      `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`.
//! 2. **The dialect's own credentials** ([`dialect_prefixes`]), by prefix, and only the
//!    dialect's own: `ANTHROPIC_*` and `CLAUDE_*` for Claude, `OPENAI_*` and `CODEX_*` for
//!    Codex, `GEMINI_*`, `GOOGLE_*`, `ANTIGRAVITY_*` and `AGY_*` for Antigravity, `XAI_*` and
//!    `GROK_*` for Grok, `MUSE_*` for Muse. These include endpoint overrides such as
//!    `ANTHROPIC_BASE_URL`. A Claude child never sees `OPENAI_API_KEY`.
//! 3. **Opt-in names** (`pass`): variable NAMES an operator lists in `[adapters.<name>]
//!    `env_pass`; each is copied from the parent when it is set there. This is the only way a
//!    proxy, `AWS_*`, `GITHUB_TOKEN` or `GH_TOKEN` reaches a child.
//! 4. **Explicit extras** (`extra`), applied last, for the key the warrant layer chooses for one
//!    attempt. An extra replaces a same-named inherited variable.
//!
//! # What never passes
//!
//! Anything not listed, which covers `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` (a
//! proxy sees the vendor's traffic and its credentials; opt in with `env_pass`), `SSH_AUTH_SOCK`
//! (the child gets no git-over-ssh; agents commit through fl), `DISPLAY` and `WAYLAND_DISPLAY`
//! (no browser login flow from an unattended attempt), `AWS_*` (Claude on Bedrock needs them
//! through an extra) and every other tool's token. A variable whose name starts with one of
//! [`DENIED_PREFIXES`] (`FL_`, `FERROGATE_`, `GITHUB_`, `GH_`) is dropped even if an allow-list
//! entry would match it. A name that is not UTF-8 is dropped.

use fl_core::telemetry::AdapterKind;
use std::ffi::{OsStr, OsString};

/// Variable names every dialect's child receives, when the parent has them.
pub const BASELINE_EXACT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "TZ",
    "LANG",
    "LANGUAGE",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "AGENT_PERSONA",
    "AGENT_NAME",
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "RUSTUP_HOME",
    "NVM_DIR",
    "PNPM_HOME",
    "BUN_INSTALL",
    "GOPATH",
    "GOROOT",
    "JAVA_HOME",
    "PYENV_ROOT",
    "VIRTUAL_ENV",
    "PYTHONPATH",
    "PYTHONHOME",
    "NODE_PATH",
    "NODE_OPTIONS",
    "NPM_CONFIG_PREFIX",
    "NPM_CONFIG_CACHE",
    "LD_LIBRARY_PATH",
    "NIX_PATH",
    "NIX_PROFILES",
    "NIX_SSL_CERT_FILE",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
];

/// Name prefixes every dialect's child receives.
pub const BASELINE_PREFIXES: &[&str] = &["LC_"];

/// Name prefixes that never reach a child by inheritance, whatever an allow-list says: fl's own
/// variables (the vault master key, hub and wire tokens), the ferrogate settings and the GitHub
/// credentials. Matching is case-sensitive; default-deny (a name must be listed to pass) is the
/// real protection, and this list is the second line.
pub const DENIED_PREFIXES: &[&str] = &["FL_", "FERROGATE_", "GITHUB_", "GH_"];

/// Name prefixes that reach a child by no route: not inheritance, not `env_pass`, not an extra.
/// Config load refuses them; the filter drops them again.
pub const NEVER_PREFIXES: &[&str] = &["FL_", "FERROGATE_"];

/// The most names one `env_pass` list may hold.
pub const MAX_ENV_PASS: usize = 64;
/// The longest variable name `env_pass` accepts.
pub const MAX_ENV_NAME_LEN: usize = 128;

/// True when `name` can reach a child by no route (see [`NEVER_PREFIXES`]).
pub fn never_passes(name: &str) -> bool {
    NEVER_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Why `name` cannot be in an `env_pass` list, or `Ok`. A name is `[A-Za-z_][A-Za-z0-9_]*`, at
/// most [`MAX_ENV_NAME_LEN`] bytes, and does not start with a [`NEVER_PREFIXES`] prefix.
pub fn check_pass_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let shape_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if name.len() > MAX_ENV_NAME_LEN {
        Err(format!(
            "`{}...` is longer than {MAX_ENV_NAME_LEN} characters",
            name.chars().take(16).collect::<String>()
        ))
    } else if !shape_ok {
        Err(format!(
            "`{name}` is not a variable name (letters, digits and `_`, not starting with a digit)"
        ))
    } else if never_passes(name) {
        Err(format!(
            "`{name}` can never reach a vendor CLI: names starting with {} are fl's own secrets",
            NEVER_PREFIXES.join(" or ")
        ))
    } else {
        Ok(())
    }
}

/// Checks a whole `env_pass` list: length, then every name. Duplicates are fine.
pub fn check_pass_list(names: &[String]) -> Result<(), String> {
    if names.len() > MAX_ENV_PASS {
        return Err(format!(
            "lists {} names; at most {MAX_ENV_PASS} are allowed",
            names.len()
        ));
    }
    names.iter().try_for_each(|n| check_pass_name(n))
}

/// The credential and endpoint prefixes of one vendor CLI, and no other vendor's.
pub const fn dialect_prefixes(kind: AdapterKind) -> &'static [&'static str] {
    match kind {
        AdapterKind::Claude => &["ANTHROPIC_", "CLAUDE_"],
        AdapterKind::Codex => &["OPENAI_", "CODEX_"],
        AdapterKind::Antigravity => &["GEMINI_", "GOOGLE_", "ANTIGRAVITY_", "AGY_"],
        AdapterKind::Grok => &["XAI_", "GROK_"],
        AdapterKind::Muse => &["MUSE_"],
    }
}

/// The environment for a `kind` child, from this process's environment. `pass` names extra
/// variables to copy from it and `extra` is applied last (see the module doc).
pub fn child_env(
    kind: AdapterKind,
    pass: &[String],
    extra: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    child_env_from(kind, std::env::vars_os(), pass, extra)
}

/// [`child_env`] with the parent environment given instead of read.
pub fn child_env_from(
    kind: AdapterKind,
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    pass: &[String],
    extra: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    env_from(Some(kind), parent, pass, extra)
}

/// The environment for a helper that is not a vendor CLI (a hydrate script, `curl`): the
/// baseline of [`child_env`] and no vendor's credentials at all.
pub fn helper_env(pass: &[String], extra: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    helper_env_from(std::env::vars_os(), pass, extra)
}

/// [`helper_env`] with the parent environment given instead of read.
pub fn helper_env_from(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    pass: &[String],
    extra: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    env_from(None, parent, pass, extra)
}

fn env_from(
    kind: Option<AdapterKind>,
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    pass: &[String],
    extra: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let prefixes: Vec<&str> = BASELINE_PREFIXES
        .iter()
        .chain(kind.map_or(&[][..], dialect_prefixes))
        .copied()
        .collect();
    let rules = Rules {
        allow_exact: BASELINE_EXACT,
        allow_prefixes: &prefixes,
        denied_prefixes: DENIED_PREFIXES,
        never_prefixes: NEVER_PREFIXES,
    };
    scrub(parent, &rules, pass, extra)
}

/// The lists [`scrub`] applies.
pub struct Rules<'a> {
    /// Names that pass by inheritance.
    pub allow_exact: &'a [&'a str],
    /// Name prefixes that pass by inheritance.
    pub allow_prefixes: &'a [&'a str],
    /// Prefixes that do not pass by inheritance even if listed above.
    pub denied_prefixes: &'a [&'a str],
    /// Prefixes that pass by no route, `pass` and `extra` included.
    pub never_prefixes: &'a [&'a str],
}

/// The filter itself, with every list given. A parent variable passes when its name starts with
/// no `never_prefixes` prefix and either is named in `pass`, or is in `allow_exact` or starts
/// with an `allow_prefixes` prefix and starts with no `denied_prefixes` prefix. `extra` replaces
/// same-named variables; only its `never_prefixes` names are removed.
pub fn scrub(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    rules: &Rules<'_>,
    pass: &[String],
    extra: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let never = |name: &str| rules.never_prefixes.iter().any(|p| name.starts_with(p));
    let mut out: Vec<(OsString, OsString)> = parent
        .into_iter()
        .filter(|(name, _)| {
            let Some(name) = name.to_str() else {
                return false;
            };
            let inherited = !rules.denied_prefixes.iter().any(|p| name.starts_with(p))
                && (rules.allow_exact.contains(&name)
                    || rules.allow_prefixes.iter().any(|p| name.starts_with(p)));
            !never(name)
                && (inherited || pass.iter().any(|n| n == name))
                && !extra.iter().any(|(e, _)| e == OsStr::new(name))
        })
        .collect();
    out.extend(
        extra
            .iter()
            .filter(|(name, _)| name.to_str().is_some_and(|n| !never(n)))
            .cloned(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const ALL: [AdapterKind; 5] = [
        AdapterKind::Claude,
        AdapterKind::Codex,
        AdapterKind::Antigravity,
        AdapterKind::Grok,
        AdapterKind::Muse,
    ];

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        list.iter().map(|(k, v)| (os(k), os(v))).collect()
    }

    fn run(
        kind: AdapterKind,
        parent: &[(&str, &str)],
        extra: &[(&str, &str)],
    ) -> BTreeMap<String, String> {
        run_with_pass(kind, parent, &[], extra)
    }

    fn run_with_pass(
        kind: AdapterKind,
        parent: &[(&str, &str)],
        pass: &[&str],
        extra: &[(&str, &str)],
    ) -> BTreeMap<String, String> {
        let pass: Vec<String> = pass.iter().map(|s| s.to_string()).collect();
        child_env_from(kind, pairs(parent), &pass, &pairs(extra))
            .into_iter()
            .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
            .collect()
    }

    /// One credential variable per dialect, as a parent might hold them all at once.
    const VENDOR_KEYS: &[(&str, &str)] = &[
        ("ANTHROPIC_API_KEY", "anthropic-sentinel"),
        ("CLAUDE_CONFIG_DIR", "/c"),
        ("OPENAI_API_KEY", "openai-sentinel"),
        ("CODEX_HOME", "/x"),
        ("GEMINI_API_KEY", "gemini-sentinel"),
        ("GOOGLE_API_KEY", "google-sentinel"),
        ("XAI_API_KEY", "xai-sentinel"),
        ("GROK_API_KEY", "grok-sentinel"),
        ("MUSE_API_KEY", "muse-sentinel"),
    ];

    const DENIED: &[(&str, &str)] = &[
        ("FL_VAULT_MASTER_KEY", "s1"),
        ("FL_VAULT_MASTER_KEY_FILE", "s2"),
        ("FL_VAULT_DIR", "s3"),
        ("FL_HUB_TOKEN", "s4"),
        ("FL_WIRE_TOKEN", "s5"),
        ("FL_GITHUB_APP_KEY", "s6"),
        ("FERROGATE_TOKEN", "s7"),
        ("FERROGATE_", "s8"),
        ("GITHUB_TOKEN", "s9"),
        ("GH_TOKEN", "s10"),
        ("HTTP_PROXY", "p1"),
        ("HTTPS_PROXY", "p2"),
        ("https_proxy", "p3"),
        ("ALL_PROXY", "p4"),
        ("NO_PROXY", "p5"),
        ("SSH_AUTH_SOCK", "/ssh"),
        ("DISPLAY", ":0"),
        ("WAYLAND_DISPLAY", "w"),
        ("AWS_SECRET_ACCESS_KEY", "aws"),
        ("NPM_TOKEN", "npm"),
        ("CARGO_REGISTRY_TOKEN", "cargo"),
        ("fl_vault_master_key", "lower"),
        ("XFL_VAULT_MASTER_KEY", "prefixed"),
        ("SOME_RANDOM_VAR", "x"),
    ];

    const BASELINE: &[(&str, &str)] = &[
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/home/u"),
        ("USER", "u"),
        ("LOGNAME", "u"),
        ("LANG", "C.UTF-8"),
        ("LC_ALL", "C"),
        ("LC_CTYPE", "C"),
        ("TERM", "xterm"),
        ("TZ", "UTC"),
        ("TMPDIR", "/t"),
        ("SHELL", "/bin/sh"),
        ("XDG_RUNTIME_DIR", "/r"),
        ("XDG_CONFIG_HOME", "/c"),
        ("XDG_DATA_HOME", "/d"),
        ("XDG_CACHE_HOME", "/k"),
        ("XDG_STATE_HOME", "/s"),
        ("NO_COLOR", "1"),
        ("COLORTERM", "truecolor"),
        ("AGENT_PERSONA", "felicity"),
        ("AGENT_NAME", "felicity"),
        ("CARGO_HOME", "/cargo"),
        ("RUSTUP_HOME", "/rustup"),
        ("NVM_DIR", "/nvm"),
        ("NODE_OPTIONS", "--max-old-space-size=4096"),
        ("SSL_CERT_FILE", "/cert"),
        ("LD_LIBRARY_PATH", "/lib"),
    ];

    fn parent_all() -> Vec<(&'static str, &'static str)> {
        let mut v: Vec<(&str, &str)> = Vec::new();
        v.extend_from_slice(BASELINE);
        v.extend_from_slice(VENDOR_KEYS);
        v.extend_from_slice(DENIED);
        v
    }

    #[test]
    fn every_dialect_gets_the_baseline_and_only_its_own_vendor_keys() {
        for kind in ALL {
            let env = run(kind, &parent_all(), &[]);
            for (k, v) in BASELINE {
                assert_eq!(env.get(*k).map(String::as_str), Some(*v), "{kind:?} {k}");
            }
            let own = dialect_prefixes(kind);
            for (k, _) in VENDOR_KEYS {
                let is_own = own.iter().any(|p| k.starts_with(p));
                assert_eq!(env.contains_key(*k), is_own, "{kind:?} {k}");
            }
            // Each dialect has at least one credential in the table above.
            assert!(
                VENDOR_KEYS.iter().any(|(k, _)| env.contains_key(*k)),
                "{kind:?} lost its own credentials"
            );
        }
    }

    #[test]
    fn the_named_dialects_see_their_own_key_and_not_the_others() {
        let cases = [
            (AdapterKind::Claude, "ANTHROPIC_API_KEY"),
            (AdapterKind::Codex, "OPENAI_API_KEY"),
            (AdapterKind::Antigravity, "GEMINI_API_KEY"),
            (AdapterKind::Grok, "XAI_API_KEY"),
            (AdapterKind::Muse, "MUSE_API_KEY"),
        ];
        for (kind, own) in cases {
            let env = run(kind, &parent_all(), &[]);
            assert!(env.contains_key(own), "{kind:?} should keep {own}");
            for (_, other) in cases.iter().filter(|(k, _)| *k != kind) {
                assert!(!env.contains_key(*other), "{kind:?} must not see {other}");
            }
        }
    }

    #[test]
    fn no_denied_class_reaches_any_dialect() {
        for kind in ALL {
            let env = run(kind, &parent_all(), &[]);
            for (k, _) in DENIED {
                assert!(!env.contains_key(*k), "{kind:?} leaked {k}");
            }
        }
    }

    #[test]
    fn fl_ferrogate_and_github_prefixes_are_denied_even_for_a_dialect_that_would_allow_them() {
        // A hostile allow-list that lists the denied names and prefixes outright: the denial
        // still wins.
        let parent = pairs(&[
            ("FL_VAULT_MASTER_KEY", "a"),
            ("FL_ANYTHING", "b"),
            ("FERROGATE_X", "c"),
            ("GITHUB_TOKEN", "d"),
            ("GH_TOKEN", "e"),
            ("KEEP", "k"),
        ]);
        let rules = Rules {
            allow_exact: &["FL_VAULT_MASTER_KEY", "GITHUB_TOKEN", "KEEP"],
            allow_prefixes: &["FL_", "FERROGATE_", "GH_"],
            denied_prefixes: DENIED_PREFIXES,
            never_prefixes: NEVER_PREFIXES,
        };
        let out = scrub(parent, &rules, &[], &[]);
        assert_eq!(out, pairs(&[("KEEP", "k")]));
    }

    #[test]
    fn the_warrant_spliced_key_is_present_and_replaces_an_inherited_one() {
        let env = run(
            AdapterKind::Claude,
            &[
                ("PATH", "/bin"),
                ("ANTHROPIC_API_KEY", "inherited"),
                ("OPENAI_API_KEY", "other"),
            ],
            &[("ANTHROPIC_API_KEY", "spliced")],
        );
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("spliced")
        );
        assert!(!env.contains_key("OPENAI_API_KEY"));
        // Exactly one entry, so which one wins does not depend on the spawn order.
        let list = child_env_from(
            AdapterKind::Claude,
            pairs(&[("ANTHROPIC_API_KEY", "inherited")]),
            &[],
            &pairs(&[("ANTHROPIC_API_KEY", "spliced")]),
        );
        assert_eq!(list, pairs(&[("ANTHROPIC_API_KEY", "spliced")]));
    }

    #[test]
    fn an_explicit_extra_opts_a_proxy_in() {
        let env = run(
            AdapterKind::Codex,
            &[("HTTPS_PROXY", "http://p")],
            &[("HTTPS_PROXY", "http://opted-in")],
        );
        assert_eq!(
            env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://opted-in")
        );
        assert!(
            !run(AdapterKind::Codex, &[("HTTPS_PROXY", "http://p")], &[])
                .contains_key("HTTPS_PROXY")
        );
    }

    #[test]
    fn a_name_that_is_not_utf8_is_dropped() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![b'P', b'A', b'T', b'H', 0xff]);
        let out = child_env_from(
            AdapterKind::Muse,
            vec![(bad, os("x")), (os("PATH"), os("/bin"))],
            &[],
            &[],
        );
        assert_eq!(out, pairs(&[("PATH", "/bin")]));
    }

    #[test]
    fn an_empty_parent_gives_only_the_extras() {
        let out = child_env_from(
            AdapterKind::Grok,
            Vec::new(),
            &[],
            &pairs(&[("XAI_API_KEY", "k")]),
        );
        assert_eq!(out, pairs(&[("XAI_API_KEY", "k")]));
    }

    #[test]
    fn never_prefixes_are_refused_from_extra_and_pass_in_every_dialect() {
        for kind in ALL {
            let env = run_with_pass(
                kind,
                &[
                    ("FL_VAULT_MASTER_KEY", "a"),
                    ("FERROGATE_TOKEN", "b"),
                    ("FL_HUB_TOKEN", "c"),
                    ("PATH", "/bin"),
                ],
                &["FL_VAULT_MASTER_KEY", "FERROGATE_TOKEN"],
                &[
                    ("FL_HUB_TOKEN", "x"),
                    ("FERROGATE_Y", "y"),
                    ("OK_EXTRA", "z"),
                ],
            );
            assert_eq!(
                env.keys().map(String::as_str).collect::<Vec<_>>(),
                ["OK_EXTRA", "PATH"],
                "{kind:?}"
            );
        }
    }

    #[test]
    fn github_names_pass_only_when_named_or_given_never_by_inheritance() {
        let parent = [("GITHUB_TOKEN", "g"), ("GH_TOKEN", "h"), ("GH_HOST", "x")];
        for kind in ALL {
            assert!(run(kind, &parent, &[]).is_empty(), "{kind:?}");
            let env = run_with_pass(kind, &parent, &["GH_TOKEN"], &[("GITHUB_TOKEN", "e")]);
            assert_eq!(env.get("GH_TOKEN").map(String::as_str), Some("h"));
            assert_eq!(env.get("GITHUB_TOKEN").map(String::as_str), Some("e"));
            assert!(!env.contains_key("GH_HOST"));
        }
    }

    #[test]
    fn pass_copies_only_the_named_variables_that_the_parent_has() {
        let env = run_with_pass(
            AdapterKind::Claude,
            &[
                ("HTTPS_PROXY", "http://p"),
                ("NO_PROXY", "localhost"),
                ("AWS_PROFILE", "dev"),
                ("AWS_SECRET_ACCESS_KEY", "s"),
            ],
            &["HTTPS_PROXY", "AWS_PROFILE", "NOT_SET_ANYWHERE"],
            &[],
        );
        assert_eq!(
            env.keys().map(String::as_str).collect::<Vec<_>>(),
            ["AWS_PROFILE", "HTTPS_PROXY"]
        );
    }

    #[test]
    fn pass_name_validation_table() {
        let long = "A".repeat(MAX_ENV_NAME_LEN + 1);
        let ok = "A".repeat(MAX_ENV_NAME_LEN);
        for good in [
            "HTTPS_PROXY",
            "_x",
            "aws_profile",
            "A1",
            "GITHUB_TOKEN",
            ok.as_str(),
        ] {
            assert!(check_pass_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "1A",
            "A-B",
            "A B",
            "A=B",
            "A\0B",
            "é",
            long.as_str(),
            "FL_VAULT_MASTER_KEY",
            "FL_",
            "FERROGATE_TOKEN",
        ] {
            assert!(check_pass_name(bad).is_err(), "{bad:?}");
        }
        // Case matters: Unix names are case-sensitive, so `fl_x` is an ordinary variable.
        assert!(check_pass_name("fl_x").is_ok());
        let many: Vec<String> = (0..=MAX_ENV_PASS).map(|i| format!("V{i}")).collect();
        assert!(check_pass_list(&many).is_err());
        assert!(check_pass_list(&many[..MAX_ENV_PASS]).is_ok());
        let dup = vec!["A".to_string(), "A".to_string()];
        assert!(check_pass_list(&dup).is_ok());
        assert!(check_pass_list(&["A".into(), "FL_B".into()]).is_err());
    }

    #[test]
    fn no_allow_list_entry_is_in_a_denied_class() {
        for name in BASELINE_EXACT {
            assert!(
                !DENIED_PREFIXES.iter().any(|p| name.starts_with(p)),
                "{name} is both allowed and denied"
            );
        }
        for kind in ALL {
            for p in dialect_prefixes(kind) {
                assert!(!DENIED_PREFIXES.iter().any(|d| p.starts_with(d)), "{p}");
            }
        }
    }

    fn helper_map(pass: &[String], extra: &[(OsString, OsString)]) -> BTreeMap<String, String> {
        helper_env_from(pairs(&parent_all()), pass, extra)
            .into_iter()
            .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
            .collect()
    }

    #[test]
    fn a_helper_gets_the_baseline_and_no_vendor_key() {
        let env = helper_map(&[], &[]);
        for (k, v) in BASELINE {
            assert_eq!(env.get(*k).map(String::as_str), Some(*v), "{k}");
        }
        for (k, _) in VENDOR_KEYS.iter().chain(DENIED) {
            assert!(!env.contains_key(*k), "{k} reached a helper");
        }
    }

    #[test]
    fn a_helper_takes_pass_names_and_extras_but_never_an_fl_name() {
        let pass = vec!["HTTPS_PROXY".to_string(), "FL_VAULT_MASTER_KEY".to_string()];
        let env = helper_map(&pass, &pairs(&[("UV_CACHE_DIR", "/uv")]));
        assert_eq!(env.get("HTTPS_PROXY").map(String::as_str), Some("p2"));
        assert_eq!(env.get("UV_CACHE_DIR").map(String::as_str), Some("/uv"));
        assert!(!env.contains_key("FL_VAULT_MASTER_KEY"));
    }
}
