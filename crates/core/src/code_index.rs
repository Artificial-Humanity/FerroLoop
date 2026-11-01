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
use std::io::Read;
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
    /// Full (uncapped) declaration line, for search tier matching.
    declaration: String,
}

#[derive(Debug, Clone)]
struct IndexedFile {
    path: String,
    lines: Vec<String>,
    estimated_tokens: u64,
    symbols: Vec<IndexedSymbol>,
}

/// A read-only view of one indexed file.
#[derive(Debug, Clone, Copy)]
pub struct CodeFileRef<'a> {
    file: &'a IndexedFile,
}

impl<'a> CodeFileRef<'a> {
    /// Path relative to the scanned root, `/`-separated.
    pub fn path(&self) -> &'a str {
        &self.file.path
    }

    /// Lower-case hex SHA-256 of the file's lines, each followed by `\n`.
    /// It changes exactly when the indexed text changes.
    pub fn content_digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for line in &self.file.lines {
            hasher.update(line.as_bytes());
            hasher.update(b"\n");
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The declarations extracted from the file, in source order.
    pub fn symbols(&self) -> impl Iterator<Item = CodeSymbolRef<'a>> + use<'a> {
        let file = self.file;
        file.symbols
            .iter()
            .map(move |sym| CodeSymbolRef { file, sym })
    }
}

/// A read-only view of one declaration.
#[derive(Debug, Clone, Copy)]
pub struct CodeSymbolRef<'a> {
    file: &'a IndexedFile,
    sym: &'a IndexedSymbol,
}

impl<'a> CodeSymbolRef<'a> {
    pub fn name(&self) -> &'a str {
        &self.sym.name
    }

    pub fn kind(&self) -> &'a str {
        &self.sym.kind
    }

    pub fn path(&self) -> &'a str {
        &self.file.path
    }

    pub fn start_line(&self) -> u64 {
        self.sym.start_line
    }

    pub fn signature(&self) -> &'a str {
        &self.sym.signature
    }

    /// The same hit a search returns for this declaration: snippet and token
    /// estimates included.
    pub fn hit(&self) -> CodeSymbolHit {
        symbol_hit(self.file, self.sym)
    }
}

/// A scanned workspace: the indexed files in relative-path order.
#[derive(Debug, Clone, Default)]
pub struct CodeIndex {
    files: Vec<IndexedFile>,
    /// The scanned root, canonicalized when that works and as given when it
    /// does not. `None` only for an index that was not made by a scan.
    root: Option<PathBuf>,
}

