//! One-shot workspace importer: legacy `.agents/memory/*.md` files become
//! `Memory` entries and `Notes/*/STATE.md` files become `Handoff` entries
//! (FerroState Phase 2 design §3, §8).
//!
//! The scan/apply split is shared by the CLI (`fl state import-workspace`)
//! and the Hub (`POST /api/v1/state/import-workspace`) so neither surface
//! duplicates the parsing: [`scan_workspace_import`] only reads the tree and
//! produces [`UpsertStateArgs`], and [`apply_workspace_import`] writes them
//! through any [`StateStore`]. A per-file or per-upsert failure lands in
//! `skipped` and never aborts the run.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::at::At;
use crate::state::{
    HandoffEntry, MAX_BODY_BYTES, MAX_NOTES_BYTES, MAX_SUMMARY_CHARS, MemoryBasis, MemoryCategory,
    MemoryEntry, MemoryLifecycle, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs, validate_slug,
};
use crate::store::StateStore;

/// Outcome of an import run (CLI JSON shape; the Hub omits `skipped`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceImportReport {
    pub imported_memories: usize,
    pub imported_handoffs: usize,
    /// Sorted, unique wire forms of the scopes that received an entry.
    pub scopes: Vec<String>,
    /// `"<path>: <reason>"` (scan) or `"<scope>/<slug>: <error>"` (apply).
    pub skipped: Vec<String>,
}

/// A read-only scan of a workspace tree, ready to apply to a store.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceImportScan {
    pub upserts: Vec<UpsertStateArgs>,
    pub skipped: Vec<String>,
}

impl WorkspaceImportScan {
    pub fn memory_count(&self) -> usize {
        self.upserts
            .iter()
            .filter(|u| matches!(u.payload, StatePayload::Memory(_)))
            .count()
    }

    pub fn handoff_count(&self) -> usize {
        self.upserts
            .iter()
            .filter(|u| matches!(u.payload, StatePayload::Handoff(_)))
            .count()
    }

    /// Sorted, unique wire forms of every scope in the scan.
    pub fn scopes(&self) -> Vec<String> {
        self.upserts
            .iter()
            .map(|u| u.scope.as_wire())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

/// First ancestor of `start` (including `start` itself) that contains a
/// `.agents/memory` or `Notes` directory; `start` when no ancestor does.
pub fn resolve_workspace_root(start: &Path) -> PathBuf {
    for ancestor in start.ancestors() {
        if ancestor.join(".agents").join("memory").is_dir() || ancestor.join("Notes").is_dir() {
            return ancestor.to_path_buf();
        }
    }
    start.to_path_buf()
}

/// Derive a valid slug from an arbitrary file stem or directory name:
/// lowercase, fold runs of invalid characters to `-`, trim leading/trailing
/// `-_`, prefix `m-` unless the result starts with `a-z`, and truncate to
/// 64 bytes with a final re-trim. `None` only when nothing usable remains.
pub fn derive_import_slug(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut last_was_dash = false;
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '_' {
            out.push(lower);
            last_was_dash = false;
        } else if lower == '-' {
            if !last_was_dash {
                out.push('-');
            }
            last_was_dash = true;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '_');
    if trimmed.is_empty() {
        return None;
    }
    let mut slug = if trimmed
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_lowercase)
    {
        trimmed.to_string()
    } else {
        format!("m-{trimmed}")
    };
    slug.truncate(64);
    let slug = slug.trim_matches(|c| c == '-' || c == '_').to_string();
    validate_slug(&slug).ok()
}

/// Collapse all whitespace to single spaces and truncate to
/// [`MAX_SUMMARY_CHARS`] characters. May return an empty string; callers
/// supply the fallback.
fn sanitize_summary(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(MAX_SUMMARY_CHARS).collect()
}

/// Truncate `text` to at most `max_bytes` bytes on a char boundary.
fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn read_utf8(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| format!("invalid UTF-8: {e}"))
}

/// Sorted paths of the entries directly inside `dir` (empty when absent).
fn sorted_dir_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    paths
}

/// Hand-parsed YAML-frontmatter fields the importer uses (no YAML crate).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    title: Option<String>,
    ty: Option<String>,
}

