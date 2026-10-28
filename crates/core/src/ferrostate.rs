//! One-shot workspace importer: legacy `.agents/memory/*.md` files become
//! `Memory` entries and `Notes/*/STATE.md` files become `Handoff` entries
//! (FerroState Phase 2 design §3, §8). Also automatic git session
//! checkpoints (§4): [`probe_git_repo`] plus [`create_state_checkpoint`].
//!
//! The scan/apply split is shared by the CLI (`fl state import-workspace`)
//! and the Hub (`POST /api/v1/state/import-workspace`) so neither surface
//! duplicates the parsing: [`scan_workspace_import`] only reads the tree and
//! produces [`UpsertStateArgs`], and [`apply_workspace_import`] writes them
//! through any [`StateStore`]. A per-file or per-upsert failure lands in
//! `skipped` and never aborts the run.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::at::At;
use crate::state::{
    HandoffEntry, MAX_BODY_BYTES, MAX_NOTES_BYTES, MAX_SUMMARY_CHARS, MemoryBasis, MemoryCategory,
    MemoryEntry, MemoryLifecycle, SensitivityPatch, StateConcern, StateEntry, StatePayload,
    StateScope, UpsertStateArgs, validate_slug,
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

/// Cap for one memory file: frontmatter headroom above the body limit.
const MAX_MEMORY_FILE_BYTES: usize = MAX_BODY_BYTES + 64 * 1024;
/// Cap for the `MEMORY.md` index (pinned detection only).
const MAX_INDEX_BYTES: usize = 1024 * 1024;
/// Cap for one `Notes/*/STATE.md` file.
const MAX_STATE_MD_BYTES: usize = 1024 * 1024;

fn read_utf8_bounded(path: &Path, cap: usize) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > cap as u64 {
        return Err(format!("file exceeds {cap} bytes ({} bytes)", meta.len()));
    }
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > cap {
        return Err(format!("file exceeds {cap} bytes"));
    }
    String::from_utf8(bytes).map_err(|e| format!("invalid UTF-8: {e}"))
}

/// Canonicalize `path` and require it to stay under `canon_root`.
fn contained_canonical(path: &Path, canon_root: &Path) -> Result<PathBuf, String> {
    let canon = fs::canonicalize(path).map_err(|e| format!("cannot resolve path: {e}"))?;
    if canon.starts_with(canon_root) {
        Ok(canon)
    } else {
        Err(format!(
            "path escapes workspace root (resolves to {})",
            canon.display()
        ))
    }
}

/// Resolve an optional source directory: `None` (quiet) when it does not
/// exist, `None` with a `skipped` entry when it exists but cannot be
/// resolved or escapes the workspace, else its canonical path.
fn contained_dir(dir: &Path, canon_root: &Path, skipped: &mut Vec<String>) -> Option<PathBuf> {
    match fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            skipped.push(format!("{}: {e}", dir.display()));
            return None;
        }
    }
    match contained_canonical(dir, canon_root) {
        Ok(canon) => Some(canon),
        Err(e) => {
            skipped.push(format!("{}: {e}", dir.display()));
            None
        }
    }
}

/// Sorted paths of the entries directly inside `dir`. A missing directory
/// yields an empty list quietly; other `read_dir` failures (including
/// per-entry errors) land in `skipped`.
fn sorted_dir_paths(dir: &Path, skipped: &mut Vec<String>) -> Vec<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            skipped.push(format!("{}: {e}", dir.display()));
            return Vec::new();
        }
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(e) => skipped.push(format!("{}: {e}", dir.display())),
        }
    }
    paths.sort();
    paths
}

/// Reserve `(scope, concern, slug)`; `false` when an earlier source won.
fn reserve_key(
    seen: &mut BTreeSet<(StateScope, StateConcern, String)>,
    scope: &StateScope,
    concern: StateConcern,
    slug: &str,
) -> bool {
    seen.insert((scope.clone(), concern, slug.to_string()))
}

