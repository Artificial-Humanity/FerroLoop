//! Slash-command catalog for `GET /api/v1/commands`: built-in commands, skills
//! (`<skills dir>/*/SKILL.md`) and workflows (`.fl/flows/*.toml`).
//!
//! Skills come from the workspace root, from the workspace parent (the checkout that holds
//! the sub-repos) and, when a home directory is given, from the user's agent skill dirs; see
//! [`skill_roots`] for the scan order, which is also the precedence for a duplicate name.
//!
//! Discovery is read-only and bounded: a directory yields at most [`MAX_ENTRIES_PER_KIND`]
//! entries, one response lists at most [`MAX_TOTAL_SKILLS`] skills, and a file is read up to
//! [`MAX_FILE_BYTES`] bytes. Only `SKILL.md` frontmatter (name and description) is read from a
//! skill; nothing in a skill dir is ever executed. A symlink is followed only when its target
//! stays inside the scanned directory or inside a trusted root (the workspace parent, and the
//! home directory when one is given).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Skills dir, relative to the workspace root.
pub const SKILLS_DIR: &str = ".agents/skills";
/// Workflow manifests dir, relative to the workspace root.
pub const FLOWS_DIR: &str = ".fl/flows";
/// Most skills one scanned directory yields, and most workflows one response lists.
pub const MAX_ENTRIES_PER_KIND: usize = 200;
/// Most skills one response lists across all scanned directories.
pub const MAX_TOTAL_SKILLS: usize = 500;
/// Skills dir, relative to the workspace parent, that holds the lab-wide skills.
pub const LAB_SKILLS_DIR: &str = "AI-Lab-AMD/skills";
/// Skill dirs, relative to the user's home directory, in precedence order.
pub const HOME_SKILLS_DIRS: [&str; 4] = [
    ".agents/skills",
    ".claude/skills",
    ".codex/skills",
    ".gemini/config/skills",
];
/// Most directory entries examined per scan, so a huge directory cannot stall the request.
const MAX_DIR_ENTRIES_SCANNED: usize = 1000;
/// Most bytes read from one `SKILL.md` or flow file.
pub const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_NAME_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 300;

/// One entry of the catalog. The JSON matches the UI's `SlashCommandEntry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEntry {
    pub name: String,
    /// `command`, `skill` or `workflow`.
    pub kind: String,
    pub description: String,
}

fn entry(name: &str, kind: &str, description: &str) -> CommandEntry {
    CommandEntry {
        name: name.to_string(),
        kind: kind.to_string(),
        description: description.to_string(),
    }
}

/// Built-in slash commands. Keep the first six in step with `DEFAULT_SLASH_COMMANDS`
/// in `ui/src/types/api.ts`, the offline fallback the UI uses when this call fails.
pub fn builtin_commands() -> Vec<CommandEntry> {
    vec![
        entry(
            "compact",
            "command",
            "Summarize and compact the active session context window",
        ),
        entry(
            "clear",
            "command",
            "Clear the conversation history for a clean-slate session",
        ),
        entry(
            "model",
            "command",
            "Switch the active language model for this session",
        ),
        entry(
            "goal",
            "command",
            "Run an autonomous long-running task until the goal is achieved",
        ),
        entry(
            "plan",
            "command",
            "Draft a structured step-by-step plan before execution",
        ),
        entry(
            "boost",
            "command",
            "Deep thinking, strategic planning, and rigorous verification",
        ),
        entry(
            "ultrareview",
            "skill",
            "Exhaustive multi-pass review of the current change set",
        ),
        entry(
            "code-review",
            "skill",
            "Subagent-driven code review, finding evaluation, and remediation",
        ),
        entry(
            "subagent-dev",
            "skill",
            "Execute a multi-task plan with per-task implementer and reviewer subagents",
        ),
        entry(
            "ste",
            "skill",
            "Rewrite text in ASD-STE100 Simplified Technical English",
        ),
    ]
}

/// The home directory to scan: `home_override` when set (tests, a hub with its own home),
/// else the `HOME` environment variable. An empty or relative `HOME` means no home.
pub fn resolve_home(home_override: Option<&Path>) -> Option<PathBuf> {
    home_from(home_override, std::env::var_os("HOME"))
}

fn home_from(
    home_override: Option<&Path>,
    env_home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(home) = home_override {
        return Some(home.to_path_buf());
    }
    let home = PathBuf::from(env_home.filter(|h| !h.is_empty())?);
    home.is_absolute().then_some(home)
}