fn strip_surrounding_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Split optional YAML frontmatter from the body. Frontmatter exists only
/// when the first line is `---` and a later line closes it with `---` or
/// `...`; keys are captured at any indentation (real files nest `type`
/// under `metadata:`). Without a closing line the whole text is the body.
fn split_frontmatter(text: &str) -> (Frontmatter, &str) {
    let mut offset = 0usize;
    let mut lines: Vec<(&str, usize)> = Vec::new();
    for chunk in text.split_inclusive('\n') {
        lines.push((chunk.trim_end_matches(['\n', '\r']), offset));
        offset += chunk.len();
    }
    if lines.first().map(|(l, _)| l.trim()) != Some("---") {
        return (Frontmatter::default(), text);
    }
    let mut fm = Frontmatter::default();
    for (idx, (line, line_offset)) in lines.iter().enumerate().skip(1) {
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            let body_start = if idx + 1 < lines.len() {
                lines[idx + 1].1
            } else {
                text.len()
            };
            let _ = line_offset;
            return (fm, &text[body_start..]);
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim().to_ascii_lowercase();
            let value = strip_surrounding_quotes(value.trim()).trim().to_string();
            if value.is_empty() {
                continue;
            }
            let slot = match key.as_str() {
                "name" => &mut fm.name,
                "description" => &mut fm.description,
                "title" => &mut fm.title,
                "type" => &mut fm.ty,
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some(value);
            }
        }
    }
    (Frontmatter::default(), text)
}

/// First Markdown heading text in `body`, `#` markers stripped.
fn first_heading(body: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let trimmed = line.trim_start();
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with([' ', '\t']) {
            let text = trimmed[hashes..].trim();
            (!text.is_empty()).then(|| text.to_string())
        } else {
            None
        }
    })
}

/// Design §8: frontmatter `type` onto the closed core enums.
fn category_for_type(ty: Option<&str>) -> MemoryCategory {
    match ty.map(str::to_ascii_lowercase).as_deref() {
        Some("feedback" | "user") => MemoryCategory::Rule,
        Some("project") => MemoryCategory::Decision,
        _ => MemoryCategory::Note,
    }
}

/// Scan `<root>/.agents/memory/*.md` into workspace `Memory` upserts.
fn scan_memories(root: &Path, by: &str, at: &At, scan: &mut WorkspaceImportScan) {
    let mem_dir = root.join(".agents").join("memory");
    let index_path = mem_dir.join("MEMORY.md");
    // `MEMORY.md` is the hand index: read for pinned detection, never imported.
    let index_text = read_utf8(&index_path).unwrap_or_default();

    for path in sorted_dir_paths(&mem_dir) {
        let Some(file_name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if file_name == "MEMORY.md" {
            continue;
        }
        let is_md = path
            .extension()
            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("md"));
        if !is_md || !path.is_file() {
            continue;
        }
        let skipped = |scan: &mut WorkspaceImportScan, reason: String| {
            scan.skipped.push(format!("{}: {reason}", path.display()));
        };
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(slug) = derive_import_slug(&stem) else {
            skipped(
                scan,
                format!("cannot derive a slug from file stem `{stem}`"),
            );
            continue;
        };
        let text = match read_utf8(&path) {
            Ok(t) => t,
            Err(e) => {
                skipped(scan, e);
                continue;
            }
        };
        let (fm, body) = split_frontmatter(&text);
        if body.len() > MAX_BODY_BYTES {
            skipped(
                scan,
                format!("body exceeds {MAX_BODY_BYTES} bytes ({} bytes)", body.len()),
            );
            continue;
        }
        let summary = [
            fm.description.as_deref(),
            fm.title.as_deref(),
            first_heading(body).as_deref(),
            fm.name.as_deref(),
            Some(slug.as_str()),
        ]
        .into_iter()
        .flatten()
        .map(sanitize_summary)
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| slug.clone());
        scan.upserts.push(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug,
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Memory(MemoryEntry {
                category: category_for_type(fm.ty.as_deref()),
                basis: MemoryBasis::Ruling,
                lifecycle: MemoryLifecycle::Active,
                superseded_by: None,
                pinned: index_text.contains(&file_name),
                summary,
                body: body.to_string(),
            }),
            at: at.clone(),
            by: by.to_string(),
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionKind {
    Shipped,
    NextSteps,
    Blockers,
    InFlight,
    Other,
}

/// Map a heading to a handoff list by lowercase substring (design §3).
fn section_kind(heading: &str) -> SectionKind {
    let normalized = heading.to_ascii_lowercase().replace(['-', '_'], " ");
    let has = |needle: &str| normalized.contains(needle);
    if has("shipped") || has("completed") || has("done") || has("accepted") {
        SectionKind::Shipped
    } else if has("open work") || has("next") || has("todo") || has("what to do next") {
        SectionKind::NextSteps
    } else if has("blocker") || has("blocked") {
        SectionKind::Blockers
    } else if has("current")
        || has("resume")
        || has("handoff")
        || has("in flight")
        || has("in progress")
    {
        SectionKind::InFlight
    } else {
        SectionKind::Other
    }
}

/// `(level, text)` when `line` is an ATX heading.
fn parse_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&level) && trimmed[level..].starts_with([' ', '\t']) {
        let text = trimmed[level..].trim().trim_end_matches('#').trim();
        (!text.is_empty()).then(|| (level, text.to_string()))
    } else {
        None
    }
}

