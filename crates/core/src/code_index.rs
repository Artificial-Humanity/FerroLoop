//! Live structural code index: a deterministic, line-oriented scan of a
//! workspace tree plus the token-savings ledger (`ferrostate-token-gain`).
//!
//! The scan is intentionally shallow — it extracts declaration lines, not
//! an AST — so a query can return a small snippet instead of a whole file.
//! [`CodeSearchOutcome::tokens_saved_this_query`] is computed per distinct
//! file as `max(0, file_tokens - sum(snippet_tokens of returned hits in
//! that file))`, summed over files: two hits in one file never count that
//! file's tokens twice.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::at::At;
use crate::state::{
    MergeMode, RuntimeEntry, SensitivityPatch, StateConcern, StatePayload, StateScope,
    UpsertStateArgs, estimate_tokens,
};
use crate::store::{StateStore, StoreError};

/// Workspace/`Runtime` slug holding the token-gain ledger JSON.
pub const TOKEN_GAIN_SLUG: &str = "ferrostate-token-gain";
/// Files larger than this are skipped by the scan (1 MiB).
pub const MAX_CODE_FILE_BYTES: u64 = 1024 * 1024;

const MAX_SIGNATURE_CHARS: usize = 200;
const MAX_LINE_NAME_CHARS: usize = 200;
const SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules", "dist", ".next"];
const INDEXED_EXTENSIONS: &[&str] = &["rs", "ts", "tsx", "js", "py", "toml", "md"];

/// One structural hit: a declaration (or a raw `line` match) with the
/// token estimate for its file, its snippet, and the difference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSymbolHit {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub signature: String,
    pub snippet: String,
    pub estimated_file_tokens: u64,
    pub snippet_tokens: u64,
    pub tokens_saved: u64,
}

/// Cumulative token-savings ledger persisted at [`TOKEN_GAIN_SLUG`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenGainSummary {
    #[serde(default)]
    pub queries_count: u64,
    #[serde(default)]
    pub tokens_saved: u64,
    #[serde(default)]
    pub files_indexed: u64,
    #[serde(default)]
    pub symbols_indexed: u64,
}

/// Which search a caller asked for: definition lookup, ranked text
/// search, or a per-file outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeSearchKind {
    Def,
    Search,
    Outline,
}

crate::wire::wire_names!(CodeSearchKind as code_search_kind_wire {
    Def => "def",
    Search => "search",
    Outline => "outline",
});
crate::wire::wire_parse!(CodeSearchKind as code_search_kind_parse);

/// Result of one search over a [`CodeIndex`].
///
/// `tokens_saved_this_query` is computed per distinct file:
/// `max(0, file_tokens - sum(snippet_tokens of returned hits in that
/// file))`, summed over files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSearchOutcome {
    pub symbols: Vec<CodeSymbolHit>,
    pub tokens_saved_this_query: u64,
    pub files_indexed: u64,
    pub symbols_indexed: u64,
}

#[derive(Debug, Clone)]
struct IndexedSymbol {
    name: String,
    kind: String,
    start_line: u64,
    end_line: u64,
    signature: String,
}

#[derive(Debug, Clone)]
struct IndexedFile {
    path: String,
    lines: Vec<String>,
    estimated_tokens: u64,
    symbols: Vec<IndexedSymbol>,
}

/// A scanned workspace: the indexed files in relative-path order.
#[derive(Debug, Clone, Default)]
pub struct CodeIndex {
    files: Vec<IndexedFile>,
}

impl CodeIndex {
    pub fn files_indexed(&self) -> u64 {
        self.files.len() as u64
    }

    pub fn symbols_indexed(&self) -> u64 {
        self.files.iter().map(|f| f.symbols.len() as u64).sum()
    }