/// A directory to scan for skills, with the roots a symlink in it may point into.
struct SkillRoots {
    /// Directories in scan order; the first one that lists a name wins.
    dirs: Vec<PathBuf>,
    /// Canonical roots, besides the scanned directory, that a symlink target may lie in.
    trusted: Vec<PathBuf>,
}

/// A canonical directory worth trusting: it exists and is not the filesystem root.
fn trusted_root(dir: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(dir).ok()?;
    (canonical.is_dir() && canonical.parent().is_some()).then_some(canonical)
}

/// Skill dirs for the workspace at `root`, in precedence order: `<root>/.agents/skills`, then
/// `<parent>/.agents/skills` and `<parent>/AI-Lab-AMD/skills` (the parent holds the sub-repos
/// the hub can run in), then, with a home, [`HOME_SKILLS_DIRS`] under it. A parent or home that
/// is the filesystem root is neither scanned nor trusted.
fn skill_roots(root: &Path, home: Option<&Path>) -> SkillRoots {
    let mut dirs = vec![root.join(SKILLS_DIR)];
    let mut trusted = Vec::new();
    let parent = fs::canonicalize(root)
        .ok()
        .and_then(|r| r.parent().map(Path::to_path_buf))
        .and_then(|p| trusted_root(&p));
    if let Some(parent) = parent {
        dirs.push(parent.join(SKILLS_DIR));
        dirs.push(parent.join(LAB_SKILLS_DIR));
        trusted.push(parent);
    }
    if let Some(home) = home.and_then(trusted_root) {
        dirs.extend(HOME_SKILLS_DIRS.iter().map(|d| home.join(d)));
        trusted.push(home);
    }
    SkillRoots { dirs, trusted }
}

/// The catalog for the workspace at `root`, without a home directory: built-ins, skills from
/// the workspace and its parent, and workflows. See [`list_commands_with_home`].
pub fn list_commands(root: &Path) -> Vec<CommandEntry> {
    list_commands_with_home(root, None)
}

/// The full catalog for the workspace at `root`: built-ins, skills and workflows, sorted by
/// `kind` then `name`, one entry per `(kind, name)`. A built-in wins over a discovered entry
/// of the same kind and name, and an earlier skill dir (see [`skill_roots`]) over a later one.
/// With `home`, the user's agent skill dirs are scanned too. Workflows come from `root` only.
pub fn list_commands_with_home(root: &Path, home: Option<&Path>) -> Vec<CommandEntry> {
    let mut all = builtin_commands();
    let roots = skill_roots(root, home);
    let mut skills = Vec::new();
    let mut seen = Vec::new();
    for dir in &roots.dirs {
        if skills.len() >= MAX_TOTAL_SKILLS {
            break;
        }
        scan_skills(dir, &roots.trusted, &mut seen, &mut skills);
    }
    all.extend(skills);
    all.extend(discover_flows(&root.join(FLOWS_DIR)));
    // Stable sort keeps the built-in ahead of a discovered duplicate, so dedup keeps it.
    all.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
    all.dedup_by(|b, a| a.kind == b.kind && a.name == b.name);
    all
}

/// Canonical paths of the entries of `dir` in name order. An entry that is a symlink leaving
/// `dir` and every root in `trusted` is dropped, and so is a second entry that resolves to an
/// already listed path.
fn contained_entries(dir: &Path, trusted: &[PathBuf]) -> Vec<PathBuf> {
    let Ok(root) = fs::canonicalize(dir) else {
        return Vec::new();
    };
    if !root.is_dir() {
        return Vec::new();
    }
    let Ok(read) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .take(MAX_DIR_ENTRIES_SCANNED)
        .map(|e| e.path())
        .collect();
    paths.sort();
    let mut seen = Vec::new();
    for canonical in paths.into_iter().filter_map(|p| fs::canonicalize(p).ok()) {
        let allowed =
            canonical.starts_with(&root) || trusted.iter().any(|t| canonical.starts_with(t));
        if allowed && !seen.contains(&canonical) {
            seen.push(canonical);
        }
    }
    seen
}

/// Read at most [`MAX_FILE_BYTES`] of `path` as UTF-8; `None` when unreadable or not text.
///
/// The cap can cut a multibyte character in half. That incomplete tail is dropped, so a file
/// whose frontmatter is valid is still read; bytes that are invalid anywhere else are not text.
fn read_capped(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES).read_to_end(&mut bytes).ok()?;
    let capped = u64::try_from(bytes.len()).is_ok_and(|len| len >= MAX_FILE_BYTES);
    match String::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(err) if capped && err.utf8_error().error_len().is_none() => {
            let valid = err.utf8_error().valid_up_to();
            let mut bytes = err.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).ok()
        }
        Err(_) => None,
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.chars().take(MAX_DESCRIPTION_CHARS).collect()
}