impl CodeIndex {
    /// The root this index was scanned from: what a ranker that keeps derived
    /// data per index keys it by, so that two roots never share it. The path is
    /// canonical when the filesystem could canonicalize it, otherwise as given.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub fn files_indexed(&self) -> u64 {
        self.files.len() as u64
    }

    pub fn symbols_indexed(&self) -> u64 {
        self.files.iter().map(|f| f.symbols.len() as u64).sum()
    }

    /// Run one search. `limit == 0`, or an empty query for `Def`/`Search`,
    /// returns no symbols (the scan counts are still reported).
    pub fn search(&self, kind: CodeSearchKind, q: &str, limit: usize) -> CodeSearchOutcome {
        let (files, symbols) = (self.files_indexed(), self.symbols_indexed());
        if limit == 0 {
            return outcome_from_hits(Vec::new(), files, symbols);
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
        outcome_from_hits(hits, files, symbols)
    }

    /// The indexed files in relative-path order, for a ranker that keeps
    /// derived data per file.
    pub fn files(&self) -> impl Iterator<Item = CodeFileRef<'_>> {
        self.files.iter().map(|file| CodeFileRef { file })
    }

    fn search_def(&self, query: &str, limit: usize) -> Vec<CodeSymbolHit> {
        let q = query.to_lowercase();
        let mut exact: Vec<(&IndexedFile, &IndexedSymbol)> = Vec::new();
        let mut prefix: Vec<(&IndexedFile, &IndexedSymbol)> = Vec::new();
        for file in &self.files {
            for sym in &file.symbols {
                let name = sym.name.to_lowercase();
                if name == q {
                    exact.push((file, sym));
                } else if name.starts_with(&q) {
                    prefix.push((file, sym));
                }
            }
        }
        let key = |f: &IndexedFile, s: &IndexedSymbol| {
            (
                s.name.to_lowercase(),
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
        let q = query.to_lowercase();
        let mut ranked: Vec<(u8, &IndexedFile, &IndexedSymbol)> = Vec::new();
        for file in &self.files {
            for sym in &file.symbols {
                let name = sym.name.to_lowercase();
                let tier = if name == q {
                    Some(0)
                } else if name.starts_with(&q) {
                    Some(1)
                } else if name.contains(&q) {
                    Some(2)
                } else if contains_all_terms(&sym.declaration.to_lowercase(), &terms) {
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
                a.2.name.to_lowercase(),
                a.2.name.clone(),
                a.1.path.clone(),
                a.2.start_line,
            )
                .cmp(&(
                    b.0,
                    b.2.name.to_lowercase(),
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
                if contains_all_terms(&line.to_lowercase(), &terms) {
                    hits.push(line_hit(file, line_no, line));
                }
            }
        }
        hits
    }

    fn search_outline(&self, query: &str, limit: usize) -> Vec<CodeSymbolHit> {
        let q = query.to_lowercase();
        let mut hits = Vec::new();
        for file in &self.files {
            if !file.path.to_lowercase().contains(&q) {
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

/// The outcome of a search that returned `hits`: `tokens_saved_this_query`
/// is computed per distinct file from those hits, so every ranker (the
/// built-in tiers or an external one) books token gain by the same rule.
pub fn outcome_from_hits(
    hits: Vec<CodeSymbolHit>,
    files_indexed: u64,
    symbols_indexed: u64,
) -> CodeSearchOutcome {
    CodeSearchOutcome {
        tokens_saved_this_query: tokens_saved_per_file(&hits),
        symbols: hits,
        files_indexed,
        symbols_indexed,
    }
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
        if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            terms.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        terms.push(cur);
    }
    // A punctuation-only query (e.g. `->`) has no word terms; fall back to
    // the whole query as one term so declaration/line matching can hit it.
    if terms.is_empty() {
        let whole = query.trim().to_lowercase();
        if !whole.is_empty() {
            terms.push(whole);
        }
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
    let identity = Some(fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()));
    let empty = || CodeIndex {
        files: Vec::new(),
        root: identity.clone(),
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    match fs::symlink_metadata(root) {
        Ok(md) if md.file_type().is_symlink() => return empty(),
        Ok(md) if md.is_file() => {
            if has_indexed_extension(root) {
                paths.push(root.to_path_buf());
            }
        }
        Ok(_) => collect_files(root, &mut paths),
        Err(_) => return empty(),
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
    CodeIndex {
        files,
        root: identity,
    }
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
    // Bounded read off the opened handle: never slurp past cap+1, so a file
    // that grows between the size pre-check and the read cannot blow up
    // memory; one byte over the cap is enough to reject it.
    let mut file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_CODE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
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
    let mut raw: Vec<(String, String, u64, String, String)> = Vec::new();
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
            line.trim().to_string(),
        ));
    }
    let last_line = lines.len() as u64;
    let mut symbols = Vec::with_capacity(raw.len());
    for (idx, (name, kind, start, signature, declaration)) in raw.iter().enumerate() {
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
            declaration: declaration.clone(),
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
            parse_js(line)
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

/// First identifier at the start of `s`, plus the remainder after it.
/// A Unicode alphabetic (or `_`/`$`) starts it; alphanumerics/`_`/`$`
/// continue it. A Rust raw prefix `r#` is stripped from the returned name.
fn take_ident(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    let s = match s.strip_prefix("r#") {
        Some(body)
            if body
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$') =>
        {
            body
        }
        _ => s,
    };
    let mut end = 0;
    for (i, ch) in s.char_indices() {
        let ok = if i == 0 {
            ch.is_alphabetic() || ch == '_' || ch == '$'
        } else {
            ch.is_alphanumeric() || ch == '_' || ch == '$'
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
            // `extern "C" fn` / `extern fn`: skip the optional ABI string.
            "extern" => {
                let r = rest.trim_start();
                if let Some(after_quote) = r.strip_prefix('"') {
                    let close = after_quote.find('"')?;
                    s = &after_quote[close + 1..];
                } else {
                    s = rest;
                }
            }
            "const" => match take_ident(rest) {
                // `const fn` is a prefix; `const NAME` is a declaration.
                Some(("fn" | "async" | "unsafe" | "extern", _)) => s = rest,
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
        // Skip the generic parameter list. `->` inside a bound (e.g.
        // `T: Fn() -> T`) must not close angle depth.
        let mut depth = 0usize;
        let mut end = None;
        let mut chars = s.char_indices().peekable();
        while let Some((i, ch)) = chars.next() {
            match ch {
                '<' => depth += 1,
                '-' if chars.peek().is_some_and(|(_, c)| *c == '>') => {
                    chars.next();
                }
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
            let before_ok = search[..pos]
                .chars()
                .next_back()
                .is_none_or(|c| !is_ident_char(c));
            let after = &search[pos + 3..];
            let after_ok = after.chars().next().is_none_or(|c| !is_ident_char(c));
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

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn parse_js(line: &str) -> Option<(String, String)> {
    // `const`/`let`/`var` count only at zero indentation or when exported,
    // so locals inside a function body do not pollute the outline.
    let top_level = line.chars().next().is_none_or(|c| !c.is_whitespace());
    let mut exported = false;
    let mut s = line.trim_start();
    loop {
        let (word, rest) = take_ident(s)?;
        match word {
            "export" | "default" | "abstract" | "async" | "declare" => {
                exported |= word == "export";
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
        "class" | "interface" | "type" | "enum" => {
            let (name, _) = take_ident(rest)?;
            Some((name.to_string(), word.to_string()))
        }
        "const" | "let" | "var" if top_level || exported => {
            let (name, _) = take_ident(rest)?;
            Some((name.to_string(), word.to_string()))
        }
        _ => None,
    }
}

fn parse_python(trimmed: &str) -> Option<(String, String)> {
    // Token-based: `async  def` / `def\tname` are accepted, not just the
    // single-space spelling.
    let (word, rest) = take_ident(trimmed)?;
    let (kind, rest) = match word {
        "def" => ("def", rest),
        "class" => ("class", rest),
        "async" => {
            let (next, rest) = take_ident(rest)?;
            if next != "def" {
                return None;
            }
            ("def", rest)
        }
        _ => return None,
    };
    let (name, _) = take_ident(rest)?;
    Some((name.to_string(), kind.to_string()))
}

fn parse_toml(trimmed: &str) -> Option<(String, String)> {
    // A trailing comment after the closing bracket(s) is allowed:
    // `[x] # note`, `[[x]] # note`.
    let (inner, rest) = if let Some(r) = trimmed.strip_prefix("[[") {
        let close = r.find("]]")?;
        (&r[..close], &r[close + 2..])
    } else {
        let r = trimmed.strip_prefix('[')?;
        let close = r.find(']')?;
        (&r[..close], &r[close + 1..])
    };
    let rest = rest.trim();
    if !rest.is_empty() && !rest.starts_with('#') {
        return None;
    }
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

/// Strict ledger load for the write paths.
///
/// A missing entry is a fresh ledger (`default`). Anything else that
/// cannot be trusted — a store error, a non-`Runtime` payload, a value
/// that does not decode as a [`TokenGainSummary`], or an existing entry
/// flagged `sensitive`/`ever_sensitive` — is an error, never a silent
/// reset to `default`: `record_*` must not overwrite ledger state it
/// could not read (a malformed or sensitive entry stays untouched for a
/// human to inspect, and a sensitive entry's value is never adopted).
fn try_load_token_gain(store: &impl StateStore) -> Result<TokenGainSummary, StoreError> {
    let Some(entry) = store.get_state_by_key(
        &StateScope::Workspace,
        StateConcern::Runtime,
        TOKEN_GAIN_SLUG,
    )?
    else {
        return Ok(TokenGainSummary::default());
    };
    if entry.sensitive || entry.ever_sensitive {
        return Err(StoreError::InvalidState(format!(
            "token-gain ledger at {TOKEN_GAIN_SLUG} is marked sensitive; refusing to read or overwrite it"
        )));
    }
    match entry.payload {
        StatePayload::Runtime(rt) => serde_json::from_value(rt.value).map_err(|e| {
            StoreError::Decode(format!("token-gain ledger at {TOKEN_GAIN_SLUG}: {e}"))
        }),
        _ => Err(StoreError::Decode(format!(
            "token-gain ledger at {TOKEN_GAIN_SLUG} does not hold a Runtime payload"
        ))),
    }
}

/// Load the token-gain ledger for stats display.
///
/// Infallible by design: a missing entry reads as `default`, and so does
/// anything the strict write-path load rejects (store error, wrong payload,
/// malformed value, sensitive/`ever_sensitive` entry) — the display fails
/// closed to zeroes instead of surfacing untrusted or sensitive numbers.
/// Writes go through the strict load and propagate those cases as errors.
pub fn load_token_gain(store: &impl StateStore) -> TokenGainSummary {
    try_load_token_gain(store).unwrap_or_default()
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
///
/// Propagates an error (writing nothing) when the existing ledger entry
/// cannot be trusted — see `try_load_token_gain`.
pub fn record_token_gain_index(
    store: &impl StateStore,
    files_indexed: u64,
    symbols_indexed: u64,
    at: At,
    by: &str,
) -> Result<TokenGainSummary, StoreError> {
    let mut summary = try_load_token_gain(store)?;
    summary.files_indexed = files_indexed;
    summary.symbols_indexed = symbols_indexed;
    save_token_gain(store, summary, at, by)
}

/// Record one query: increment `queries_count` by 1, add
/// `saved_this_query` to `tokens_saved` (saturating), and refresh the
/// latest file/symbol counts.
///
/// Propagates an error (writing nothing) when the existing ledger entry
/// cannot be trusted — see `try_load_token_gain`.
pub fn record_token_gain_query(
    store: &impl StateStore,
    saved_this_query: u64,
    files_indexed: u64,
    symbols_indexed: u64,
    at: At,
    by: &str,
) -> Result<TokenGainSummary, StoreError> {
    let mut summary = try_load_token_gain(store)?;
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
            "src/extra.rs",
            concat!(
                "pub(crate) async fn café() {}\n",
                "pub(crate) const fn const_made() {}\n",
                "extern \"C\" fn extern_made() {}\n",
                "pub extern fn pub_extern_made() {}\n",
                "pub fn r#match() {}\n",
                "impl<T: Fn() -> T> Foo<T> {}\n",
            ),
        );
        write(
            root,
            "src/app.ts",
            concat!(
                "export function buildWidget() {}\n",
                "export interface WidgetProps { title: string }\n",
                "export default function defaultWidget() {}\n",
                "export const exportedConst = 1;\n",
                "const topConst = 2;\n",
                "function outer() {\n",
                "  const localConst = 3;\n",
                "  let localLet = 4;\n",
                "}\n",
            ),
        );
        write(
            root,
            "tools/make.py",
            "def make_widget():\n    return 1\n\nclass WidgetFactory:\n    pass\nasync  def fetch_widget():\n    pass\n",
        );
        write(
            root,
            "Cargo.toml",
            "[package] # the package\nname = \"x\"\n[[bin]] # binaries\nname = \"y\"\n",
        );
        write(
            root,
            "README.md",
            "# Title Here\n\ntext\n## Sub Section\nmore\n",
        );
        write(root, "target/hidden.rs", "pub fn hidden_target() {}\n");
        write(root, ".git/hidden.rs", "pub fn hidden_git() {}\n");
        write(root, "dist/hidden.js", "function hidden_dist() {}\n");
        write(root, ".next/hidden.js", "function hidden_next() {}\n");
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
            6,
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
            syms(&index, "src/extra.rs"),
            vec![
                ("café", "fn", 1, 1),
                ("const_made", "fn", 2, 2),
                ("extern_made", "fn", 3, 3),
                ("pub_extern_made", "fn", 4, 4),
                ("match", "fn", 5, 5),
                ("Foo", "impl", 6, 6)
            ]
        );
        assert_eq!(
            syms(&index, "src/app.ts"),
            vec![
                ("buildWidget", "function", 1, 1),
                ("WidgetProps", "interface", 2, 2),
                ("defaultWidget", "function", 3, 3),
                ("exportedConst", "const", 4, 4),
                ("topConst", "const", 5, 5),
                ("outer", "function", 6, 9)
            ]
        );
        assert_eq!(
            syms(&index, "tools/make.py"),
            vec![
                ("make_widget", "def", 1, 3),
                ("WidgetFactory", "class", 4, 5),
                ("fetch_widget", "def", 6, 7)
            ]
        );
        assert_eq!(
            syms(&index, "Cargo.toml"),
            vec![("package", "section", 1, 2), ("bin", "section", 3, 4)]
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
        assert!(!all_names.contains(&"hidden_git"));
        assert!(!all_names.contains(&"hidden_dist"));
        assert!(!all_names.contains(&"hidden_next"));
        assert!(!all_names.contains(&"localConst"));
        assert!(!all_names.contains(&"localLet"));
        // Café exact-matches case-insensitively (Unicode) via Def and Search.
        let def = index.search(CodeSearchKind::Def, "CAFÉ", 10);
        assert_eq!(
            def.symbols
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["café"]
        );
        let found = index.search(CodeSearchKind::Search, "café", 10);
        assert_eq!(found.symbols[0].name, "café");
        assert_eq!(found.symbols[0].kind, "fn");
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
    fn outcome_from_hits_books_the_gain_like_search_and_in_any_order() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.rs", "pub fn one() {}\npub fn two() {}\n");
        write(tmp.path(), "b.rs", "pub fn three() {}\n");
        let index = scan_code_index(tmp.path());
        let found = index.search(CodeSearchKind::Outline, ".rs", 10);
        assert_eq!(found.symbols.len(), 3);
        let again = outcome_from_hits(
            found.symbols.clone(),
            found.files_indexed,
            found.symbols_indexed,
        );
        assert_eq!(again, found);
        let mut reversed = found.symbols.clone();
        reversed.reverse();
        let shuffled = outcome_from_hits(reversed, 2, 3);
        assert_eq!(
            shuffled.tokens_saved_this_query,
            found.tokens_saved_this_query
        );
        assert_eq!(
            outcome_from_hits(Vec::new(), 2, 3).tokens_saved_this_query,
            0
        );
    }

    #[test]
    fn the_index_remembers_its_canonical_root() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/a.rs", "pub fn one() {}\n");
        let canonical = fs::canonicalize(tmp.path()).unwrap();
        let dotted = tmp.path().join("src").join("..");
        assert_eq!(
            scan_code_index(tmp.path()).root(),
            Some(canonical.as_path())
        );
        assert_eq!(scan_code_index(&dotted).root(), Some(canonical.as_path()));
        // A root that cannot be canonicalized is kept as given, empty or not.
        let missing = tmp.path().join("missing");
        let none = scan_code_index(&missing);
        assert_eq!(none.root(), Some(missing.as_path()));
        assert_eq!(none.files_indexed(), 0);
        assert_eq!(CodeIndex::default().root(), None);
    }

    #[test]
    fn file_views_expose_paths_digests_and_symbols_in_order() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "b.rs", "pub fn beta() {}\n");
        write(tmp.path(), "a.rs", "pub fn alpha() {}\npub struct Gamma;\n");
        let index = scan_code_index(tmp.path());
        let files: Vec<_> = index.files().collect();
        assert_eq!(
            files.iter().map(|f| f.path()).collect::<Vec<_>>(),
            ["a.rs", "b.rs"]
        );
        let syms: Vec<_> = files[0].symbols().collect();
        assert_eq!(
            syms.iter()
                .map(|s| (s.name(), s.kind(), s.path(), s.start_line()))
                .collect::<Vec<_>>(),
            [("alpha", "fn", "a.rs", 1), ("Gamma", "struct", "a.rs", 2)]
        );
        assert_eq!(
            syms[0].hit(),
            index.search(CodeSearchKind::Def, "alpha", 5).symbols[0]
        );
        // The digest follows the text: stable on a rescan, new when a line changes.
        let digest = files[0].content_digest();
        assert_eq!(digest.len(), 64);
        assert_eq!(
            scan_code_index(tmp.path())
                .files()
                .next()
                .unwrap()
                .content_digest(),
            digest
        );
        write(tmp.path(), "a.rs", "pub fn alpha() {}\npub struct Delta;\n");
        let changed = scan_code_index(tmp.path());
        assert_ne!(changed.files().next().unwrap().content_digest(), digest);
        assert_eq!(
            changed.files().nth(1).unwrap().content_digest(),
            files[1].content_digest()
        );
    }

    #[test]
    fn search_matches_term_past_signature_cap_as_symbol_not_line() {
        let tmp = TempDir::new().unwrap();
        let pad = "a".repeat(220);
        write(
            tmp.path(),
            "long.rs",
            &format!("pub fn long_decl({pad}: u32, zzz_past_cap_marker: u32) {{}}\n"),
        );
        let index = scan_code_index(tmp.path());
        let out = index.search(CodeSearchKind::Search, "zzz_past_cap_marker", 10);
        assert_eq!(out.symbols.len(), 1, "{out:?}");
        assert_eq!(out.symbols[0].name, "long_decl");
        assert_eq!(out.symbols[0].kind, "fn");
        assert!(!out.symbols[0].signature.contains("zzz_past_cap_marker"));
    }

    #[test]
    fn punctuation_only_query_finds_line() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "punct.rs",
            "pub fn holder() {\n    let x = a -> b;\n}\n",
        );
        let index = scan_code_index(tmp.path());
        let out = index.search(CodeSearchKind::Search, "->", 10);
        assert!(
            out.symbols
                .iter()
                .any(|s| s.kind == "line" && s.start_line == 2),
            "{out:?}"
        );
    }

    #[test]
    fn root_as_file_indexes_that_file_and_missing_root_is_empty() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "solo.rs", "pub fn solo() {}\n");
        let file_root = scan_code_index(&tmp.path().join("solo.rs"));
        assert_eq!(file_root.files_indexed(), 1);
        assert_eq!(file_root.symbols_indexed(), 1);
        let missing = scan_code_index(&tmp.path().join("does-not-exist"));
        assert_eq!(missing.files_indexed(), 0);
        assert_eq!(missing.symbols_indexed(), 0);
        let out = missing.search(CodeSearchKind::Search, "solo", 10);
        assert_eq!(out.files_indexed, 0);
        assert_eq!(out.symbols_indexed, 0);
        assert!(out.symbols.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_file_is_skipped() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "real.rs", "pub fn real_one() {}\n");
        std::os::unix::fs::symlink(tmp.path().join("real.rs"), tmp.path().join("link.rs")).unwrap();
        let index = scan_code_index(tmp.path());
        assert_eq!(index.files_indexed(), 1);
        assert_eq!(syms(&index, "real.rs"), vec![("real_one", "fn", 1, 1)]);
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

    fn seed_ledger(store: &MemStore, value: serde_json::Value, sensitivity: SensitivityPatch) {
        store
            .upsert_state(UpsertStateArgs {
                scope: StateScope::Workspace,
                slug: TOKEN_GAIN_SLUG.to_string(),
                sensitivity,
                payload: StatePayload::Runtime(RuntimeEntry {
                    value,
                    merge_mode: MergeMode::LastWriter,
                }),
                at: at(),
                by: "seeder".to_string(),
            })
            .unwrap();
    }

    fn stored_ledger(store: &MemStore) -> crate::state::StateEntry {
        store
            .get_state_by_key(
                &StateScope::Workspace,
                StateConcern::Runtime,
                TOKEN_GAIN_SLUG,
            )
            .unwrap()
            .expect("ledger entry present")
    }

    #[test]
    fn token_gain_malformed_entry_errors_and_stays_unchanged() {
        let store = MemStore::default();
        let bogus = serde_json::json!({"queries_count": "oops"});
        seed_ledger(&store, bogus.clone(), SensitivityPatch::Inherit);
        assert_eq!(load_token_gain(&store), TokenGainSummary::default());
        assert!(record_token_gain_query(&store, 10, 1, 1, at(), "tester").is_err());
        assert!(record_token_gain_index(&store, 1, 1, at(), "tester").is_err());
        match stored_ledger(&store).payload {
            StatePayload::Runtime(rt) => assert_eq!(rt.value, bogus),
            other => panic!("expected Runtime payload, got {other:?}"),
        }
    }

    #[test]
    fn token_gain_sensitive_entry_errors_and_loads_as_default() {
        let store = MemStore::default();
        let secret = serde_json::json!({
            "queries_count": 7,
            "tokens_saved": 99,
            "files_indexed": 1,
            "symbols_indexed": 2
        });
        seed_ledger(&store, secret.clone(), SensitivityPatch::SetSensitive);
        let seeded = stored_ledger(&store);
        assert!(seeded.sensitive || seeded.ever_sensitive);
        assert_eq!(load_token_gain(&store), TokenGainSummary::default());
        assert!(record_token_gain_query(&store, 10, 1, 1, at(), "tester").is_err());
        assert!(record_token_gain_index(&store, 1, 1, at(), "tester").is_err());
        match stored_ledger(&store).payload {
            StatePayload::Runtime(rt) => assert_eq!(rt.value, secret),
            other => panic!("expected Runtime payload, got {other:?}"),
        }
    }
}