    /// Run one search. `limit == 0`, or an empty query for `Def`/`Search`,
    /// returns no symbols (the scan counts are still reported).
    pub fn search(&self, kind: CodeSearchKind, q: &str, limit: usize) -> CodeSearchOutcome {
        let mut out = CodeSearchOutcome {
            symbols: Vec::new(),
            tokens_saved_this_query: 0,
            files_indexed: self.files_indexed(),
            symbols_indexed: self.symbols_indexed(),
        };
        if limit == 0 {
            return out;
        }
        let query = q.trim();
        let hits: Vec<CodeSymbolHit> = match kind {
            CodeSearchKind::Def => {
                if query.is_empty() {
                    Vec::new()
                } else {
                    self.search_def(query, limit)
                }
            }
            CodeSearchKind::Search => {
                if query.is_empty() {
                    Vec::new()
                } else {
                    self.search_text(query, limit)
                }
            }
            CodeSearchKind::Outline => self.search_outline(query, limit),
        };
        out.tokens_saved_this_query = tokens_saved_per_file(&hits);
        out.symbols = hits;
        out
    }

    fn search_def(&self, query: &str, limit: usize) -> Vec<CodeSymbolHit> {
        let q = query.to_ascii_lowercase();
        let mut exact: Vec<(&IndexedFile, &IndexedSymbol)> = Vec::new();
        let mut prefix: Vec<(&IndexedFile, &IndexedSymbol)> = Vec::new();
        for file in &self.files {
            for sym in &file.symbols {
                let name = sym.name.to_ascii_lowercase();
                if name == q {
                    exact.push((file, sym));
                } else if name.starts_with(&q) {
                    prefix.push((file, sym));
                }
            }
        }
        let key = |f: &IndexedFile, s: &IndexedSymbol| {
            (
                s.name.to_ascii_lowercase(),
                s.name.clone(),
                f.path.clone(),
                s.start_line,
            )
        };
        exact.sort_by_key(|(f, s)| key(f, s));
        prefix.sort_by_key(|(f, s)| key(f, s));
        exact
            .into_iter()
            .chain(prefix)
            .take(limit)
            .map(|(f, s)| symbol_hit(f, s))
            .collect()
    }

    fn search_text(&self, query: &str, limit: usize) -> Vec<CodeSymbolHit> {
        let terms = split_terms(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let q = query.to_ascii_lowercase();
        let mut ranked: Vec<(u8, &IndexedFile, &IndexedSymbol)> = Vec::new();
        for file in &self.files {
            for sym in &file.symbols {
                let name = sym.name.to_ascii_lowercase();
                let tier = if name == q {
                    Some(0)
                } else if name.starts_with(&q) {
                    Some(1)
                } else if name.contains(&q) {
                    Some(2)
                } else if contains_all_terms(&sym.signature.to_ascii_lowercase(), &terms) {
                    Some(3)
                } else {
                    None
                };
                if let Some(t) = tier {
                    ranked.push((t, file, sym));
                }
            }
        }
        ranked.sort_by(|a, b| {
            (
                a.0,
                a.2.name.to_ascii_lowercase(),
                a.2.name.clone(),
                a.1.path.clone(),
                a.2.start_line,
            )
                .cmp(&(
                    b.0,
                    b.2.name.to_ascii_lowercase(),
                    b.2.name.clone(),
                    b.1.path.clone(),
                    b.2.start_line,
                ))
        });
        let mut hits: Vec<CodeSymbolHit> = ranked
            .into_iter()
            .take(limit)
            .map(|(_, f, s)| symbol_hit(f, s))
            .collect();
        if hits.len() >= limit {
            return hits;
        }
        let returned_decl: BTreeSet<(String, u64)> = hits
            .iter()
            .map(|h| (h.path.clone(), h.start_line))
            .collect();
        for file in &self.files {
            for (i, line) in file.lines.iter().enumerate() {
                if hits.len() >= limit {
                    return hits;
                }
                let line_no = i as u64 + 1;
                if returned_decl.contains(&(file.path.clone(), line_no)) {
                    continue;
                }
                if contains_all_terms(&line.to_ascii_lowercase(), &terms) {
                    hits.push(line_hit(file, line_no, line));
                }
            }
        }
        hits
    }

    fn search_outline(&self, query: &str, limit: usize) -> Vec<CodeSymbolHit> {
        let q = query.to_ascii_lowercase();
        let mut hits = Vec::new();
        for file in &self.files {
            if !file.path.to_ascii_lowercase().contains(&q) {
                continue;
            }
            for sym in &file.symbols {
                if hits.len() >= limit {
                    return hits;
                }
                hits.push(symbol_hit(file, sym));
            }
        }
        hits
    }
}

/// Scan `root` and run one search over the result.
pub fn search_code_index(
    root: &Path,
    kind: CodeSearchKind,
    q: &str,
    limit: usize,
) -> CodeSearchOutcome {
    scan_code_index(root).search(kind, q, limit)
}

fn tokens_saved_per_file(hits: &[CodeSymbolHit]) -> u64 {
    let mut per_file: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for hit in hits {
        let e = per_file
            .entry(hit.path.as_str())
            .or_insert((hit.estimated_file_tokens, 0));
        e.1 = e.1.saturating_add(hit.snippet_tokens);
    }
    per_file.values().fold(0u64, |acc, (file_tokens, snippet)| {
        acc.saturating_add(file_tokens.saturating_sub(*snippet))
    })
}

fn split_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut cur = String::new();
    for ch in query.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            cur.push(ch.to_ascii_lowercase());
        } else if !cur.is_empty() {
            terms.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        terms.push(cur);
    }
    terms
}

