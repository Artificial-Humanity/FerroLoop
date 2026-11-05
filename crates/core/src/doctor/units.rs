//! `G7.unit-order`: a user unit must not be wanted by `default.target` and also ordered
//! after it.
//!
//! `WantedBy=default.target` pulls the unit in before `default.target`. `After=default.target`
//! pushes it after. Together they are a `systemd --user` ordering cycle, and systemd drops
//! the start jobs at boot. The engine only judges facts. [`user_unit_directives`] reads the
//! unit text; it does no I/O.

use super::{CheckStatus, DoctorCheck, DoctorInputs, ids, joined, make};

const TITLE: &str = "User unit ordering";
const DEFAULT_TARGET: &str = "default.target";

/// Whether a unit file orders itself against `default.target`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitDirectives {
    /// `[Unit]` `After=` names `default.target` (a leading `-` still orders).
    pub after_default_target: bool,
    /// `[Install]` `WantedBy=` names `default.target`.
    pub wanted_by_default_target: bool,
}

/// The `After=` / `WantedBy=` facts of one unit file.
///
/// Comments and the wrong section do not count. Repeated `After=` lines are merged; an empty
/// `After=` does not clear an earlier one. An empty `WantedBy=` clears `WantedBy=` values
/// already seen, and a later non-empty `WantedBy=` applies again. One matching quote pair
/// around a `WantedBy` token is removed; quotes on `After=` stay literal. A trailing `\`
/// continues the line. A `#` or `;` line is skipped, including one that ends in `\`, and a
/// comment inside a continuation does not end it. Whitespace around `=` is ignored.
pub fn user_unit_directives(text: &str) -> UnitDirectives {
    let mut section = String::new();
    let mut after = false;
    let mut wanted = false;
    for line in logical_lines(text) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some(name) = section_name(trimmed) {
            section = name;
            continue;
        }
        if section == "Unit"
            && let Some(value) = assignment(trimmed, "After")
        {
            // Empty `After=` does not drop dependencies already read.
            after |= mentions_default_target(value);
        } else if section == "Install"
            && let Some(value) = assignment(trimmed, "WantedBy")
        {
            if value.is_empty() {
                wanted = false;
            } else {
                wanted |= mentions_wanted_by_default(value);
            }
        }
    }
    UnitDirectives {
        after_default_target: after,
        wanted_by_default_target: wanted,
    }
}

fn section_name(trimmed: &str) -> Option<String> {
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    if inner.is_empty() || inner.contains(['[', ']']) {
        return None;
    }
    Some(inner.to_string())
}

fn assignment<'a>(trimmed: &'a str, key: &str) -> Option<&'a str> {
    let rest = trimmed.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?;
    Some(rest.trim())
}

fn mentions_default_target(value: &str) -> bool {
    value.split_whitespace().any(mentions_default_target_token)
}

/// One leading `-` still orders (`-default.target` is the unit `default.target`).
fn mentions_default_target_token(token: &str) -> bool {
    let token = token.strip_prefix('-').unwrap_or(token);
    token == DEFAULT_TARGET
}

/// `WantedBy=` is `config_parse_strv` (`EXTRACT_UNQUOTE`): one matching quote pair is removed.
/// `After=` keeps quotes, so this is not used for it.
fn mentions_wanted_by_default(value: &str) -> bool {
    value.split_whitespace().any(wanted_by_token_is_default)
}

fn wanted_by_token_is_default(token: &str) -> bool {
    if let Some(inner) = matching_quotes(token) {
        return mentions_default_target_token(inner);
    }
    if let Some(rest) = token.strip_prefix('-')
        && let Some(inner) = matching_quotes(rest)
    {
        // `-"default.target"` unquotes to `-default.target`; the dash strip then
        // compares the quoted remainder itself.
        return inner == DEFAULT_TARGET;
    }
    mentions_default_target_token(token)
}

/// Matching outer `"` or `'`. Mismatched or unbalanced quotes stay literal.
fn matching_quotes(token: &str) -> Option<&str> {
    let bytes = token.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let quote = bytes[0];
    if (quote == b'"' || quote == b'\'') && bytes[bytes.len() - 1] == quote {
        Some(&token[1..bytes.len() - 1])
    } else {
        None
    }
}

fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut continued = false;
    for raw in text.lines() {
        // Skip every comment line. `continue` leaves `continued` alone, so a `#`/`\` line
        // neither joins into the value nor swallows the next directive.
        let trimmed = raw.trim();
        if trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        let open = continued;
        continued = raw.ends_with('\\') && !raw.ends_with("\\\\");
        let piece = if continued {
            &raw[..raw.len() - 1]
        } else {
            raw
        };
        if open {
            buf.push(' ');
            buf.push_str(piece.trim_start());
        } else {
            buf.push_str(piece);
        }
        if !continued {
            out.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

pub(super) fn check(inputs: &DoctorInputs<'_>) -> DoctorCheck {
    let facts = &inputs.units;
    let mut units = facts.units.clone();
    units.sort_by(|a, b| a.name.cmp(&b.name));
    let cycles: Vec<String> = units
        .iter()
        .filter(|u| u.readable && u.after_default_target && u.wanted_by_default_target)
        .map(|u| u.name.clone())
        .collect();
    if !cycles.is_empty() {
        let verb = if cycles.len() == 1 {
            "declares"
        } else {
            "declare"
        };
        let mut detail = format!(
            "{} {verb} both WantedBy=default.target and After=default.target, which creates \
             a systemd --user boot ordering cycle (WantedBy=default.target implies \
             Before=default.target). Remediate with `fl install`, or remove `After=default.target`",
            joined(&cycles, 8)
        );
        if let Some(note) = &facts.note {
            detail.push_str("; ");
            detail.push_str(note);
        }
        return make(ids::UNIT_ORDER, TITLE, CheckStatus::Warn, detail);
    }
    if let Some(note) = &facts.note {
        return make(
            ids::UNIT_ORDER,
            TITLE,
            CheckStatus::Unsupported,
            note.clone(),
        );
    }
    let unread: Vec<String> = units
        .iter()
        .filter(|u| !u.readable)
        .map(|u| u.name.clone())
        .collect();
    if !unread.is_empty() {
        return make(
            ids::UNIT_ORDER,
            TITLE,
            CheckStatus::Warn,
            format!(
                "could not read {} (a unit that cannot be read is not treated as clean)",
                joined(&unread, 8)
            ),
        );
    }
    if units.is_empty() {
        return make(
            ids::UNIT_ORDER,
            TITLE,
            CheckStatus::NotInstalled,
            "no FerroLoop user units (ferro*.service) are installed".into(),
        );
    }
    let word = if units.len() == 1 { "unit" } else { "units" };
    make(
        ids::UNIT_ORDER,
        TITLE,
        CheckStatus::Pass,
        format!(
            "{} FerroLoop user {word}; none declare both WantedBy=default.target and \
             After=default.target",
            units.len()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATES: [&str; 4] = [
        include_str!("../../../../contrib/units/ferrostore.service"),
        include_str!("../../../../contrib/units/ferrostore@.service"),
        include_str!("../../../../contrib/units/ferrowire.service"),
        include_str!("../../../../contrib/units/ferroloop-hub.service"),
    ];

    #[test]
    fn shipped_templates_are_wanted_by_default_and_not_ordered_after_it() {
        for text in TEMPLATES {
            let d = user_unit_directives(text);
            assert!(d.wanted_by_default_target, "{text}");
            assert!(!d.after_default_target, "{text}");
        }
    }

    #[test]
    fn after_and_wanted_by_default_target_are_both_seen() {
        let d = user_unit_directives(
            "[Unit]\nAfter=default.target\n\n[Install]\nWantedBy=default.target\n",
        );
        assert!(d.after_default_target && d.wanted_by_default_target);
    }

    #[test]
    fn a_token_among_others_and_an_optional_dependency_still_order() {
        let d = user_unit_directives(
            "[Unit]\nAfter=ferrostore.service -default.target\n\n[Install]\nWantedBy=graphical.target default.target\n",
        );
        assert!(d.after_default_target && d.wanted_by_default_target);
    }

    #[test]
    fn a_continued_after_line_is_one_assignment() {
        let d = user_unit_directives(
            "[Unit]\nAfter=ferrostore.service \\\n    default.target\n\n[Install]\nWantedBy=default.target\n",
        );
        assert!(d.after_default_target && d.wanted_by_default_target);
    }

    #[test]
    fn whitespace_around_the_equals_is_still_an_assignment() {
        let d = user_unit_directives(
            "[Unit]\nAfter = default.target\n\n[Install]\nWantedBy = default.target\n",
        );
        assert!(d.after_default_target && d.wanted_by_default_target);
    }

    #[test]
    fn a_comment_inside_a_continuation_is_not_a_token() {
        let commented = user_unit_directives(
            "[Unit]\nAfter=ferrostore.service \\\n# default.target\n; default.target\n    network.target\n\n[Install]\nWantedBy=default.target\n",
        );
        assert!(
            !commented.after_default_target,
            "a comment in the continuation gap is not part of the value"
        );
        assert!(commented.wanted_by_default_target);

        let joined = user_unit_directives(
            "[Unit]\nAfter=ferrostore.service \\\n# skipped\n; skipped too\n    default.target\n\n[Install]\nWantedBy=default.target\n",
        );
        assert!(
            joined.after_default_target && joined.wanted_by_default_target,
            "the content line after the skipped comment block is still joined"
        );
    }

    #[test]
    fn comments_wrong_sections_and_near_names_do_not_count() {
        let d = user_unit_directives(
            "\
# After=default.target
; WantedBy=default.target
[Unit]
Description=After=default.target is not a directive
After=not-default.target
After=default.target.wants/foo.service

[Service]
After=default.target

[Install]
WantedBy=multi-user.target
",
        );
        assert!(!d.after_default_target);
        assert!(!d.wanted_by_default_target);
    }

    #[test]
    fn wanted_by_without_after_is_not_the_cycle() {
        let d = user_unit_directives("[Install]\nWantedBy=default.target\n");
        assert!(!d.after_default_target && d.wanted_by_default_target);
        let d = user_unit_directives("[Unit]\nAfter=default.target\n");
        assert!(d.after_default_target && !d.wanted_by_default_target);
    }

    #[test]
    fn empty_wanted_by_resets_and_empty_after_does_not() {
        let cleared = user_unit_directives("[Install]\nWantedBy=default.target\nWantedBy=\n");
        assert!(!cleared.wanted_by_default_target);
        let blanks = user_unit_directives("[Install]\nWantedBy=default.target\nWantedBy=   \n");
        assert!(!blanks.wanted_by_default_target);
        let restored = user_unit_directives(
            "[Install]\nWantedBy=default.target\nWantedBy=\nWantedBy=default.target\n",
        );
        assert!(restored.wanted_by_default_target);

        let kept = user_unit_directives("[Unit]\nAfter=default.target\nAfter=\n");
        assert!(kept.after_default_target);
        let later = user_unit_directives("[Unit]\nAfter=\nAfter=default.target\n");
        assert!(later.after_default_target);
    }

    #[test]
    fn wanted_by_quotes_match_and_the_same_after_quotes_do_not() {
        // Outer quotes, quotes around a leading `-`, and a `-` outside the quotes.
        let quoted = [
            "\"default.target\"",
            "'default.target'",
            "\"-default.target\"",
            "'-default.target'",
            "-\"default.target\"",
            "-'default.target'",
        ];
        for token in quoted {
            let wanted = user_unit_directives(&format!("[Install]\nWantedBy={token}\n"));
            assert!(
                wanted.wanted_by_default_target,
                "WantedBy={token} should match"
            );
            let after = user_unit_directives(&format!("[Unit]\nAfter={token}\n"));
            assert!(
                !after.after_default_target,
                "After={token} keeps the quotes and must not match"
            );
        }
        for token in [
            "\"default.target'",
            "'default.target\"",
            "\"default.target",
            "default.target\"",
            "-\"default.target",
            "-'default.target",
        ] {
            let wanted = user_unit_directives(&format!("[Install]\nWantedBy={token}\n"));
            assert!(
                !wanted.wanted_by_default_target,
                "unbalanced WantedBy={token} must stay literal"
            );
        }
    }

    #[test]
    fn a_comment_ending_in_backslash_does_not_swallow_the_next_directive() {
        for line in ["# comment \\", "; comment \\"] {
            let d = user_unit_directives(&format!("[Unit]\n{line}\nAfter=default.target\n"));
            assert!(
                d.after_default_target,
                "a comment ending in \\ must not swallow the next line: {line:?}"
            );
        }
    }
}