fn unquote(value: &str) -> String {
    let v = value.trim();
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

/// `name` and `description` from the YAML frontmatter of a `SKILL.md`. `None` when there is no
/// closed `---` frontmatter block. Supports plain and quoted scalars, and folded (`>`) or
/// literal (`|`) block scalars; other YAML is ignored.
fn parse_frontmatter(text: &str) -> Option<(Option<String>, Option<String>)> {
    // Some editors save UTF-8 with a byte order mark, which would hide the opening fence.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut block: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        block.push(line);
    }
    if !closed {
        return None;
    }
    let mut name = None;
    let mut description = None;
    let mut i = 0;
    while i < block.len() {
        let line = block[i];
        i += 1;
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let parsed = if matches!(value, ">" | "|" | ">-" | "|-" | ">+" | "|+") {
            let mut parts = Vec::new();
            while i < block.len()
                && (block[i].starts_with(char::is_whitespace) || block[i].is_empty())
            {
                parts.push(block[i].trim());
                i += 1;
            }
            parts.join(" ")
        } else {
            unquote(value)
        };
        match key {
            "name" => name = Some(parsed),
            "description" => description = Some(parsed),
            _ => {}
        }
    }
    Some((name, description))
}

/// Skills under `dir`: one per `<dir>/<skill>/SKILL.md` whose frontmatter names it.
/// A `SKILL.md` that is unreadable, not UTF-8, without frontmatter, without a valid `name`,
/// or that resolves outside `dir` is skipped. Symlinks must stay inside `dir`.
pub fn discover_skills(dir: &Path) -> Vec<CommandEntry> {
    let mut out = Vec::new();
    scan_skills(dir, &[], &mut Vec::new(), &mut out);
    out
}

/// Append the skills under `dir` to `out`. A skill whose canonical dir is in `seen`, or whose
/// name `out` already lists, is skipped; this makes the first dir that lists a skill win and
/// collapses symlinked copies. Stops at [`MAX_ENTRIES_PER_KIND`] additions from this dir or
/// when `out` holds [`MAX_TOTAL_SKILLS`]. A `SKILL.md` must lie inside its skill dir or `dir`.
fn scan_skills(
    dir: &Path,
    trusted: &[PathBuf],
    seen: &mut Vec<PathBuf>,
    out: &mut Vec<CommandEntry>,
) {
    let Ok(root) = fs::canonicalize(dir) else {
        return;
    };
    let mut added = 0;
    for skill_dir in contained_entries(dir, trusted) {
        if added >= MAX_ENTRIES_PER_KIND || out.len() >= MAX_TOTAL_SKILLS {
            break;
        }
        if !skill_dir.is_dir() || seen.contains(&skill_dir) {
            continue;
        }
        let Ok(skill_md) = fs::canonicalize(skill_dir.join("SKILL.md")) else {
            continue;
        };
        if !(skill_md.starts_with(&skill_dir) || skill_md.starts_with(&root)) || !skill_md.is_file()
        {
            continue;
        }
        let Some(text) = read_capped(&skill_md) else {
            continue;
        };
        let Some((Some(name), description)) = parse_frontmatter(&text) else {
            continue;
        };
        let name = name.trim().to_string();
        if !valid_name(&name) {
            continue;
        }
        seen.push(skill_dir);
        if out.iter().any(|e| e.name == name) {
            continue;
        }
        out.push(CommandEntry {
            name,
            kind: "skill".to_string(),
            description: one_line(&description.unwrap_or_default()),
        });
        added += 1;
    }
}