/// Whether `file_name` appears in `index_text` bounded by characters that
/// cannot be part of a filename, so `rule.md` is not pinned by `unrule.md`.
fn is_pinned(index_text: &str, file_name: &str) -> bool {
    if file_name.is_empty() {
        return false;
    }
    let filenameish = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
    let bytes = index_text.as_bytes();
    let mut start = 0usize;
    while let Some(pos) = index_text[start..].find(file_name) {
        let at = start + pos;
        let before_ok = at == 0 || !filenameish(bytes[at - 1]);
        let after = at + file_name.len();
        let after_ok = after >= bytes.len() || !filenameish(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = at + file_name.chars().next().map_or(1, char::len_utf8);
    }
    false
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
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
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
fn scan_memories(
    root: &Path,
    canon_root: &Path,
    by: &str,
    at: &At,
    scan: &mut WorkspaceImportScan,
    seen: &mut BTreeSet<(StateScope, StateConcern, String)>,
) {
    let mem_dir = root.join(".agents").join("memory");
    let Some(canon_mem_dir) = contained_dir(&mem_dir, canon_root, &mut scan.skipped) else {
        return;
    };
    // `MEMORY.md` is the hand index: read for pinned detection, never
    // imported. Missing is fine; an existing but escaping, unreadable, or
    // oversize index yields no pinned detection without aborting memories.
    let index_path = canon_mem_dir.join("MEMORY.md");
    let index_text = match fs::symlink_metadata(&index_path) {
        Ok(_) => match contained_canonical(&index_path, canon_root)
            .and_then(|canon| read_utf8_bounded(&canon, MAX_INDEX_BYTES))
        {
            Ok(t) => t,
            Err(e) => {
                scan.skipped.push(format!("{}: {e}", index_path.display()));
                String::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            scan.skipped.push(format!("{}: {e}", index_path.display()));
            String::new()
        }
    };

    for path in sorted_dir_paths(&canon_mem_dir, &mut scan.skipped) {
        let Some(file_name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if file_name.eq_ignore_ascii_case("MEMORY.md") {
            continue;
        }
        let is_md = path
            .extension()
            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("md"));
        if !is_md {
            continue;
        }
        // Plain subdirectories are not candidates; files and symlinks are.
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_dir() => continue,
            Ok(_) => {}
            Err(e) => {
                scan.skipped.push(format!("{}: {e}", path.display()));
                continue;
            }
        }
        let canon_path = match contained_canonical(&path, canon_root) {
            Ok(canon) => canon,
            Err(e) => {
                scan.skipped.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        if !canon_path.is_file() {
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
        let text = match read_utf8_bounded(&canon_path, MAX_MEMORY_FILE_BYTES) {
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
        if !reserve_key(seen, &StateScope::Workspace, StateConcern::Memory, &slug) {
            skipped(
                scan,
                format!(
                    "slug `{slug}` collides with an earlier source for scope `workspace` and concern `memory`"
                ),
            );
            continue;
        }
        scan.upserts.push(UpsertStateArgs {
            scope: StateScope::Workspace,
            slug,
            sensitivity: SensitivityPatch::Inherit,
            payload: StatePayload::Memory(MemoryEntry {
                category: category_for_type(fm.ty.as_deref()),
                basis: MemoryBasis::Ruling,
                lifecycle: MemoryLifecycle::Active,
                superseded_by: None,
                pinned: is_pinned(&index_text, &file_name),
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

/// Bullet text when `line` starts (after indent) with `-`, `*`, `+`, or an
/// ordered marker (`1.`, `2)`).
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
        Some(c) if c.is_ascii_digit() => {
            let digits = 1 + chars.clone().take_while(char::is_ascii_digit).count();
            let after_digits = &trimmed[digits..];
            let mut marker = after_digits.chars();
            match marker.next() {
                Some('.' | ')') => {
                    let rest = marker.as_str();
                    if rest.starts_with([' ', '\t']) {
                        Some(rest.split_whitespace().collect::<Vec<_>>().join(" "))
                    } else {
                        None
                    }
                }
                _ => None,
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
    // Mapped ancestors by heading level; unmapped subheadings inherit the
    // nearest mapped ancestor's kind.
    let mut heading_stack: Vec<(usize, SectionKind)> = Vec::new();
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
            while heading_stack
                .last()
                .is_some_and(|(stack_level, _)| *stack_level >= level)
            {
                heading_stack.pop();
            }
            let mapped = section_kind(&heading);
            if mapped != SectionKind::Other {
                heading_stack.push((level, mapped));
                kind = mapped;
            } else {
                kind = heading_stack
                    .last()
                    .map_or(SectionKind::Other, |(_, ancestor)| *ancestor);
            }
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
fn scan_handoffs(
    root: &Path,
    canon_root: &Path,
    by: &str,
    at: &At,
    scan: &mut WorkspaceImportScan,
    seen: &mut BTreeSet<(StateScope, StateConcern, String)>,
) {
    let notes_dir = root.join("Notes");
    let Some(canon_notes_dir) = contained_dir(&notes_dir, canon_root, &mut scan.skipped) else {
        return;
    };
    for dir in sorted_dir_paths(&canon_notes_dir, &mut scan.skipped) {
        // Plain files are not project directories; dirs and symlinks are
        // candidates whose resolution must stay inside the workspace.
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_dir() || meta.file_type().is_symlink() => {}
            Ok(_) => continue,
            Err(e) => {
                scan.skipped.push(format!("{}: {e}", dir.display()));
                continue;
            }
        }
        let canon_dir = match contained_canonical(&dir, canon_root) {
            Ok(canon) => canon,
            Err(e) => {
                scan.skipped.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        if !canon_dir.is_dir() {
            continue;
        }
        let state_path = dir.join("STATE.md");
        match fs::symlink_metadata(&state_path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                scan.skipped.push(format!("{}: {e}", state_path.display()));
                continue;
            }
        }
        let canon_state = match contained_canonical(&state_path, canon_root) {
            Ok(canon) => canon,
            Err(e) => {
                scan.skipped.push(format!("{}: {e}", state_path.display()));
                continue;
            }
        };
        if !canon_state.is_file() {
            scan.skipped
                .push(format!("{}: not a regular file", state_path.display()));
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
        let text = match read_utf8_bounded(&canon_state, MAX_STATE_MD_BYTES) {
            Ok(t) => t,
            Err(e) => {
                scan.skipped.push(format!("{}: {e}", state_path.display()));
                continue;
            }
        };
        let scope = StateScope::Project(project_slug.clone());
        if !reserve_key(seen, &scope, StateConcern::Handoff, "project-state") {
            scan.skipped.push(format!(
                "{}: slug `project-state` collides with an earlier source for scope `{}` and concern `handoff`",
                state_path.display(),
                scope.as_wire()
            ));
            continue;
        }
        scan.upserts.push(UpsertStateArgs {
            scope,
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
    let canon_root = match fs::canonicalize(workspace_root) {
        Ok(canon) => canon,
        Err(e) => {
            scan.skipped.push(format!(
                "{}: cannot resolve workspace root: {e}",
                workspace_root.display()
            ));
            return scan;
        }
    };
    let mut seen: BTreeSet<(StateScope, StateConcern, String)> = BTreeSet::new();
    scan_memories(workspace_root, &canon_root, by, &at, &mut scan, &mut seen);
    scan_handoffs(workspace_root, &canon_root, by, &at, &mut scan, &mut seen);
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

/// Maximum dirty entries recorded in a checkpoint's `in_flight` list.
/// The full count is always preserved in the notes and the report.
pub const MAX_CHECKPOINT_DIRTY_ENTRIES: usize = 200;

/// Serializable outcome of a state checkpoint (CLI/MCP/Hub report shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateCheckpointReport {
    pub entry: StateEntry,
    pub branch: String,
    pub head_sha: String,
    /// Full dirty-file count, even when the captured list is capped.
    pub dirty_files: usize,
}

/// Git state probed for a checkpoint. `dirty_entries` holds one
/// `XY path` string per `git status --porcelain=v1 -z` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRepoProbe {
    pub top_level: PathBuf,
    pub branch: String,
    pub head_sha: String,
    pub latest_subject: String,
    pub dirty_entries: Vec<String>,
}

fn run_git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run `git {}`: {e}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let detail = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        };
        return Err(format!(
            "`git {}` failed in {}{detail}",
            args.join(" "),
            repo.display()
        ));
    }
    Ok(output.stdout)
}

fn run_git(repo: &Path, args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&run_git_bytes(repo, args)?).into_owned())
}

/// Parse `git status --porcelain=v1 -z` output into one `XY path` string
/// per entry. NUL termination keeps paths with newlines or non-ASCII
/// bytes intact (decoded lossily, never C-quoted). A rename/copy entry
/// is `XY dest\0src\0`; the following source field is consumed so the
/// rename counts once, formatted `XY src -> dest`.
fn parse_porcelain_z(stdout: &[u8]) -> Vec<String> {
    let mut entries = Vec::new();
    let mut fields = stdout.split(|b| *b == 0);
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let status = String::from_utf8_lossy(&field[..2]).into_owned();
        let path = String::from_utf8_lossy(&field[3..]).into_owned();
        if path.is_empty() {
            continue;
        }
        if field[0] == b'R' || field[0] == b'C' || field[1] == b'R' || field[1] == b'C' {
            // In `-z` mode the source path follows as its own field.
            if let Some(src) = fields.next() {
                let src = String::from_utf8_lossy(src).into_owned();
                entries.push(format!("{status} {src} -> {path}"));
                continue;
            }
        }
        entries.push(format!("{status} {path}"));
    }
    entries
}

/// Probe `repo` with git. A non-repository, a repository with no commits,
/// or any git failure is a clear `Err`; never a bogus checkpoint.
pub fn probe_git_repo(repo: &Path) -> Result<GitRepoProbe, String> {
    let top_level = run_git(repo, &["rev-parse", "--show-toplevel"])
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("not a git repository at {} ({e})", repo.display()))?;
    if top_level.is_empty() {
        return Err(format!("not a git repository at {}", repo.display()));
    }
    let branch = run_git(repo, &["branch", "--show-current"])?;
    let branch = branch.trim();
    let branch = if branch.is_empty() {
        "detached".to_string()
    } else {
        branch.to_string()
    };
    let head_sha = run_git(repo, &["rev-parse", "HEAD"])
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("cannot resolve HEAD in {} ({e})", repo.display()))?;
    if head_sha.is_empty() {
        return Err(format!("no commits yet in {}", repo.display()));
    }
    let latest_subject = sanitize_summary(&run_git(repo, &["log", "-1", "--format=%s"])?);
    let status = run_git_bytes(
        repo,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let dirty_entries = parse_porcelain_z(&status);
    Ok(GitRepoProbe {
        top_level: PathBuf::from(top_level),
        branch,
        head_sha,
        latest_subject,
        dirty_entries,
    })
}

/// Resolve a checkpoint slug: an explicit slug must pass
/// [`validate_slug`]; the default is `checkpoint-<by>` derived to a
/// valid slug (falling back to plain `checkpoint`).
pub fn resolve_checkpoint_slug(slug: Option<&str>, by: &str) -> Result<String, String> {
    match slug {
        Some(raw) => validate_slug(raw).map_err(|e| e.to_string()),
        None => Ok(derive_import_slug(&format!("checkpoint-{by}"))
            .unwrap_or_else(|| "checkpoint".to_string())),
    }
}

/// Build the `Handoff` upsert for a checkpoint from `probe`.
pub fn build_checkpoint_upsert(
    probe: &GitRepoProbe,
    scope: StateScope,
    slug: Option<&str>,
    summary: Option<&str>,
    by: &str,
    at: At,
) -> Result<UpsertStateArgs, String> {
    let slug = resolve_checkpoint_slug(slug, by)?;
    let dirty_total = probe.dirty_entries.len();
    let short_head: String = probe.head_sha.chars().take(7).collect();
    let default_summary = format!(
        "Checkpoint on {} @ {} ({} dirty): {}",
        probe.branch, short_head, dirty_total, probe.latest_subject
    );
    let summary = summary
        .map(sanitize_summary)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| sanitize_summary(&default_summary));
    let summary = if summary.is_empty() {
        format!("Checkpoint on {} @ {}", probe.branch, short_head)
    } else {
        summary
    };
    let mut notes_lines = vec![
        format!("branch: {}", probe.branch),
        format!("head_sha: {}", probe.head_sha),
        format!("dirty_files: {dirty_total}"),
        format!("latest_subject: {}", probe.latest_subject),
        format!("repo_top_level: {}", probe.top_level.display()),
    ];
    if dirty_total > MAX_CHECKPOINT_DIRTY_ENTRIES {
        notes_lines.push(format!(
            "dirty_files_truncated: in_flight lists first {MAX_CHECKPOINT_DIRTY_ENTRIES} of {dirty_total}"
        ));
    }
    let notes = truncate_bytes(&notes_lines.join("\n"), MAX_NOTES_BYTES);
    Ok(UpsertStateArgs {
        scope,
        slug,
        sensitivity: SensitivityPatch::Inherit,
        payload: StatePayload::Handoff(HandoffEntry {
            summary,
            shipped: vec![format!("{} {}", probe.head_sha, probe.latest_subject)],
            in_flight: probe
                .dirty_entries
                .iter()
                .take(MAX_CHECKPOINT_DIRTY_ENTRIES)
                .cloned()
                .collect(),
            blockers: Vec::new(),
            next_steps: Vec::new(),
            notes: Some(notes),
        }),
        at,
        by: by.to_string(),
    })
}

/// Write a checkpoint for an already-probed repository.
pub fn create_state_checkpoint_from_probe<S: StateStore>(
    store: &S,
    probe: &GitRepoProbe,
    scope: StateScope,
    slug: Option<&str>,
    summary: Option<&str>,
    by: &str,
    at: At,
) -> Result<StateCheckpointReport, String> {
    let args = build_checkpoint_upsert(probe, scope, slug, summary, by, at)?;
    let dirty_files = probe.dirty_entries.len();
    let entry = store.upsert_state(args).map_err(|e| e.to_string())?;
    Ok(StateCheckpointReport {
        entry,
        branch: probe.branch.clone(),
        head_sha: probe.head_sha.clone(),
        dirty_files,
    })
}

/// Probe `repo` with git and record the result as a `Handoff` checkpoint.
pub fn create_state_checkpoint<S: StateStore>(
    store: &S,
    repo: &Path,
    scope: StateScope,
    slug: Option<&str>,
    summary: Option<&str>,
    by: &str,
    at: At,
) -> Result<StateCheckpointReport, String> {
    // Validate/resolve the slug before probing git so an invalid explicit
    // slug fails even when `repo` is not a repository.
    let resolved_slug = resolve_checkpoint_slug(slug, by)?;
    let probe = probe_git_repo(repo)?;
    create_state_checkpoint_from_probe(
        store,
        &probe,
        scope,
        Some(resolved_slug.as_str()),
        summary,
        by,
        at,
    )
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

    #[test]
    fn scan_slug_collision_keeps_first_sorted_source() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join(".agents/memory/a b.md"),
            "# Winner\n\nwinner body\n",
        );
        write(
            &root.join(".agents/memory/a-b.md"),
            "# Loser\n\nloser body\n",
        );
        let scan = scan_workspace_import(root, "tester", at());
        assert_eq!(scan.memory_count(), 1, "{:?}", scan.skipped);
        let m = memory(&scan, "a-b");
        assert!(m.body.contains("winner body"), "{}", m.body);
        assert_eq!(scan.skipped.len(), 1);
        assert!(scan.skipped[0].contains("a-b.md"), "{}", scan.skipped[0]);
        assert!(scan.skipped[0].contains("collides"), "{}", scan.skipped[0]);
    }

    #[test]
    fn scan_ordered_bullets_and_subheading_inheritance() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Notes/Demo/STATE.md"),
            "# Demo\n\n## Shipped\n\n### 2026-01-01\n\n- Dated shipped thing\n\n## Next steps\n\n1. First next thing\n2) Second next thing\n\n## Odd Section\n\n- Loose thing\n",
        );
        let scan = scan_workspace_import(root, "tester", at());
        assert!(scan.skipped.is_empty(), "{:?}", scan.skipped);
        let h = handoff(&scan, "demo");
        assert_eq!(h.shipped, vec!["Dated shipped thing"]);
        assert_eq!(h.next_steps, vec!["First next thing", "Second next thing"]);
        let notes = h.notes.as_deref().unwrap_or_default();
        assert!(notes.contains("Loose thing"), "{notes}");
        assert!(!notes.contains("Dated shipped thing"), "{notes}");
    }

    #[test]
    fn scan_bom_crlf_frontmatter_and_unclosed_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join(".agents/memory/bom.md"),
            "\u{feff}---\r\ndescription: BOM summary\r\ntype: user\r\n---\r\nBOM body.\r\n",
        );
        write(
            &root.join(".agents/memory/unclosed.md"),
            "---\ndescription: Not frontmatter\n# Real Heading\n\nwhole body\n",
        );
        let scan = scan_workspace_import(root, "tester", at());
        assert!(scan.skipped.is_empty(), "{:?}", scan.skipped);
        let bom = memory(&scan, "bom");
        assert_eq!(bom.summary, "BOM summary");
        assert_eq!(bom.category, MemoryCategory::Rule);
        assert!(bom.body.contains("BOM body."));
        assert!(!bom.body.contains("description:"));
        let unclosed = memory(&scan, "unclosed");
        assert_eq!(unclosed.summary, "Real Heading");
        assert!(unclosed.body.contains("description: Not frontmatter"));
        assert!(unclosed.body.starts_with("---"));
    }

    #[test]
    fn scan_pinned_detection_is_boundary_aware_and_index_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join(".agents/memory/MEMORY.md"),
            "- [Unrule](unrule.md)\n",
        );
        write(
            &root.join(".agents/memory/rule.md"),
            "# Rule\n\nrule body\n",
        );
        write(
            &root.join(".agents/memory/unrule.md"),
            "# Unrule\n\nunrule body\n",
        );
        write(
            &root.join(".agents/memory/memory.md"),
            "# Lower\n\nnot imported\n",
        );
        let scan = scan_workspace_import(root, "tester", at());
        assert!(!memory(&scan, "rule").pinned);
        assert!(memory(&scan, "unrule").pinned);
        assert!(scan.upserts.iter().all(|u| u.slug != "memory"));
        assert_eq!(scan.memory_count(), 2);
    }

    #[test]
    fn scan_oversize_sources_are_skipped_without_aborting_siblings() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join(".agents/memory/oversize.md"),
            &"x".repeat(MAX_MEMORY_FILE_BYTES + 1),
        );
        write(&root.join(".agents/memory/fine.md"), "# Fine\n\nok body\n");
        write(
            &root.join("Notes/Big/STATE.md"),
            &"x".repeat(MAX_STATE_MD_BYTES + 1),
        );
        write(
            &root.join("Notes/Small/STATE.md"),
            "# Small\n\n## Next steps\n\n- Keep going\n",
        );
        let scan = scan_workspace_import(root, "tester", at());
        assert_eq!(scan.memory_count(), 1);
        assert_eq!(scan.handoff_count(), 1);
        assert!(
            scan.skipped.iter().any(|s| s.contains("oversize")),
            "{:?}",
            scan.skipped
        );
        assert!(
            scan.skipped.iter().any(|s| s.contains("Big")),
            "{:?}",
            scan.skipped
        );
        assert!(
            scan.skipped.iter().all(|s| s.contains("exceeds")),
            "{:?}",
            scan.skipped
        );
    }

    #[test]
    fn scan_non_directory_memory_source_is_skipped_not_silent() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join(".agents/memory"), "not a directory\n");
        write(
            &root.join("Notes/Demo/STATE.md"),
            "# Demo\n\n## Next steps\n\n- Keep going\n",
        );
        let scan = scan_workspace_import(root, "tester", at());
        assert_eq!(scan.memory_count(), 0);
        assert_eq!(scan.handoff_count(), 1);
        assert!(
            scan.skipped.iter().any(|s| s.contains("memory")),
            "{:?}",
            scan.skipped
        );

        // Missing optional sources stay a quiet zero.
        let empty = TempDir::new().unwrap();
        let quiet = scan_workspace_import(empty.path(), "tester", at());
        assert!(quiet.upserts.is_empty());
        assert!(quiet.skipped.is_empty(), "{:?}", quiet.skipped);
    }

    #[cfg(unix)]
    #[test]
    fn scan_symlink_escape_is_skipped_and_internal_symlink_followed() {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("secret.md");
        fs::write(&secret, "# Secret\n\noutside body\n").unwrap();
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let mem_dir = root.join(".agents/memory");
        fs::create_dir_all(&mem_dir).unwrap();
        if symlink(&secret, mem_dir.join("escape.md")).is_err() {
            return;
        }
        let real = root.join("real.md");
        fs::write(&real, "# Real\n\ninside body\n").unwrap();
        if symlink(&real, mem_dir.join("inside.md")).is_err() {
            return;
        }
        let scan = scan_workspace_import(root, "tester", at());
        assert_eq!(scan.memory_count(), 1, "{:?}", scan.skipped);
        assert!(memory(&scan, "inside").body.contains("inside body"));
        assert!(scan.upserts.iter().all(|u| u.slug != "escape"));
        assert!(
            scan.skipped
                .iter()
                .any(|s| s.contains("escape.md") && s.contains("escapes workspace root")),
            "{:?}",
            scan.skipped
        );
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} in {}: {}",
            args,
            repo.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A git repo on `main` with one commit, one modified tracked file,
    /// and one untracked file.
    fn git_fixture() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        git(repo, &["init", "-b", "main"]);
        git(repo, &["config", "user.name", "Checkpoint Tester"]);
        git(repo, &["config", "user.email", "tester@example.invalid"]);
        write(&repo.join("tracked.txt"), "initial\n");
        git(repo, &["add", "tracked.txt"]);
        git(repo, &["commit", "-m", "initial subject line"]);
        write(&repo.join("tracked.txt"), "modified\n");
        write(&repo.join("untracked.txt"), "new\n");
        tmp
    }

    #[test]
    fn checkpoint_probe_and_create_capture_git_state() {
        let tmp = git_fixture();
        let repo = tmp.path();
        let probe = probe_git_repo(repo).unwrap();
        assert_eq!(probe.branch, "main");
        assert_eq!(probe.head_sha, git(repo, &["rev-parse", "HEAD"]));
        assert_eq!(probe.head_sha.len(), 40);
        assert_eq!(probe.latest_subject, "initial subject line");
        assert_eq!(probe.dirty_entries.len(), 2, "{:?}", probe.dirty_entries);
        assert!(
            probe
                .dirty_entries
                .iter()
                .any(|d| d.trim() == "M tracked.txt"),
            "{:?}",
            probe.dirty_entries
        );
        assert!(
            probe.dirty_entries.iter().any(|d| d == "?? untracked.txt"),
            "{:?}",
            probe.dirty_entries
        );

        let store = MemStore::default();
        let report = create_state_checkpoint(
            &store,
            repo,
            StateScope::Workspace,
            None,
            None,
            "tester",
            at(),
        )
        .unwrap();
        assert_eq!(report.branch, "main");
        assert_eq!(report.head_sha, probe.head_sha);
        assert_eq!(report.dirty_files, 2);
        assert_eq!(report.entry.slug, "checkpoint-tester");
        assert_eq!(report.entry.scope, StateScope::Workspace);
        assert_eq!(report.entry.concern, StateConcern::Handoff);
        assert!(!report.entry.sensitive);
        let StatePayload::Handoff(h) = &report.entry.payload else {
            panic!("checkpoint must be a handoff");
        };
        let short: String = probe.head_sha.chars().take(7).collect();
        assert_eq!(
            h.summary,
            format!("Checkpoint on main @ {short} (2 dirty): initial subject line")
        );
        assert_eq!(
            h.shipped,
            vec![format!("{} initial subject line", probe.head_sha)]
        );
        assert_eq!(h.in_flight.len(), 2);
        assert!(h.in_flight.iter().any(|d| d == "?? untracked.txt"));
        let notes = h.notes.as_deref().unwrap();
        assert!(notes.contains("branch: main"), "{notes}");
        assert!(
            notes.contains(&format!("head_sha: {}", probe.head_sha)),
            "{notes}"
        );
        assert!(notes.contains("dirty_files: 2"), "{notes}");
        assert!(notes.contains("initial subject line"), "{notes}");
        assert!(
            notes.contains(&probe.top_level.display().to_string()),
            "{notes}"
        );

        // The report serializes to exactly the shared wire shape.
        let wire = serde_json::to_value(&report).unwrap();
        let obj = wire.as_object().unwrap();
        assert_eq!(obj.len(), 4, "{wire}");
        assert_eq!(wire["branch"], "main");
        assert_eq!(wire["dirty_files"], 2);
        assert_eq!(wire["entry"]["slug"], "checkpoint-tester");
    }

    #[test]
    fn checkpoint_explicit_slug_and_summary_are_sanitized() {
        let tmp = git_fixture();
        let store = MemStore::default();
        let long = format!("  hello\nworld\t  {}", "x".repeat(300));
        let report = create_state_checkpoint(
            &store,
            tmp.path(),
            StateScope::Workspace,
            Some("My-Checkpoint"),
            Some(long.as_str()),
            "tester",
            at(),
        )
        .unwrap();
        assert_eq!(report.entry.slug, "my-checkpoint");
        let StatePayload::Handoff(h) = &report.entry.payload else {
            panic!("checkpoint must be a handoff");
        };
        assert!(!h.summary.contains('\n'), "{}", h.summary);
        assert!(h.summary.starts_with("hello world"), "{}", h.summary);
        assert_eq!(h.summary.chars().count(), MAX_SUMMARY_CHARS);

        let err = create_state_checkpoint(
            &store,
            tmp.path(),
            StateScope::Workspace,
            Some("1bad slug!"),
            None,
            "tester",
            at(),
        )
        .unwrap_err();
        assert!(err.contains("invalid slug"), "{err}");
        assert_eq!(store.list_state(None, None).unwrap().len(), 1);
    }

    #[test]
    fn checkpoint_non_repo_and_empty_repo_are_errors() {
        let plain = TempDir::new().unwrap();
        let err = probe_git_repo(plain.path()).unwrap_err();
        assert!(err.contains("not a git repository"), "{err}");
        let store = MemStore::default();
        assert!(
            create_state_checkpoint(
                &store,
                plain.path(),
                StateScope::Workspace,
                None,
                None,
                "tester",
                at(),
            )
            .is_err()
        );
        assert!(store.list_state(None, None).unwrap().is_empty());

        let empty = TempDir::new().unwrap();
        git(empty.path(), &["init", "-b", "main"]);
        let err = probe_git_repo(empty.path()).unwrap_err();
        assert!(err.contains("HEAD"), "{err}");
    }

    #[test]
    fn checkpoint_probe_expands_untracked_dir_and_preserves_odd_names() {
        let tmp = git_fixture();
        let repo = tmp.path();
        // Baseline: 1 modified + 1 untracked file.
        assert_eq!(probe_git_repo(repo).unwrap().dirty_entries.len(), 2);
        write(&repo.join("newdir/a.txt"), "a\n");
        write(&repo.join("newdir/b.txt"), "b\n");
        write(&repo.join("café.txt"), "x\n");
        write(&repo.join("line\nbreak.txt"), "x\n");
        let probe = probe_git_repo(repo).unwrap();
        assert_eq!(probe.dirty_entries.len(), 6, "{:?}", probe.dirty_entries);
        for expected in [
            "?? newdir/a.txt",
            "?? newdir/b.txt",
            "?? café.txt",
            "?? line\nbreak.txt",
        ] {
            assert!(
                probe.dirty_entries.iter().any(|d| d == expected),
                "missing {expected:?} in {:?}",
                probe.dirty_entries
            );
        }
        assert!(
            probe
                .dirty_entries
                .iter()
                .all(|d| !d.contains('"') && !d.contains("\\303")),
            "paths must not be C-quoted: {:?}",
            probe.dirty_entries
        );
        assert!(
            !probe.dirty_entries.iter().any(|d| d == "?? newdir/"),
            "untracked dir must expand to files: {:?}",
            probe.dirty_entries
        );
    }

    #[test]
    fn checkpoint_probe_rename_counts_as_one_entry() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        git(repo, &["init", "-b", "main"]);
        git(repo, &["config", "user.name", "Checkpoint Tester"]);
        git(repo, &["config", "user.email", "tester@example.invalid"]);
        write(&repo.join("old.txt"), "rename me with enough content\n");
        git(repo, &["add", "old.txt"]);
        git(repo, &["commit", "-m", "before rename"]);
        git(repo, &["mv", "old.txt", "new.txt"]);
        let probe = probe_git_repo(repo).unwrap();
        assert_eq!(probe.dirty_entries.len(), 1, "{:?}", probe.dirty_entries);
        assert!(
            probe.dirty_entries[0].contains("old.txt -> new.txt"),
            "{:?}",
            probe.dirty_entries
        );
    }

    #[test]
    fn checkpoint_invalid_slug_fails_before_git_probe_on_non_repo() {
        let plain = TempDir::new().unwrap();
        let store = MemStore::default();
        let err = create_state_checkpoint(
            &store,
            plain.path(),
            StateScope::Workspace,
            Some("1bad slug!"),
            None,
            "tester",
            at(),
        )
        .unwrap_err();
        assert!(err.contains("invalid slug"), "{err}");
        assert!(!err.contains("not a git repository"), "{err}");
        assert!(store.list_state(None, None).unwrap().is_empty());
    }

    #[test]
    fn checkpoint_detached_head_reports_detached_branch() {
        let tmp = git_fixture();
        git(tmp.path(), &["checkout", "--detach", "HEAD"]);
        let probe = probe_git_repo(tmp.path()).unwrap();
        assert_eq!(probe.branch, "detached");
    }
}