fn contains_all_terms(haystack_lower: &str, terms: &[String]) -> bool {
    terms.iter().all(|t| haystack_lower.contains(t.as_str()))
}

fn cap_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn symbol_hit(file: &IndexedFile, sym: &IndexedSymbol) -> CodeSymbolHit {
    let start = sym.start_line.max(1) as usize;
    let last = (sym.end_line.max(sym.start_line) as usize).min(start + 15);
    let snippet = if start <= file.lines.len() {
        file.lines[start - 1..last.min(file.lines.len())].join("\n")
    } else {
        String::new()
    };
    let snippet_tokens = estimate_tokens(&snippet);
    CodeSymbolHit {
        name: sym.name.clone(),
        kind: sym.kind.clone(),
        path: file.path.clone(),
        start_line: sym.start_line,
        end_line: sym.end_line,
        signature: sym.signature.clone(),
        snippet,
        estimated_file_tokens: file.estimated_tokens,
        snippet_tokens,
        tokens_saved: file.estimated_tokens.saturating_sub(snippet_tokens),
    }
}

fn line_hit(file: &IndexedFile, line_no: u64, line: &str) -> CodeSymbolHit {
    let trimmed = line.trim();
    let snippet_tokens = estimate_tokens(line);
    CodeSymbolHit {
        name: cap_chars(trimmed, MAX_LINE_NAME_CHARS),
        kind: "line".to_string(),
        path: file.path.clone(),
        start_line: line_no,
        end_line: line_no,
        signature: cap_chars(trimmed, MAX_SIGNATURE_CHARS),
        snippet: line.to_string(),
        estimated_file_tokens: file.estimated_tokens,
        snippet_tokens,
        tokens_saved: file.estimated_tokens.saturating_sub(snippet_tokens),
    }
}

/// Deterministically scan `root` (sorted walk, no symlink following).
/// Directories named `.git`/`target`/`node_modules`/`dist`/`.next` are
/// skipped at any depth; non-UTF-8 files and files over
/// [`MAX_CODE_FILE_BYTES`] are skipped. A missing root yields an empty index.
pub fn scan_code_index(root: &Path) -> CodeIndex {
    let mut paths: Vec<PathBuf> = Vec::new();
    match fs::symlink_metadata(root) {
        Ok(md) if md.file_type().is_symlink() => return CodeIndex::default(),
        Ok(md) if md.is_file() => {
            if has_indexed_extension(root) {
                paths.push(root.to_path_buf());
            }
        }
        Ok(_) => collect_files(root, &mut paths),
        Err(_) => return CodeIndex::default(),
    }
    paths.sort();
    let root_is_file = paths.len() == 1 && root.is_file();
    let mut files = Vec::new();
    for path in paths {
        let rel = if root_is_file {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            relative_slash_path(root, &path)
        };
        if rel.is_empty() {
            continue;
        }
        if let Some(file) = index_file(&path, rel) {
            files.push(file);
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    CodeIndex { files }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            let name = entry.file_name();
            if SKIPPED_DIRS.iter().any(|d| name.to_string_lossy() == *d) {
                continue;
            }
            collect_files(&path, out);
        } else if ft.is_file() && has_indexed_extension(&path) {
            out.push(path);
        }
    }
}

