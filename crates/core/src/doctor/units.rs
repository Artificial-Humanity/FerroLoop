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
/// Comments and the wrong section do not count. Repeated keys are merged, which is how
/// systemd reads them. A trailing `\` continues the line.
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
            after |= mentions_default_target(value);
        } else if section == "Install"
            && let Some(value) = assignment(trimmed, "WantedBy")
        {
            wanted |= mentions_default_target(value);
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
    let rest = trimmed.strip_prefix(key)?;
    let rest = rest.strip_prefix('=')?;
    Some(rest.trim())
}

fn mentions_default_target(value: &str) -> bool {
    value.split_whitespace().any(|token| {
        let token = token.strip_prefix('-').unwrap_or(token);
        token == DEFAULT_TARGET
    })
}

fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for raw in text.lines() {
        let continued = raw.ends_with('\\') && !raw.ends_with("\\\\");
        let piece = if continued {
            &raw[..raw.len() - 1]
        } else {
            raw
        };
        if buf.is_empty() {
            buf.push_str(piece);
        } else {
            buf.push(' ');
            buf.push_str(piece.trim_start());
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
}
