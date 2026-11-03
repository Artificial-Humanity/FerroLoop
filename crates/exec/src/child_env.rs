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
//! 3. **Explicit extras**: the `extra` argument, applied last, for the key the warrant layer
//!    chooses for one attempt and for any setting an operator opts in to (a proxy, for
//!    example). An extra replaces a same-named inherited variable.
//!
//! # What never passes
//!
//! Anything not listed, which covers `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` (a
//! proxy sees the vendor's traffic and its credentials; opt in through an extra), `SSH_AUTH_SOCK`
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

/// Name prefixes that never reach a child, whatever an allow-list says. These are fl's own
/// variables (the vault master key, hub and wire tokens), the ferrogate settings and the GitHub
/// credentials.
pub const DENIED_PREFIXES: &[&str] = &["FL_", "FERROGATE_", "GITHUB_", "GH_"];

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

/// The environment for a `kind` child, from this process's environment. `extra` is applied
/// last (see the module doc).
pub fn child_env(kind: AdapterKind, extra: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    child_env_from(kind, std::env::vars_os(), extra)
}

/// [`child_env`] with the parent environment given instead of read.
pub fn child_env_from(
    kind: AdapterKind,
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    extra: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let prefixes: Vec<&str> = BASELINE_PREFIXES
        .iter()
        .chain(dialect_prefixes(kind))
        .copied()
        .collect();
    scrub(parent, BASELINE_EXACT, &prefixes, DENIED_PREFIXES, extra)
}

/// The filter itself, with every list given. A name passes when it is in `allow_exact` or starts
/// with one of `allow_prefixes`, and starts with none of `denied_prefixes`. `extra` replaces
/// same-named variables and is not filtered.
pub fn scrub(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    allow_exact: &[&str],
    allow_prefixes: &[&str],
    denied_prefixes: &[&str],
    extra: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let mut out: Vec<(OsString, OsString)> = parent
        .into_iter()
        .filter(|(name, _)| {
            let Some(name) = name.to_str() else {
                return false;
            };
            !denied_prefixes.iter().any(|p| name.starts_with(p))
                && (allow_exact.contains(&name)
                    || allow_prefixes.iter().any(|p| name.starts_with(p)))
                && !extra.iter().any(|(e, _)| e == OsStr::new(name))
        })
        .collect();
    out.extend(extra.iter().cloned());
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
        child_env_from(kind, pairs(parent), &pairs(extra))
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
        let out = scrub(
            parent,
            &["FL_VAULT_MASTER_KEY", "GITHUB_TOKEN", "KEEP"],
            &["FL_", "FERROGATE_", "GH_"],
            DENIED_PREFIXES,
            &[],
        );
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
        );
        assert_eq!(out, pairs(&[("PATH", "/bin")]));
    }

    #[test]
    fn an_empty_parent_gives_only_the_extras() {
        let out = child_env_from(
            AdapterKind::Grok,
            Vec::new(),
            &pairs(&[("XAI_API_KEY", "k")]),
        );
        assert_eq!(out, pairs(&[("XAI_API_KEY", "k")]));
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
}