/// Workflows under `dir`: one per `*.toml` file. The name is the file stem; the description is
/// the file's `description`, else its `title`, else a generic line. A file that is unreadable,
/// is not valid TOML, has an invalid stem, or resolves outside `dir` is skipped.
pub fn discover_flows(dir: &Path) -> Vec<CommandEntry> {
    let Ok(root) = fs::canonicalize(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for path in contained_entries(dir, &[]) {
        if out.len() >= MAX_ENTRIES_PER_KIND {
            break;
        }
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        debug_assert!(path.starts_with(&root));
        // The name comes from the link's target, so an alias inside `dir` adds no entry.
        let Some(stem) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if !valid_name(&stem) {
            continue;
        }
        let Some(text) = read_capped(&path) else {
            continue;
        };
        let Ok(doc) = text.parse::<toml::Table>() else {
            continue;
        };
        let described = ["description", "title"].iter().find_map(|key| {
            doc.get(*key)
                .and_then(toml::Value::as_str)
                .map(one_line)
                .filter(|s| !s.is_empty())
        });
        out.push(CommandEntry {
            description: described.unwrap_or_else(|| format!("Run the {stem} workflow")),
            name: stem,
            kind: "workflow".to_string(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn frontmatter_parses_plain_quoted_and_block_scalars() {
        let (n, d) =
            parse_frontmatter("---\nname: alpha\ndescription: \"Does a thing\"\n---\nbody")
                .unwrap();
        assert_eq!(n.as_deref(), Some("alpha"));
        assert_eq!(d.as_deref(), Some("Does a thing"));
        let (n, d) = parse_frontmatter(
            "---\nname: beta\ndescription: >\n  folded line one\n  line two\nother: x\n---\n",
        )
        .unwrap();
        assert_eq!(n.as_deref(), Some("beta"));
        assert_eq!(d.as_deref(), Some("folded line one line two"));
        assert!(parse_frontmatter("no frontmatter").is_none());
        assert!(parse_frontmatter("---\nname: unclosed\n").is_none());
    }

    #[test]
    fn skills_and_flows_are_discovered_and_bounded() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join(".agents/skills/alpha/SKILL.md"),
            "---\nname: alpha\ndescription: Alpha skill\n---\n# Alpha\n",
        );
        write(
            &root.join(".agents/skills/bad/SKILL.md"),
            "no frontmatter at all",
        );
        // A name with no `---` fence, and a fence that never closes, are malformed too.
        write(
            &root.join(".agents/skills/unfenced/SKILL.md"),
            "intro\nname: unfenced\ndescription: x\n---\n",
        );
        write(
            &root.join(".agents/skills/unclosed/SKILL.md"),
            "---\nname: unclosed\ndescription: x\n",
        );
        write(
            &root.join(".agents/skills/noname/SKILL.md"),
            "---\ndescription: nameless\n---\n",
        );
        write(
            &root.join(".agents/skills/badname/SKILL.md"),
            "---\nname: has space\ndescription: x\n---\n",
        );
        fs::create_dir_all(root.join(".agents/skills/empty-dir")).unwrap();
        fs::write(root.join(".agents/skills/not-a-dir.md"), "x").unwrap();
        write(
            &root.join(".fl/flows/ship.toml"),
            "title = \"Ship it\"\nstart_step = \"a\"\n",
        );
        write(&root.join(".fl/flows/plain.toml"), "start_step = \"a\"\n");
        write(&root.join(".fl/flows/broken.toml"), "this is = = not toml");
        write(&root.join(".fl/flows/readme.txt"), "ignored");

        let skills = discover_skills(&root.join(SKILLS_DIR));
        assert_eq!(skills, vec![entry("alpha", "skill", "Alpha skill")]);
        let flows = discover_flows(&root.join(FLOWS_DIR));
        assert_eq!(
            flows,
            vec![
                entry("plain", "workflow", "Run the plain workflow"),
                entry("ship", "workflow", "Ship it"),
            ]
        );
    }

    #[test]
    fn read_capped_drops_a_multibyte_character_cut_by_the_cap() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let cap = usize::try_from(MAX_FILE_BYTES).unwrap();
        let head = "---\nname: big\ndescription: Big skill\n---\n";
        // A 3-byte character starting one byte before the cap: the cap keeps only its first byte.
        let mut body = head.to_string();
        body.push_str(&"a".repeat(cap - head.len() - 1));
        body.push('\u{20ac}');
        body.push_str("tail");
        assert!(body.len() > cap && !body.is_char_boundary(cap));
        write(&root.join(".agents/skills/big/SKILL.md"), &body);
        let text = read_capped(&root.join(".agents/skills/big/SKILL.md")).unwrap();
        assert_eq!(text.len(), cap - 1);
        assert_eq!(
            discover_skills(&root.join(SKILLS_DIR)),
            vec![entry("big", "skill", "Big skill")]
        );

        // Bytes that are not UTF-8 anywhere else are still not text.
        let bad = root.join("bad.bin");
        fs::write(&bad, b"---\nname: x\n\xff\xfe---\n").unwrap();
        assert_eq!(read_capped(&bad), None);
        // A short file that ends mid-character was not cut by the cap, so it is not text either.
        let short = root.join("short.bin");
        fs::write(&short, b"abc\xe2\x82").unwrap();
        assert_eq!(read_capped(&short), None);
    }

    #[test]
    fn symlinks_out_of_the_scanned_dirs_are_ignored_and_inside_ones_followed() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("ws");
        let outside = tmp.path().join("outside");
        write(
            &outside.join("evil/SKILL.md"),
            "---\nname: evil\ndescription: escaped\n---\n",
        );
        write(&outside.join("evil-flow.toml"), "title = \"escaped\"\n");
        write(
            &root.join(".agents/skills/real/SKILL.md"),
            "---\nname: real\ndescription: real one\n---\n",
        );
        write(&root.join(".agents/secret.md"), "---\nname: secret\n---\n");
        // Whole skill dir links out, SKILL.md links out, and a flow links out.
        symlink(outside.join("evil"), root.join(".agents/skills/evil")).unwrap();
        fs::create_dir_all(root.join(".agents/skills/linkmd")).unwrap();
        symlink(
            root.join(".agents/secret.md"),
            root.join(".agents/skills/linkmd/SKILL.md"),
        )
        .unwrap();
        fs::create_dir_all(root.join(".fl/flows")).unwrap();
        symlink(
            outside.join("evil-flow.toml"),
            root.join(".fl/flows/escape.toml"),
        )
        .unwrap();
        write(&root.join(".fl/flows/real.toml"), "title = \"Real flow\"\n");
        // A link that stays inside the directory is followed.
        symlink(
            root.join(".fl/flows/real.toml"),
            root.join(".fl/flows/alias.toml"),
        )
        .unwrap();

        let skills = discover_skills(&root.join(SKILLS_DIR));
        assert_eq!(skills.len(), 1, "{skills:?}");
        assert_eq!(skills[0].name, "real");
        let flows = discover_flows(&root.join(FLOWS_DIR));
        let names: Vec<_> = flows.iter().map(|f| f.name.as_str()).collect();
        assert!(!names.contains(&"escape"), "{names:?}");
        // The inside link resolves to `real.toml`, so only `real` is listed (once).
        assert_eq!(names, vec!["real"], "{names:?}");
    }

    #[test]
    fn oversized_and_non_utf8_files_and_entry_counts_are_capped() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Frontmatter that never closes within the byte cap is skipped.
        let huge = format!(
            "---\nname: huge\ndescription: {}\n---\n",
            "x".repeat(MAX_FILE_BYTES as usize)
        );
        write(&root.join(".agents/skills/huge/SKILL.md"), &huge);
        fs::create_dir_all(root.join(".agents/skills/binary")).unwrap();
        fs::write(
            root.join(".agents/skills/binary/SKILL.md"),
            [0xff, 0xfe, 0x00],
        )
        .unwrap();
        for i in 0..(MAX_ENTRIES_PER_KIND + 20) {
            write(
                &root.join(format!(".agents/skills/s{i:04}/SKILL.md")),
                &format!("---\nname: s{i:04}\ndescription: d\n---\n"),
            );
        }
        let skills = discover_skills(&root.join(SKILLS_DIR));
        assert_eq!(skills.len(), MAX_ENTRIES_PER_KIND);
        assert!(
            skills
                .iter()
                .all(|s| s.name != "huge" && s.name != "binary")
        );
    }

    #[test]
    fn catalog_is_sorted_deduped_and_builtins_win() {
        let tmp = TempDir::new().unwrap();
        // Nested, so the workspace-parent scan stays inside the tempdir and never reaches the
        // shared temp dir.
        let root = &tmp.path().join("ws/root");
        write(
            &root.join(".agents/skills/code-review/SKILL.md"),
            "---\nname: code-review\ndescription: workspace override\n---\n",
        );
        write(
            &root.join(".agents/skills/zeta/SKILL.md"),
            "---\nname: zeta\ndescription: z\n---\n",
        );
        write(
            &root.join(".fl/flows/code-review.toml"),
            "title = \"Flow\"\n",
        );
        let all = list_commands(root);
        let keys: Vec<(String, String)> = all
            .iter()
            .map(|c| (c.kind.clone(), c.name.clone()))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        let mut unique = keys.clone();
        unique.dedup();
        assert_eq!(keys, unique);
        let review: Vec<_> = all.iter().filter(|c| c.name == "code-review").collect();
        // Same name, different kinds: the skill (built-in wins) and the workflow.
        assert_eq!(review.len(), 2);
        assert_eq!(
            review
                .iter()
                .find(|c| c.kind == "skill")
                .unwrap()
                .description,
            "Subagent-driven code review, finding evaluation, and remediation"
        );
        assert!(all.iter().any(|c| c.name == "zeta" && c.kind == "skill"));
        // Missing directories are fine.
        let empty = TempDir::new().unwrap();
        let empty_root = empty.path().join("ws/root");
        fs::create_dir_all(&empty_root).unwrap();
        assert_eq!(list_commands(&empty_root), {
            let mut b = builtin_commands();
            b.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
            b
        });
    }

    #[test]
    fn frontmatter_survives_a_byte_order_mark_and_windows_line_endings() {
        let doc = "\u{feff}---\r\nname: win\r\ndescription: \"From Windows\"\r\n---\r\nbody";
        let (n, d) = parse_frontmatter(doc).unwrap();
        assert_eq!(n.as_deref(), Some("win"));
        assert_eq!(d.as_deref(), Some("From Windows"));
        // Only a leading mark is skipped; a fence further down is still not frontmatter.
        assert!(parse_frontmatter("\nx\n---\nname: a\n---\n").is_none());
    }

    #[test]
    fn skill_and_flow_names_are_restricted_to_slash_command_characters() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        for (dir, name) in [
            ("ok", "ok-name_1.v2:x"),
            ("space", "has space"),
            ("slash", "a/b"),
            ("newline", "\"a\\nb\""),
            ("long", &"n".repeat(MAX_NAME_CHARS + 1)),
        ] {
            write(
                &root.join(format!(".agents/skills/{dir}/SKILL.md")),
                &format!("---\nname: {name}\ndescription: d\n---\n"),
            );
        }
        let names: Vec<String> = discover_skills(&root.join(SKILLS_DIR))
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["ok-name_1.v2:x"]);
        write(&root.join(".fl/flows/bad name.toml"), "description = 'x'");
        write(&root.join(".fl/flows/good.toml"), "description = 'x'");
        let flows: Vec<String> = discover_flows(&root.join(FLOWS_DIR))
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(flows, vec!["good"]);
    }

    #[test]
    fn descriptions_are_one_line_and_bounded() {
        let tmp = TempDir::new().unwrap();
        let long = "word ".repeat(200);
        write(
            &tmp.path().join(".agents/skills/s/SKILL.md"),
            &format!("---\nname: s\ndescription: >\n  {long}\n\n  second\n---\n"),
        );
        let skills = discover_skills(&tmp.path().join(SKILLS_DIR));
        assert_eq!(skills.len(), 1);
        let d = &skills[0].description;
        assert!(d.chars().count() <= MAX_DESCRIPTION_CHARS);
        assert!(!d.contains('\n') && !d.contains("  "));
    }

    fn skill(path: &Path, name: &str, description: &str) {
        write(
            &path.join("SKILL.md"),
            &format!("---\nname: {name}\ndescription: {description}\n---\n"),
        );
    }

    /// `ws/FerroLoop` (the root), workspace skills, and a separate home with its own skills.
    struct Layout {
        _tmp: TempDir,
        ws: PathBuf,
        root: PathBuf,
        home: PathBuf,
    }

    fn layout() -> Layout {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        let root = ws.join("FerroLoop");
        let home = tmp.path().join("home");
        fs::create_dir_all(&root).unwrap();
        skill(
            &ws.join(".agents/skills/brandkit"),
            "brandkit",
            "Brand kits",
        );
        skill(
            &ws.join(".agents/skills/code-review"),
            "code-review",
            "ws review",
        );
        skill(&ws.join("AI-Lab-AMD/skills/ste"), "ste", "ws ste");
        skill(
            &ws.join("AI-Lab-AMD/skills/training-hygiene"),
            "training-hygiene",
            "Training box facts",
        );
        skill(
            &home.join(".agents/skills/lucida"),
            "lucida",
            "Media generation",
        );
        skill(
            &home.join(".agents/skills/find-skills"),
            "find-skills",
            "Find skills",
        );
        for (dir, target) in [
            (
                ".claude/skills/brandkit",
                ws.join(".agents/skills/brandkit"),
            ),
            (
                ".claude/skills/code-review",
                ws.join(".agents/skills/code-review"),
            ),
            (".claude/skills/ste", ws.join("AI-Lab-AMD/skills/ste")),
            (
                ".claude/skills/training-hygiene",
                ws.join("AI-Lab-AMD/skills/training-hygiene"),
            ),
            (".claude/skills/lucida", home.join(".agents/skills/lucida")),
            (".codex/skills/lucida", home.join(".agents/skills/lucida")),
            (
                ".gemini/config/skills/lucida",
                home.join(".agents/skills/lucida"),
            ),
        ] {
            fs::create_dir_all(home.join(dir).parent().unwrap()).unwrap();
            symlink(target, home.join(dir)).unwrap();
        }
        // Skills that live outside every scanned dir and are reachable only through a link:
        // one from the home into the workspace, one from the workspace into the home.
        skill(
            &ws.join("extras/ws-only"),
            "ws-only",
            "Only via a home link",
        );
        skill(
            &home.join("stash/home-only"),
            "home-only",
            "Only via a workspace link",
        );
        symlink(
            ws.join("extras/ws-only"),
            home.join(".claude/skills/ws-only"),
        )
        .unwrap();
        symlink(
            home.join("stash/home-only"),
            ws.join(".agents/skills/home-only"),
        )
        .unwrap();
        Layout {
            _tmp: tmp,
            ws,
            root,
            home,
        }
    }

    fn skill_names(all: &[CommandEntry]) -> Vec<&str> {
        all.iter()
            .filter(|c| c.kind == "skill")
            .map(|c| c.name.as_str())
            .collect()
    }

    #[test]
    fn workspace_parent_and_home_skills_are_listed_once_through_symlinks() {
        let l = layout();
        let all = list_commands_with_home(&l.root, Some(&l.home));
        let names = skill_names(&all);
        for want in [
            "brandkit",
            "training-hygiene",
            "lucida",
            "find-skills",
            "code-review",
            "ste",
        ] {
            assert_eq!(
                names.iter().filter(|n| **n == want).count(),
                1,
                "{want}: {names:?}"
            );
        }
        // Built-ins still there, and nothing extra: 4 built-in skills + 6 discovered.
        assert_eq!(names.len(), 10, "{names:?}");
        let described = |n: &str| {
            all.iter()
                .find(|c| c.name == n && c.kind == "skill")
                .unwrap()
        };
        assert_eq!(described("lucida").description, "Media generation");
        assert_eq!(
            described("training-hygiene").description,
            "Training box facts"
        );
        // Built-ins win over the workspace copies of the same name.
        assert_ne!(described("ste").description, "ws ste");
        assert_ne!(described("code-review").description, "ws review");

        // Without a home only the workspace-parent roots count.
        let no_home = list_commands(&l.root);
        let names = skill_names(&no_home);
        assert!(names.contains(&"brandkit") && names.contains(&"training-hygiene"));
        assert!(
            !names.contains(&"lucida") && !names.contains(&"find-skills"),
            "{names:?}"
        );
        assert_eq!(list_commands_with_home(&l.root, None), no_home);
    }

    #[test]
    fn home_dirs_alone_still_follow_links_into_the_workspace() {
        let l = layout();
        // A root whose parent is unrelated to the workspace: only the home is trusted.
        let other = l.home.join("elsewhere/proj");
        fs::create_dir_all(&other).unwrap();
        let all = list_commands_with_home(&other, Some(&l.home));
        let names = skill_names(&all);
        // The home root is trusted, so `~/.claude/skills/lucida` (a link into `~/.agents`) works.
        assert!(
            names.contains(&"lucida") && names.contains(&"find-skills"),
            "{names:?}"
        );
        // Links into the workspace are outside the trusted roots here and are rejected.
        assert!(
            !names.contains(&"brandkit") && !names.contains(&"training-hygiene"),
            "{names:?}"
        );
    }

    #[test]
    fn symlinks_leaving_the_trusted_roots_are_rejected() {
        use std::os::unix::fs::symlink;
        let l = layout();
        let outside = TempDir::new().unwrap();
        skill(&outside.path().join("evil"), "evil", "escaped");
        symlink(
            outside.path().join("evil"),
            l.home.join(".claude/skills/evil"),
        )
        .unwrap();
        // A `..` path that leaves both roots.
        let dots = l.home.join(".codex/skills/dots");
        symlink(
            Path::new("../../../../")
                .join(outside.path().strip_prefix("/").unwrap())
                .join("evil"),
            &dots,
        )
        .unwrap();
        // A SKILL.md link out of a skill dir, and a link to a dir without SKILL.md.
        fs::create_dir_all(l.home.join(".claude/skills/linkmd")).unwrap();
        symlink(
            outside.path().join("evil/SKILL.md"),
            l.home.join(".claude/skills/linkmd/SKILL.md"),
        )
        .unwrap();
        fs::create_dir_all(l.home.join(".claude/skills/nomd")).unwrap();
        // A loop, and a dangling link.
        symlink(
            l.home.join(".claude/skills/loop-b"),
            l.home.join(".claude/skills/loop-a"),
        )
        .unwrap();
        symlink(
            l.home.join(".claude/skills/loop-a"),
            l.home.join(".claude/skills/loop-b"),
        )
        .unwrap();
        symlink(
            outside.path().join("missing"),
            l.home.join(".claude/skills/dangling"),
        )
        .unwrap();
        // A link to the filesystem root's /etc.
        symlink("/etc", l.home.join(".claude/skills/etc")).unwrap();

        let all = list_commands_with_home(&l.root, Some(&l.home));
        let names = skill_names(&all);
        assert_eq!(names.len(), 10, "{names:?}");
        assert!(!names.contains(&"evil"), "{names:?}");
        // The escape also fails when the link is the only route to the skill.
        let alone = discover_skills(&l.home.join(".claude/skills"));
        assert!(alone.iter().all(|e| e.name != "evil"));
    }

    #[test]
    fn earlier_roots_win_a_duplicate_name_and_builtins_win_over_all() {
        let l = layout();
        skill(&l.root.join(".agents/skills/lucida"), "lucida", "from root");
        skill(&l.ws.join(".agents/skills/dup"), "dup", "from parent");
        skill(&l.ws.join("AI-Lab-AMD/skills/dup2"), "dup", "from lab");
        skill(&l.home.join(".agents/skills/dup3"), "dup", "from home");
        skill(&l.home.join(".claude/skills/ste"), "ste", "home ste");
        let all = list_commands_with_home(&l.root, Some(&l.home));
        let get = |n: &str| {
            let hits: Vec<_> = all
                .iter()
                .filter(|c| c.name == n && c.kind == "skill")
                .collect();
            assert_eq!(hits.len(), 1, "{n}");
            hits[0].description.clone()
        };
        assert_eq!(get("lucida"), "from root");
        assert_eq!(get("dup"), "from parent");
        assert_ne!(get("ste"), "home ste");
        assert_eq!(
            get("ste"),
            "Rewrite text in ASD-STE100 Simplified Technical English"
        );
    }

    #[test]
    fn skills_listed_in_total_are_capped_in_scan_order() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        let root = ws.join("root");
        let home = tmp.path().join("home");
        fs::create_dir_all(&root).unwrap();
        // 3 dirs of 200 skills exceed the total cap; the root dir is scanned first.
        for (dir, tag) in [
            (root.join(".agents/skills"), "x"),
            (ws.join(".agents/skills"), "y"),
            (home.join(".agents/skills"), "z"),
        ] {
            for i in 0..MAX_ENTRIES_PER_KIND {
                let name = format!("{tag}{i:04}");
                skill(&dir.join(&name), &name, "d");
            }
        }
        let all = list_commands_with_home(&root, Some(&home));
        let count = |tag: char| {
            all.iter()
                .filter(|c| c.kind == "skill" && c.name.starts_with(tag))
                .count()
        };
        assert_eq!(count('x'), MAX_ENTRIES_PER_KIND);
        assert_eq!(count('y'), MAX_ENTRIES_PER_KIND);
        assert_eq!(count('z'), MAX_TOTAL_SKILLS - 2 * MAX_ENTRIES_PER_KIND);
        // Deterministic: the first names of the last dir are the ones kept.
        assert!(all.iter().any(|c| c.name == "z0099"));
        assert!(!all.iter().any(|c| c.name == "z0100"));
        assert_eq!(all, list_commands_with_home(&root, Some(&home)));
    }

    #[test]
    fn a_filesystem_root_parent_is_not_scanned_or_trusted() {
        assert!(trusted_root(Path::new("/")).is_none());
        let roots = skill_roots(Path::new("/"), None);
        assert_eq!(roots.dirs, vec![Path::new("/").join(SKILLS_DIR)]);
        assert!(roots.trusted.is_empty());
    }

    #[test]
    fn home_comes_from_the_override_else_a_nonempty_absolute_env_home() {
        use std::ffi::OsString;
        let over = Path::new("/some/override");
        assert_eq!(
            home_from(Some(over), Some(OsString::from("/env/home"))),
            Some(over.to_path_buf())
        );
        assert_eq!(
            home_from(None, Some(OsString::from("/env/home"))),
            Some(PathBuf::from("/env/home"))
        );
        assert_eq!(home_from(None, Some(OsString::new())), None);
        assert_eq!(home_from(None, Some(OsString::from("relative/home"))), None);
        assert_eq!(home_from(None, None), None);
    }
}