/// Bullet text when `line` starts (after indent) with `-`, `*`, or `+`.
fn parse_bullet(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    match chars.next() {
        Some('-' | '*' | '+') => {
            let rest = chars.as_str();
            if rest.starts_with([' ', '\t']) {
                Some(rest.split_whitespace().collect::<Vec<_>>().join(" "))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Parse one `STATE.md` into a `HandoffEntry` for project `project_slug`.
fn parse_handoff(text: &str, project_slug: &str) -> HandoffEntry {
    let mut shipped: Vec<String> = Vec::new();
    let mut in_flight: Vec<String> = Vec::new();
    let mut blockers: Vec<String> = Vec::new();
    let mut next_steps: Vec<String> = Vec::new();
    let mut notes_lines: Vec<String> = Vec::new();

    let mut h1: Option<String> = None;
    let mut first_substantive: Option<String> = None;
    let mut kind = SectionKind::Other;
    let mut pending_heading: Option<String> = None;
    // The list that an indented continuation line extends, if any.
    let mut open_bullet: Option<SectionKind> = None;
    let mut notes_bullet_open = false;

    let note_heading = |notes: &mut Vec<String>, pending: &mut Option<String>| {
        if let Some(h) = pending.take() {
            if !notes.is_empty() {
                notes.push(String::new());
            }
            notes.push(format!("## {h}"));
        }
    };
    let note_substantive = |first: &mut Option<String>, text: &str| {
        if first.is_none() {
            let cleaned = sanitize_summary(text);
            if !cleaned.is_empty() {
                *first = Some(cleaned);
            }
        }
    };

    for line in text.lines() {
        if let Some((level, heading)) = parse_heading(line) {
            if level == 1 && h1.is_none() {
                h1 = Some(heading.clone());
            }
            kind = section_kind(&heading);
            pending_heading = Some(heading);
            open_bullet = None;
            notes_bullet_open = false;
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" || trimmed == "***" || trimmed == "___" {
            open_bullet = None;
            notes_bullet_open = false;
            continue;
        }
        if let Some(bullet) = parse_bullet(line) {
            if bullet.is_empty() {
                continue;
            }
            note_substantive(&mut first_substantive, &bullet);
            notes_bullet_open = false;
            match kind {
                SectionKind::Shipped => {
                    shipped.push(bullet);
                    open_bullet = Some(SectionKind::Shipped);
                }
                SectionKind::NextSteps => {
                    next_steps.push(bullet);
                    open_bullet = Some(SectionKind::NextSteps);
                }
                SectionKind::Blockers => {
                    blockers.push(bullet);
                    open_bullet = Some(SectionKind::Blockers);
                }
                SectionKind::InFlight => {
                    in_flight.push(bullet);
                    open_bullet = Some(SectionKind::InFlight);
                }
                SectionKind::Other => {
                    note_heading(&mut notes_lines, &mut pending_heading);
                    notes_lines.push(format!("- {bullet}"));
                    open_bullet = None;
                    notes_bullet_open = true;
                }
            }
            continue;
        }
        // Wrapped continuation: an indented line joins the previous bullet.
        let indented = line.starts_with([' ', '\t']);
        let joined = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
        if indented {
            let target = match open_bullet {
                Some(SectionKind::Shipped) => shipped.last_mut(),
                Some(SectionKind::NextSteps) => next_steps.last_mut(),
                Some(SectionKind::Blockers) => blockers.last_mut(),
                Some(SectionKind::InFlight) => in_flight.last_mut(),
                _ => None,
            };
            if let Some(last) = target {
                last.push(' ');
                last.push_str(&joined);
                continue;
            }
            if notes_bullet_open && let Some(last) = notes_lines.last_mut() {
                last.push(' ');
                last.push_str(&joined);
                continue;
            }
        }
        // Paragraph: preserved into notes (with heading context) whichever
        // section it sits in, so import never drops prose.
        open_bullet = None;
        notes_bullet_open = false;
        note_substantive(&mut first_substantive, trimmed);
        note_heading(&mut notes_lines, &mut pending_heading);
        notes_lines.push(trimmed.to_string());
    }

    let summary = first_substantive
        .filter(|s| !s.is_empty())
        .or_else(|| {
            h1.as_deref()
                .map(sanitize_summary)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| format!("Imported project state for {project_slug}"));
    let notes_text = notes_lines.join("\n").trim().to_string();
    let notes = if notes_text.is_empty() {
        None
    } else {
        Some(truncate_bytes(&notes_text, MAX_NOTES_BYTES))
    };
    HandoffEntry {
        summary,
        shipped,
        in_flight,
        blockers,
        next_steps,
        notes,
    }
}

/// Scan immediate `<root>/Notes/<dir>/STATE.md` files into project handoffs.
fn scan_handoffs(root: &Path, by: &str, at: &At, scan: &mut WorkspaceImportScan) {
    let notes_dir = root.join("Notes");
    for dir in sorted_dir_paths(&notes_dir) {
        if !dir.is_dir() {
            continue;
        }
        let state_path = dir.join("STATE.md");
        if !state_path.is_file() {
            continue;
        }
        let dir_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(project_slug) = derive_import_slug(&dir_name) else {
            scan.skipped.push(format!(
                "{}: cannot derive a slug from directory name `{dir_name}`",
                state_path.display()
            ));
            continue;
        };
        let text = match read_utf8(&state_path) {
            Ok(t) => t,
            Err(e) => {
                scan.skipped.push(format!("{}: {e}", state_path.display()));
                continue;
            }
        };
        scan.upserts.push(UpsertStateArgs {
            scope: StateScope::Project(project_slug.clone()),
            slug: "project-state".to_string(),
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Handoff(parse_handoff(&text, &project_slug)),
            at: at.clone(),
            by: by.to_string(),
        });
    }
}

/// Scan `workspace_root` once: memories first (sorted), then handoffs
/// (sorted), all stamped with the same `by`/`at`. Read-only; failures land
/// in `skipped` and never abort the scan.
pub fn scan_workspace_import(workspace_root: &Path, by: &str, at: At) -> WorkspaceImportScan {
    let mut scan = WorkspaceImportScan::default();
    scan_memories(workspace_root, by, &at, &mut scan);
    scan_handoffs(workspace_root, by, &at, &mut scan);
    scan
}

/// Apply scanned `upserts` to `store`. A failed upsert is appended to the
/// returned `skipped` (seeded from the scan's) and never aborts the run.
pub fn apply_workspace_import<S: StateStore>(
    store: &S,
    upserts: Vec<UpsertStateArgs>,
    skipped: Vec<String>,
) -> WorkspaceImportReport {
    let mut report = WorkspaceImportReport {
        skipped,
        ..WorkspaceImportReport::default()
    };
    let mut scopes: BTreeSet<String> = BTreeSet::new();
    for args in upserts {
        let concern = args.payload.concern();
        let scope_wire = args.scope.as_wire();
        let slug = args.slug.clone();
        match store.upsert_state(args) {
            Ok(_) => {
                scopes.insert(scope_wire);
                match concern {
                    StateConcern::Memory => report.imported_memories += 1,
                    StateConcern::Handoff => report.imported_handoffs += 1,
                    StateConcern::Runtime => {}
                }
            }
            Err(e) => report.skipped.push(format!("{scope_wire}/{slug}: {e}")),
        }
    }
    report.scopes = scopes.into_iter().collect();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use tempfile::TempDir;

    fn at() -> At {
        At::from_unix_millis(1_727_658_123_004)
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn memory(scan: &WorkspaceImportScan, slug: &str) -> MemoryEntry {
        scan.upserts
            .iter()
            .find(|u| u.slug == slug)
            .and_then(|u| match &u.payload {
                StatePayload::Memory(m) => Some(m.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no memory upsert `{slug}`"))
    }

    fn handoff(scan: &WorkspaceImportScan, project: &str) -> HandoffEntry {
        scan.upserts
            .iter()
            .find(|u| u.scope == StateScope::Project(project.to_string()))
            .and_then(|u| match &u.payload {
                StatePayload::Handoff(h) => Some(h.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no handoff upsert for `{project}`"))
    }

    fn fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join(".agents/memory/MEMORY.md"),
            "# Index\n\n- [Pinned rule](pinned-rule.md)\n",
        );
        write(
            &root.join(".agents/memory/pinned-rule.md"),
            "---\nname: pinned-rule\ndescription: \"Obey the pinned rule\"\nmetadata:\n  type: user\n---\n\nBody of the pinned rule.\n",
        );
        write(
            &root.join(".agents/memory/no-front.md"),
            "# Plain Heading Here\n\nSome plain body for the fallback memory.\n",
        );
        write(
            &root.join(".agents/memory/2026-retro.md"),
            "---\ntitle: Retro notes\ntype: project\n---\nDecided things body.\n",
        );
        write(
            &root.join("Notes/ArtificialHumanity.io/STATE.md"),
            "# Project State — ArtificialHumanity.io\n\nIntro paragraph stays in notes.\n\n## Shipped on main\n\n- First shipped thing\n  with wrapped detail\n- Second shipped thing\n\n## Current State\n\n- Live at the apex domain\n\n## Blockers\n\n- Waiting on DNS\n\n## Next steps\n\n- Ship the importer\n\n## Odd Section\n\n- Loose thought worth keeping\n\nLoose paragraph worth keeping.\n",
        );
        tmp
    }

    #[test]
    fn slug_derivation_folds_prefixes_and_truncates() {
        assert_eq!(
            derive_import_slug("ArtificialHumanity.io").as_deref(),
            Some("artificialhumanity-io")
        );
        assert_eq!(
            derive_import_slug("2026-retro").as_deref(),
            Some("m-2026-retro")
        );
        assert_eq!(
            derive_import_slug("Hello  World!").as_deref(),
            Some("hello-world")
        );
        assert_eq!(derive_import_slug("___"), None);
        assert_eq!(derive_import_slug("!!!"), None);
        let long = derive_import_slug(&"a".repeat(100)).unwrap();
        assert_eq!(long.len(), 64);
    }

    #[test]
    fn scan_reads_nested_metadata_type_and_pinned_index() {
        let tmp = fixture();
        let scan = scan_workspace_import(tmp.path(), "tester", at());
        assert!(scan.skipped.is_empty(), "{:?}", scan.skipped);
        let m = memory(&scan, "pinned-rule");
        assert_eq!(m.category, MemoryCategory::Rule);
        assert_eq!(m.basis, MemoryBasis::Ruling);
        assert_eq!(m.summary, "Obey the pinned rule");
        assert!(m.pinned, "filename listed in MEMORY.md must pin");
        assert!(m.body.contains("Body of the pinned rule."));
        assert!(!m.body.contains("metadata:"));
    }

    #[test]
    fn scan_falls_back_without_frontmatter_and_for_digit_stems() {
        let tmp = fixture();
        let scan = scan_workspace_import(tmp.path(), "tester", at());
        let plain = memory(&scan, "no-front");
        assert_eq!(plain.summary, "Plain Heading Here");
        assert_eq!(plain.category, MemoryCategory::Note);
        assert!(!plain.pinned);
        let retro = memory(&scan, "m-2026-retro");
        assert_eq!(retro.summary, "Retro notes");
        assert_eq!(retro.category, MemoryCategory::Decision);
        // MEMORY.md itself is never an import source.
        assert!(scan.upserts.iter().all(|u| u.slug != "memory"));
        assert_eq!(scan.memory_count(), 3);
    }

    #[test]
    fn scan_maps_notes_sections_wrapped_bullets_and_dotted_dirs() {
        let tmp = fixture();
        let scan = scan_workspace_import(tmp.path(), "tester", at());
        let upsert = scan
            .upserts
            .iter()
            .find(|u| matches!(u.payload, StatePayload::Handoff(_)))
            .unwrap();
        assert_eq!(upsert.slug, "project-state");
        assert_eq!(
            upsert.scope,
            StateScope::Project("artificialhumanity-io".to_string())
        );
        let h = handoff(&scan, "artificialhumanity-io");
        assert_eq!(
            h.shipped,
            vec![
                "First shipped thing with wrapped detail",
                "Second shipped thing"
            ]
        );
        assert_eq!(h.in_flight, vec!["Live at the apex domain"]);
        assert_eq!(h.blockers, vec!["Waiting on DNS"]);
        assert_eq!(h.next_steps, vec!["Ship the importer"]);
        assert_eq!(h.summary, "Intro paragraph stays in notes.");
        let notes = h.notes.as_deref().unwrap();
        assert!(notes.contains("Odd Section"), "{notes}");
        assert!(notes.contains("Loose thought worth keeping"), "{notes}");
        assert!(notes.contains("Loose paragraph worth keeping."), "{notes}");
        assert!(notes.contains("Intro paragraph stays in notes."), "{notes}");
    }

    #[test]
    fn apply_reports_counts_scopes_and_reimport_is_idempotent() {
        let tmp = fixture();
        let store = MemStore::default();
        let scan = scan_workspace_import(tmp.path(), "tester", at());
        let scan_scopes = scan.scopes();
        let report = apply_workspace_import(&store, scan.upserts, scan.skipped);
        assert_eq!(report.imported_memories, 3);
        assert_eq!(report.imported_handoffs, 1);
        assert_eq!(
            report.scopes,
            vec!["project:artificialhumanity-io", "workspace"]
        );
        assert_eq!(scan_scopes, report.scopes);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(store.list_state(None, None).unwrap().len(), 4);

        // Re-import at the same deterministic keys updates, never duplicates.
        let scan2 = scan_workspace_import(tmp.path(), "tester", at());
        let report2 = apply_workspace_import(&store, scan2.upserts, scan2.skipped);
        assert_eq!(report2.imported_memories, 3);
        assert_eq!(report2.imported_handoffs, 1);
        assert_eq!(store.list_state(None, None).unwrap().len(), 4);
    }

    #[test]
    fn scan_skips_invalid_utf8_and_oversize_bodies_without_aborting() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let bad = root.join(".agents/memory/bad-bytes.md");
        fs::create_dir_all(bad.parent().unwrap()).unwrap();
        fs::write(&bad, [0xff, 0xfe, b'x']).unwrap();
        write(
            &root.join(".agents/memory/huge.md"),
            &"x".repeat(MAX_BODY_BYTES + 1),
        );
        write(&root.join(".agents/memory/fine.md"), "# Fine\n\nok body\n");
        let scan = scan_workspace_import(root, "tester", at());
        assert_eq!(scan.memory_count(), 1);
        assert_eq!(scan.skipped.len(), 2);
        assert!(scan.skipped.iter().all(|s| s.contains(".md:")));
        assert!(scan.skipped.iter().any(|s| s.contains("bad-bytes")));
        assert!(scan.skipped.iter().any(|s| s.contains("huge")));
    }

    #[test]
    fn resolve_workspace_root_walks_up_and_falls_back_to_start() {
        let tmp = fixture();
        let deep = tmp.path().join("a/b/c");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(resolve_workspace_root(&deep), tmp.path().to_path_buf());
        let empty = TempDir::new().unwrap();
        assert_eq!(
            resolve_workspace_root(empty.path()),
            empty.path().to_path_buf()
        );
    }

    #[test]
    fn apply_appends_upsert_failures_to_skipped() {
        let store = MemStore::default();
        let bad = UpsertStateArgs {
            scope: StateScope::Workspace,
            slug: "ok-slug".to_string(),
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Memory(MemoryEntry {
                category: MemoryCategory::Note,
                basis: MemoryBasis::Ruling,
                lifecycle: MemoryLifecycle::Active,
                superseded_by: None,
                pinned: false,
                summary: String::new(),
                body: "body".to_string(),
            }),
            at: at(),
            by: "tester".to_string(),
        };
        let report = apply_workspace_import(&store, vec![bad], vec!["scan note".into()]);
        assert_eq!(report.imported_memories, 0);
        assert_eq!(report.skipped.len(), 2);
        assert_eq!(report.skipped[0], "scan note");
        assert!(report.skipped[1].starts_with("workspace/ok-slug:"));
    }
}
