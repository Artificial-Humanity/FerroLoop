//! Project display paths and their projection from the workspace registry.

use std::collections::BTreeMap;

use fl_core::agent::AreaRecord;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::project_init::{
    InitError, checked_display_name, checked_slug, invalid, slug_from_display,
};

pub(crate) fn display_path(raw: &str) -> Result<String, InitError> {
    let name = checked_display_name(raw)?;
    let segments: Vec<_> = name.split('/').map(str::trim).collect();
    if segments.len() > 4 || segments.iter().any(|s| s.is_empty()) {
        return Err(invalid(
            "name: use one to four nonempty parts separated by '/'",
        ));
    }
    Ok(segments.join("/").nfc().collect())
}

pub(crate) fn matches_display_path(raw: &str, name: &str) -> bool {
    raw == name || display_path(raw).is_ok_and(|path| path == name)
}

/// Use the existing flat-name slug form for ASCII names. For hierarchical names, each
/// segment must produce its own kebab-case token. Ignore combining marks on Latin letters.
pub(crate) fn slug_from_project_name(name: &str) -> Result<String, InitError> {
    if name.is_ascii() && !name.contains('/') {
        return slug_from_display(name);
    }
    let cannot_derive = || {
        invalid(format!(
            "cannot derive a project slug from `{name}`. Pass --slug or set `slug` explicitly"
        ))
    };
    let nested = name.contains('/');
    let mut tokens = Vec::new();
    for segment in name.split('/') {
        let mut token = String::new();
        let mut latin = false;
        for c in segment.nfd() {
            if is_combining_mark(c) && latin {
                continue;
            }
            latin = c.is_ascii_alphabetic();
            if !c.is_ascii() && c.is_alphabetic() {
                return Err(cannot_derive());
            }
            if c.is_ascii_alphanumeric() || (!nested && c == '_') {
                token.push(c.to_ascii_lowercase());
            } else if !token.ends_with('-') {
                token.push('-');
            }
        }
        let token = token.trim_matches('-');
        if token.is_empty() {
            return Err(cannot_derive());
        }
        tokens.push(token.to_string());
    }
    checked_slug(&tokens.join("-")).map_err(|_| cannot_derive())
}

/// Return the verified root-to-parent chain. A malformed or incomplete linked area projects
/// as a root. Each hop must remove one name segment, so cycles cannot consume unbounded work.
pub(crate) fn ancestors(
    areas: &BTreeMap<String, AreaRecord>,
    area: &AreaRecord,
) -> Option<Vec<String>> {
    if area.parent.is_none() {
        return Some(Vec::new());
    }
    let mut links = Vec::new();
    let mut expected = display_path(&area.name).ok()?;
    let mut current = area;
    while let Some(parent) = &current.parent {
        let (prefix, _) = expected.rsplit_once('/')?;
        current = areas.get(parent)?;
        if !matches_display_path(&current.name, prefix) {
            return None;
        }
        expected.truncate(prefix.len());
        links.push(parent.clone());
    }
    if expected.contains('/') {
        return None;
    }
    links.reverse();
    Some(links)
}

pub(crate) fn roster_areas(areas: Vec<AreaRecord>) -> Vec<serde_json::Value> {
    let areas: BTreeMap<_, _> = areas
        .into_iter()
        .map(|area| (area.area_key.clone(), area))
        .collect();
    areas
        .values()
        .map(|area| {
            let links = ancestors(&areas, area).unwrap_or_default();
            let mut row = serde_json::to_value(area).expect("area records serialize as objects");
            row["parent"] = serde_json::json!(links.last());
            row["ancestors"] = serde_json::json!(links);
            row
        })
        .collect()
}