fn has_indexed_extension(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| INDEXED_EXTENSIONS.contains(&e.as_str()))
}

fn extension_lower(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn relative_slash_path(root: &Path, path: &Path) -> String {
    let Ok(rel) = path.strip_prefix(root) else {
        return String::new();
    };
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn index_file(path: &Path, rel: String) -> Option<IndexedFile> {
    let md = fs::symlink_metadata(path).ok()?;
    if md.file_type().is_symlink() || md.len() > MAX_CODE_FILE_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    if bytes.len() as u64 > MAX_CODE_FILE_BYTES {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    let estimated_tokens = estimate_tokens(&text);
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let ext = extension_lower(path);
    let symbols = extract_symbols(&ext, &lines);
    Some(IndexedFile {
        path: rel,
        lines,
        estimated_tokens,
        symbols,
    })
}

fn extract_symbols(ext: &str, lines: &[String]) -> Vec<IndexedSymbol> {
    let mut raw: Vec<(String, String, u64, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some((name, kind)) = parse_declaration(ext, line) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        raw.push((
            name,
            kind,
            i as u64 + 1,
            cap_chars(line.trim(), MAX_SIGNATURE_CHARS),
        ));
    }
    let last_line = lines.len() as u64;
    let mut symbols = Vec::with_capacity(raw.len());
    for (idx, (name, kind, start, signature)) in raw.iter().enumerate() {
        let end = match raw.get(idx + 1) {
            Some(next) => next.2.saturating_sub(1).max(*start),
            None => last_line.max(*start),
        };
        symbols.push(IndexedSymbol {
            name: name.clone(),
            kind: kind.clone(),
            start_line: *start,
            end_line: end,
            signature: signature.clone(),
        });
    }
    symbols
}

fn parse_declaration(ext: &str, line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    match ext {
        "rs" => {
            if trimmed.starts_with("//") || trimmed.starts_with("#[") {
                return None;
            }
            parse_rust(trimmed)
        }
        "ts" | "tsx" | "js" => {
            if trimmed.starts_with("//") {
                return None;
            }
            parse_js(trimmed)
        }
        "py" => {
            if trimmed.starts_with('#') {
                return None;
            }
            parse_python(trimmed)
        }
        "toml" => {
            if trimmed.starts_with('#') {
                return None;
            }
            parse_toml(trimmed)
        }
        "md" => parse_markdown(trimmed),
        _ => None,
    }
}

/// First identifier (`[A-Za-z_$][A-Za-z0-9_$]*`) at the start of `s`,
/// plus the remainder after it.
fn take_ident(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    let mut end = 0;
    for (i, ch) in s.char_indices() {
        let ok = if i == 0 {
            ch.is_ascii_alphabetic() || ch == '_' || ch == '$'
        } else {
            ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
        };
        if !ok {
            break;
        }
        end = i + ch.len_utf8();
    }
    if end == 0 {
        None
    } else {
        Some((&s[..end], &s[end..]))
    }
}

fn parse_rust(trimmed: &str) -> Option<(String, String)> {
    let mut s = trimmed;
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix("pub(") {
            let close = rest.find(')')?;
            s = &rest[close + 1..];
            continue;
        }
        let (word, rest) = take_ident(t)?;
        match word {
            "pub" | "async" | "unsafe" | "default" => s = rest,
            "const" => match take_ident(rest) {
                // `const fn` is a prefix; `const NAME` is a declaration.
                Some(("fn", _)) => s = rest,
                _ => break,
            },
            _ => break,
        }
        s = s.trim_start();
        if s.is_empty() {
            return None;
        }
    }
    let (word, rest) = take_ident(s)?;
    match word {
        "fn" | "struct" | "enum" | "trait" | "type" | "const" | "mod" => {
            let (name, _) = take_ident(rest.trim_start().trim_start_matches('*'))?;
            Some((name.to_string(), word.to_string()))
        }
        "impl" => Some((rust_impl_name(rest)?, "impl".to_string())),
        _ => None,
    }
}

fn rust_impl_name(rest: &str) -> Option<String> {
    let mut s = rest.trim_start();
    if s.starts_with('<') {
        let mut depth = 0usize;
        let mut end = None;
        for (i, ch) in s.char_indices() {
            match ch {
                '<' => depth += 1,
                '>' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        s = s[end?..].trim_start();
    }
    // Type after a whole-word `for`, else the first type expression.
    let segment = {
        let mut found = None;
        let mut search = s;
        let mut offset = 0usize;
        while let Some(pos) = search.find("for") {
            let before_ok = pos == 0 || !is_ident_byte(search.as_bytes()[pos - 1]);
            let after = &search[pos + 3..];
            let after_ok =
                after.is_empty() || !after.as_bytes().first().is_some_and(|b| is_ident_byte(*b));
            if before_ok && after_ok {
                found = Some(offset + pos + 3);
                break;
            }
            offset += pos + 3;
            search = after;
        }
        match found {
            Some(idx) => s[idx..].trim_start(),
            None => s,
        }
    };
    let segment = segment.split(['{', ';']).next().unwrap_or(segment);
    let segment = segment.split(" where ").next().unwrap_or(segment).trim();
    let segment = segment.split('<').next().unwrap_or(segment).trim();
    let segment = segment.rsplit("::").next().unwrap_or(segment).trim();
    let segment = segment.trim_start_matches(['&', ' ']);
    let segment = segment
        .strip_prefix("dyn ")
        .map_or(segment, str::trim_start);
    let (name, _) = take_ident(segment)?;
    Some(name.to_string())
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn parse_js(trimmed: &str) -> Option<(String, String)> {
    let mut s = trimmed;
    loop {
        let (word, rest) = take_ident(s)?;
        match word {
            "export" | "default" | "abstract" | "async" | "declare" => {
                s = rest.trim_start();
                if s.is_empty() {
                    return None;
                }
            }
            _ => break,
        }
    }
    let (word, rest) = take_ident(s)?;
    match word {
        "function" => {
            let (name, _) = take_ident(rest.trim_start().trim_start_matches('*'))?;
            Some((name.to_string(), "function".to_string()))
        }
        "class" | "interface" | "type" | "enum" | "const" | "let" | "var" => {
            let (name, _) = take_ident(rest)?;
            Some((name.to_string(), word.to_string()))
        }
        _ => None,
    }
}

fn parse_python(trimmed: &str) -> Option<(String, String)> {
    if let Some(rest) = trimmed.strip_prefix("async def ") {
        let (name, _) = take_ident(rest)?;
        return Some((name.to_string(), "def".to_string()));
    }
    if let Some(rest) = trimmed.strip_prefix("def ") {
        let (name, _) = take_ident(rest)?;
        return Some((name.to_string(), "def".to_string()));
    }
    if let Some(rest) = trimmed.strip_prefix("class ") {
        let (name, _) = take_ident(rest)?;
        return Some((name.to_string(), "class".to_string()));
    }
    None
}

fn parse_toml(trimmed: &str) -> Option<(String, String)> {
    let inner = if let Some(r) = trimmed.strip_prefix("[[") {
        r.strip_suffix("]]")?
    } else {
        let r = trimmed.strip_prefix('[')?;
        r.strip_suffix(']')?
    };
    let name = inner.trim();
    if name.is_empty() {
        None
    } else {
        Some((name.to_string(), "section".to_string()))
    }
}

fn parse_markdown(trimmed: &str) -> Option<(String, String)> {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim();
    if text.is_empty() {
        None
    } else {
        Some((text.to_string(), "heading".to_string()))
    }
}

/// Load the token-gain ledger; missing or malformed state reads as default.
pub fn load_token_gain(store: &impl StateStore) -> TokenGainSummary {
    let Ok(Some(entry)) = store.get_state_by_key(
        &StateScope::Workspace,
        StateConcern::Runtime,
        TOKEN_GAIN_SLUG,
    ) else {
        return TokenGainSummary::default();
    };
    match entry.payload {
        StatePayload::Runtime(rt) => serde_json::from_value(rt.value).unwrap_or_default(),
        _ => TokenGainSummary::default(),
    }
}

fn save_token_gain(
    store: &impl StateStore,
    summary: TokenGainSummary,
    at: At,
    by: &str,
) -> Result<TokenGainSummary, StoreError> {
    let value = serde_json::json!({
        "queries_count": summary.queries_count,
        "tokens_saved": summary.tokens_saved,
        "files_indexed": summary.files_indexed,
        "symbols_indexed": summary.symbols_indexed,
    });
    store.upsert_state(UpsertStateArgs {
        scope: StateScope::Workspace,
        slug: TOKEN_GAIN_SLUG.to_string(),
        sensitivity: SensitivityPatch::Inherit,
        payload: StatePayload::Runtime(RuntimeEntry {
            value,
            merge_mode: MergeMode::LastWriter,
        }),
        at,
        by: by.to_string(),
    })?;
    Ok(summary)
}

/// Record a completed scan: refresh the latest file/symbol counts only;
/// `queries_count` and `tokens_saved` are untouched.
pub fn record_token_gain_index(
    store: &impl StateStore,
    files_indexed: u64,
    symbols_indexed: u64,
    at: At,
    by: &str,
) -> Result<TokenGainSummary, StoreError> {
    let mut summary = load_token_gain(store);
    summary.files_indexed = files_indexed;
    summary.symbols_indexed = symbols_indexed;
    save_token_gain(store, summary, at, by)
}

/// Record one query: increment `queries_count` by 1, add
/// `saved_this_query` to `tokens_saved` (saturating), and refresh the
/// latest file/symbol counts.
pub fn record_token_gain_query(
    store: &impl StateStore,
    saved_this_query: u64,
    files_indexed: u64,
    symbols_indexed: u64,
    at: At,
    by: &str,
) -> Result<TokenGainSummary, StoreError> {
    let mut summary = load_token_gain(store);
    summary.queries_count = summary.queries_count.saturating_add(1);
    summary.tokens_saved = summary.tokens_saved.saturating_add(saved_this_query);
    summary.files_indexed = files_indexed;
    summary.symbols_indexed = symbols_indexed;
    save_token_gain(store, summary, at, by)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemStore;
    use tempfile::TempDir;

    fn at() -> At {
        At::from_unix_millis(1_727_658_123_004)
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn syms<'a>(index: &'a CodeIndex, path: &str) -> Vec<(&'a str, &'a str, u64, u64)> {
        let file = index
            .files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| {
                panic!(
                    "no indexed file `{path}`: {:?}",
                    index.files.iter().map(|f| &f.path).collect::<Vec<_>>()
                )
            });
        file.symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind.as_str(), s.start_line, s.end_line))
            .collect()
    }

    #[test]
    fn extracts_symbols_across_languages_with_lines_and_kinds() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            root,
            "src/lib.rs",
            "// fn ignored() {}\npub fn alpha() {}\npub struct Widget {\n    pub x: u32,\n}\nimpl std::fmt::Display for Widget {\n}\n",
        );
        write(
            root,
            "src/app.ts",
            "export function buildWidget() {}\nexport interface WidgetProps { title: string }\n",
        );
        write(
            root,
            "tools/make.py",
            "def make_widget():\n    return 1\n\nclass WidgetFactory:\n    pass\n",
        );
        write(root, "Cargo.toml", "[package]\nname = \"x\"\n");
        write(
            root,
            "README.md",
            "# Title Here\n\ntext\n## Sub Section\nmore\n",
        );
        write(root, "target/hidden.rs", "pub fn hidden_target() {}\n");
        write(
            root,
            "node_modules/pkg/index.js",
            "function hidden_nm() {}\n",
        );
        fs::write(root.join("src/bad.rs"), [0xff, 0xfe, b'x']).unwrap();
        write(
            root,
            "src/big.rs",
            &"x".repeat(MAX_CODE_FILE_BYTES as usize + 1),
        );

        let index = scan_code_index(root);
        assert_eq!(
            index.files_indexed(),
            5,
            "{:?}",
            index.files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        assert_eq!(
            syms(&index, "src/lib.rs"),
            vec![
                ("alpha", "fn", 2, 2),
                ("Widget", "struct", 3, 5),
                ("Widget", "impl", 6, 7)
            ]
        );
        assert_eq!(
            syms(&index, "src/app.ts"),
            vec![
                ("buildWidget", "function", 1, 1),
                ("WidgetProps", "interface", 2, 2)
            ]
        );
        assert_eq!(
            syms(&index, "tools/make.py"),
            vec![
                ("make_widget", "def", 1, 3),
                ("WidgetFactory", "class", 4, 5)
            ]
        );
        assert_eq!(
            syms(&index, "Cargo.toml"),
            vec![("package", "section", 1, 2)]
        );
        assert_eq!(
            syms(&index, "README.md"),
            vec![
                ("Title Here", "heading", 1, 3),
                ("Sub Section", "heading", 4, 5)
            ]
        );
        let all_names: Vec<&str> = index
            .files
            .iter()
            .flat_map(|f| f.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert!(!all_names.contains(&"hidden_target"));
        assert!(!all_names.contains(&"hidden_nm"));
        assert!(!all_names.contains(&"ignored"));
    }

    #[test]
    fn def_is_exact_before_prefix_and_never_substring() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "a.rs",
            "pub fn runner() {}\npub fn run() {}\npub fn outrun() {}\n",
        );
        let index = scan_code_index(tmp.path());
        let out = index.search(CodeSearchKind::Def, "RUN", 10);
        let names: Vec<&str> = out.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["run", "runner"]);
        assert_eq!(out.files_indexed, 1);
        assert_eq!(out.symbols_indexed, 3);
        assert!(
            index
                .search(CodeSearchKind::Def, "run", 0)
                .symbols
                .is_empty()
        );
        assert!(
            index
                .search(CodeSearchKind::Def, "  ", 10)
                .symbols
                .is_empty()
        );
    }

    #[test]
    fn search_ranks_tiers_adds_line_hits_and_dedupes_decl_lines() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "a.rs",
            "pub fn hay_needle_stack() {}\npub fn needlework() {}\npub fn needle() {}\npub fn other(needle_arg: u32) {}\npub fn holder() {\n    let needle_local = 1;\n}\n",
        );
        let index = scan_code_index(tmp.path());
        let out = index.search(CodeSearchKind::Search, "needle", 20);
        let names_kinds: Vec<(&str, &str)> = out
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind.as_str()))
            .collect();
        assert_eq!(names_kinds[0], ("needle", "fn"));
        assert_eq!(names_kinds[1], ("needlework", "fn"));
        assert_eq!(names_kinds[2], ("hay_needle_stack", "fn"));
        assert_eq!(names_kinds[3], ("other", "fn"));
        let line_hits: Vec<&CodeSymbolHit> =
            out.symbols.iter().filter(|s| s.kind == "line").collect();
        assert_eq!(line_hits.len(), 1, "{names_kinds:?}");
        assert_eq!(line_hits[0].start_line, 6);
        assert_eq!(line_hits[0].end_line, 6);
        assert!(line_hits[0].name.contains("needle_local"));
        assert_eq!(out.symbols.iter().filter(|s| s.start_line == 3).count(), 1);
    }

    #[test]
    fn outline_matches_by_path_suffix_in_start_order() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "src/deep/widget.rs",
            "pub fn second() {}\npub fn first_declared_later() {}\n",
        );
        write(tmp.path(), "src/other.rs", "pub fn elsewhere() {}\n");
        let index = scan_code_index(tmp.path());
        let out = index.search(CodeSearchKind::Outline, "deep/widget.rs", 20);
        let got: Vec<(&str, &str, u64)> = out
            .symbols
            .iter()
            .map(|s| (s.path.as_str(), s.name.as_str(), s.start_line))
            .collect();
        assert_eq!(
            got,
            vec![
                ("src/deep/widget.rs", "second", 1),
                ("src/deep/widget.rs", "first_declared_later", 2)
            ]
        );
    }

    #[test]
    fn tokens_saved_this_query_is_per_distinct_file() {
        let tmp = TempDir::new().unwrap();
        let mut text = String::from("pub fn alpha() {\n");
        for i in 0..30 {
            text.push_str(&format!("    let pad_a_{i} = {i};\n"));
        }
        text.push_str("}\npub fn beta() {\n");
        for i in 0..30 {
            text.push_str(&format!("    let pad_b_{i} = {i};\n"));
        }
        text.push_str("}\n");
        write(tmp.path(), "big.rs", &text);
        let out = search_code_index(tmp.path(), CodeSearchKind::Outline, "big.rs", 10);
        assert_eq!(out.symbols.len(), 2);
        let file_tokens = out.symbols[0].estimated_file_tokens;
        let snippet_sum: u64 = out.symbols.iter().map(|s| s.snippet_tokens).sum();
        assert_eq!(
            out.tokens_saved_this_query,
            file_tokens.saturating_sub(snippet_sum)
        );
        let naive_per_hit_sum: u64 = out.symbols.iter().map(|s| s.tokens_saved).sum();
        assert!(out.tokens_saved_this_query < naive_per_hit_sum);
        assert!(out.tokens_saved_this_query > 0);
    }

    #[test]
    fn token_gain_ledger_records_index_then_queries() {
        let store = MemStore::default();
        assert_eq!(load_token_gain(&store), TokenGainSummary::default());

        let after_index = record_token_gain_index(&store, 3, 10, at(), "tester").unwrap();
        assert_eq!(
            after_index,
            TokenGainSummary {
                queries_count: 0,
                tokens_saved: 0,
                files_indexed: 3,
                symbols_indexed: 10
            }
        );
        let after_q1 = record_token_gain_query(&store, 100, 3, 10, at(), "tester").unwrap();
        assert_eq!(after_q1.queries_count, 1);
        assert_eq!(after_q1.tokens_saved, 100);
        let after_q2 = record_token_gain_query(&store, 50, 4, 12, at(), "tester").unwrap();
        assert_eq!(
            after_q2,
            TokenGainSummary {
                queries_count: 2,
                tokens_saved: 150,
                files_indexed: 4,
                symbols_indexed: 12
            }
        );
        assert_eq!(load_token_gain(&store), after_q2);

        let entry = store
            .get_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                TOKEN_GAIN_SLUG,
            )
            .unwrap()
            .expect("ledger entry persisted");
        assert_eq!(entry.slug, "ferrostate-token-gain");
        match entry.payload {
            StatePayload::Runtime(rt) => {
                assert_eq!(rt.merge_mode, MergeMode::LastWriter);
                assert_eq!(rt.value["queries_count"], 2);
                assert_eq!(rt.value["tokens_saved"], 150);
                assert_eq!(rt.value["files_indexed"], 4);
                assert_eq!(rt.value["symbols_indexed"], 12);
            }
            other => panic!("expected Runtime payload, got {other:?}"),
        }
    }
}
