//! Slash-command catalog for `GET /api/v1/commands`: built-in commands, workspace skills
//! (`.agents/skills/*/SKILL.md`) and workflows (`.fl/flows/*.toml`).
//!
//! Discovery is read-only and bounded: a directory yields at most [`MAX_ENTRIES_PER_KIND`]
//! entries, a file is read up to [`MAX_FILE_BYTES`] bytes, and nothing a symlink points to
//! outside the directory being scanned is read.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Skills dir, relative to the workspace root.
pub const SKILLS_DIR: &str = ".agents/skills";
/// Workflow manifests dir, relative to the workspace root.
pub const FLOWS_DIR: &str = ".fl/flows";
/// Most skills, and most workflows, one response lists.
pub const MAX_ENTRIES_PER_KIND: usize = 200;
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

/// The full catalog for the workspace at `root`: built-ins, skills and workflows, sorted by
/// `kind` then `name`, one entry per `(kind, name)`. A built-in wins over a discovered entry
/// of the same kind and name.
pub fn list_commands(root: &Path) -> Vec<CommandEntry> {
    let mut all = builtin_commands();
    all.extend(discover_skills(&root.join(SKILLS_DIR)));
    all.extend(discover_flows(&root.join(FLOWS_DIR)));
    // Stable sort keeps the built-in ahead of a discovered duplicate, so dedup keeps it.
    all.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
    all.dedup_by(|b, a| a.kind == b.kind && a.name == b.name);
    all
}

/// Canonical paths of the entries of `dir` in name order. An entry that is a symlink leaving
/// `dir` is dropped, and so is a second entry that resolves to an already listed path.
fn contained_entries(dir: &Path) -> Vec<PathBuf> {
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
        if canonical.starts_with(&root) && !seen.contains(&canonical) {
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
/// or that resolves outside `dir` is skipped.
pub fn discover_skills(dir: &Path) -> Vec<CommandEntry> {
    let Ok(root) = fs::canonicalize(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for skill_dir in contained_entries(dir) {
        if out.len() >= MAX_ENTRIES_PER_KIND {
            break;
        }
        if !skill_dir.is_dir() {
            continue;
        }
        let Ok(skill_md) = fs::canonicalize(skill_dir.join("SKILL.md")) else {
            continue;
        };
        if !skill_md.starts_with(&root) || !skill_md.is_file() {
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
        out.push(CommandEntry {
            name,
            kind: "skill".to_string(),
            description: one_line(&description.unwrap_or_default()),
        });
    }
    out
}

/// Workflows under `dir`: one per `*.toml` file. The name is the file stem; the description is
/// the file's `description`, else its `title`, else a generic line. A file that is unreadable,
/// is not valid TOML, has an invalid stem, or resolves outside `dir` is skipped.
pub fn discover_flows(dir: &Path) -> Vec<CommandEntry> {
    let Ok(root) = fs::canonicalize(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for path in contained_entries(dir) {
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
        let root = tmp.path();
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
        assert_eq!(list_commands(empty.path()), {
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
}
