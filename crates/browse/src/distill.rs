//! The distiller: one pass over a parsed [`Document`] that yields a token-efficient
//! accessibility outline, a table of the interactive elements with deterministic handles, and
//! the facts the auditor judges.
//!
//! # Handles
//!
//! Interactive elements get `@e1`, `@e2`, ... in document order. An element is interactive when
//! it is visible and is one of:
//!
//! * `a` with an `href`, `button`, `textarea`, `select`, or `input` whose type is not `hidden`;
//! * an element whose `role` is one of [`INTERACTIVE_ROLES`] (an explicit role wins over the tag's
//!   own);
//! * an element with a `data-testid` (its role is the explicit or tag role, else `generic`).
//!
//! # Hidden content
//!
//! An element and everything under it is skipped when it has the `hidden` attribute,
//! `aria-hidden="true"`, `type="hidden"` (inputs), or an inline `display:none`. The subtrees of
//! `head`, `script`, `style` and `template` are not content and are skipped too. Skipped
//! elements get no handle, no outline line, and are not audited.
//!
//! # Accessible name
//!
//! The first non-empty of: `aria-labelledby` (the text of the referenced ids, in order),
//! `aria-label`, then by element: a text input, textarea or select takes its `<label>` (by `for`
//! and `id`, or the wrapping label), `title`, and `placeholder`; a `button`-like input takes its
//! `value` (a submit or reset button without one is named by the browser's own default,
//! `Submit` or `Reset`), an image input its `alt`; a `button` takes its label, then its text;
//! every other element takes its text (an `img` inside it counting as its `alt`), then `title`.
//! A name has runs of whitespace and control characters folded to one space and is cut at
//! [`MAX_NAME_CHARS`] characters.
//!
//! # Size accounting
//!
//! `distilled_bytes` is the length of [`Snapshot::render_body`], which is the title, the
//! viewport meta, the outline and the element table, as printed. `raw_bytes` is the length of
//! the document that was parsed. `token_savings_pct` is
//! `round((1 - distilled_bytes / raw_bytes) * 1000) / 10`, or `0.0` for an empty document. It
//! is a byte ratio, a stand-in for a token ratio and not a token count; it is negative when
//! the distilled form is larger than the source.

use crate::html::{Document, Element, MAX_DEPTH, NodeId, NodeKind};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

/// The longest accessible name, heading or attribute value kept, in characters.
pub const MAX_NAME_CHARS: usize = 160;
/// The most element ids one `aria-labelledby` resolves.
const MAX_LABELLEDBY_IDS: usize = 16;
/// The most bytes of raw text gathered while computing one name.
const NAME_GATHER_BYTES: usize = 4096;
/// The most nodes visited while computing one name, so a subtree of empty elements ends too.
const NAME_GATHER_NODES: usize = 2048;

/// Roles that make an element interactive whatever its tag.
pub const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "link",
    "tab",
    "searchbox",
    "textbox",
    "checkbox",
    "radio",
    "switch",
    "menuitem",
    "combobox",
    "slider",
];

/// Roles that make an element a landmark whatever its tag.
pub const LANDMARK_ROLES: &[&str] = &[
    "banner",
    "complementary",
    "contentinfo",
    "form",
    "main",
    "navigation",
    "region",
    "search",
];

/// Attributes copied into an element's `attrs` (when present). `value` is left out of a
/// password input.
pub const KEY_ATTRS: &[&str] = &["type", "name", "id", "value", "disabled", "required"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Landmark {
    pub role: String,
    pub tag: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Heading {
    pub level: u8,
    pub text: String,
}

/// One interactive element of the snapshot.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapElement {
    /// `@e1`, `@e2`, ... in document order.
    pub handle: String,
    pub tag: String,
    pub role: String,
    /// The accessible name; empty when the element has none.
    pub name: String,
    pub test_id: Option<String>,
    pub href: Option<String>,
    pub autocomplete: Option<String>,
    /// The [`KEY_ATTRS`] the element carries.
    pub attrs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metrics {
    pub raw_bytes: usize,
    pub distilled_bytes: usize,
    pub token_savings_pct: f64,
}

/// The distilled view of one document.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
    pub title: Option<String>,
    /// The `content` of the first `<meta name="viewport">`.
    pub viewport_meta: Option<String>,
    pub landmarks: Vec<Landmark>,
    pub headings: Vec<Heading>,
    pub elements: Vec<SnapElement>,
    /// The indented outline: landmarks, headings and interactive elements in document order.
    pub outline: String,
    pub metrics: Metrics,
    /// A parser bound was hit, so the snapshot is of part of the document.
    pub truncated: bool,
}

/// A declared inline `min-width` in px.
#[derive(Debug, Clone, PartialEq)]
pub struct MinWidth {
    pub tag: String,
    /// The handle, when the element is interactive.
    pub handle: Option<String>,
    pub px: f64,
}

/// What one pass learned; the snapshot and the auditor both read it.
#[derive(Debug, Clone)]
pub struct Scan {
    pub title: Option<String>,
    pub viewport_meta: Option<String>,
    pub landmarks: Vec<Landmark>,
    pub headings: Vec<Heading>,
    pub elements: Vec<SnapElement>,
    pub min_widths: Vec<MinWidth>,
    pub outline: Vec<String>,
}

/// Fold whitespace and control characters to single spaces, trim, and cut at `max` characters
/// (marking a cut with `…`).
pub fn normalize(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut chars = 0;
    let mut pending_space = false;
    for c in s.chars() {
        if c.is_whitespace() || c.is_control() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            if chars >= max {
                out.push('…');
                return out;
            }
            out.push(' ');
            chars += 1;
            pending_space = false;
        }
        if chars >= max {
            out.push('…');
            return out;
        }
        out.push(c);
        chars += 1;
    }
    out
}

/// Parse the declarations of an inline `style` attribute into lowercase `(property, value)`.
fn declarations(style: &str) -> Vec<(String, String)> {
    let mut cleaned = String::with_capacity(style.len());
    let mut rest = style;
    while let Some(i) = rest.find("/*") {
        cleaned.push_str(&rest[..i]);
        match rest[i + 2..].find("*/") {
            Some(j) => rest = &rest[i + 2 + j + 2..],
            None => {
                rest = "";
                break;
            }
        }
    }
    cleaned.push_str(rest);
    cleaned
        .split(';')
        .filter_map(|d| {
            let (k, v) = d.split_once(':')?;
            let v = v.trim().to_ascii_lowercase();
            // Strip from the first `!` when what follows is `important` (`! important` too).
            let v = match v.split_once('!') {
                Some((value, flag)) if flag.trim().eq_ignore_ascii_case("important") => {
                    value.trim()
                }
                _ => v.as_str(),
            };
            Some((k.trim().to_ascii_lowercase(), v.to_string()))
        })
        .collect()
}

/// The value of the last inline declaration of `property`.
fn style_value(e: &Element, property: &str) -> Option<String> {
    let style = e.attr("style")?;
    declarations(style)
        .into_iter()
        .rev()
        .find(|(k, _)| k == property)
        .map(|(_, v)| v)
}

/// Whether the element itself is hidden (its descendants are hidden with it).
pub fn is_hidden(e: &Element) -> bool {
    e.has_attr("hidden")
        || e.attr("aria-hidden")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
        || (e.tag == "input"
            && e.attr("type")
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("hidden")))
        || style_value(e, "display").is_some_and(|v| v == "none")
}

/// Whether the element's subtree is not page content.
fn is_non_content(tag: &str) -> bool {
    matches!(tag, "head" | "script" | "style" | "template")
}

/// The first token of the `role` attribute, lowercase.
fn explicit_role(e: &Element) -> Option<String> {
    e.attr("role")
        .and_then(|r| r.split_whitespace().next())
        .map(str::to_ascii_lowercase)
}

fn input_type(e: &Element) -> String {
    e.attr("type")
        .map(|t| t.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

/// The role the element's tag implies, when it implies one worth listing.
fn implicit_role(e: &Element) -> Option<&'static str> {
    Some(match e.tag.as_str() {
        "a" if e.has_attr("href") => "link",
        "button" => "button",
        "textarea" => "textbox",
        "select" => "combobox",
        "input" => match input_type(e).as_str() {
            "submit" | "button" | "reset" | "image" => "button",
            "checkbox" => "checkbox",
            "radio" => "radio",
            "search" => "searchbox",
            "range" => "slider",
            "number" => "spinbutton",
            _ => "textbox",
        },
        _ => return None,
    })
}

fn landmark_role(e: &Element) -> Option<String> {
    if let Some(r) = explicit_role(e) {
        return LANDMARK_ROLES.contains(&r.as_str()).then_some(r);
    }
    Some(
        match e.tag.as_str() {
            "header" => "banner",
            "nav" => "navigation",
            "main" => "main",
            "aside" => "complementary",
            "footer" => "contentinfo",
            _ => return None,
        }
        .to_string(),
    )
}

fn heading_level(e: &Element) -> Option<u8> {
    match e.tag.as_str() {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

/// The role of an interactive element, or `None` when the element is not interactive.
fn interactive_role(e: &Element) -> Option<String> {
    if let Some(r) = explicit_role(e)
        && INTERACTIVE_ROLES.contains(&r.as_str())
    {
        return Some(r);
    }
    if let Some(r) = implicit_role(e) {
        return Some(r.to_string());
    }
    if e.has_attr("data-testid") {
        return Some(explicit_role(e).unwrap_or_else(|| "generic".to_string()));
    }
    None
}

/// Tags around which the text of a name gets a separating space.
fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "br"
            | "dd"
            | "details"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "td"
            | "th"
            | "tr"
            | "ul"
    )
}

struct Scanner<'d> {
    doc: &'d Document,
    ids: HashMap<&'d str, NodeId>,
    labels: HashMap<&'d str, NodeId>,
    /// The text of a subtree, by node: each subtree is gathered once.
    texts: RefCell<HashMap<NodeId, String>>,
    out: Scan,
}

impl<'d> Scanner<'d> {
    fn new(doc: &'d Document) -> Self {
        let mut ids = HashMap::new();
        let mut labels = HashMap::new();
        let mut title = None;
        let mut viewport_meta = None;
        // Whether a node lies inside a template, a script, a style, an svg or math subtree, or a
        // hidden one. Parents come before children. The real `<head>` does not count.
        let mut inert = vec![false; doc.len()];
        for id in doc.ids() {
            if let Some(p) = doc.node(id).parent {
                inert[id] = inert[p]
                    || doc.element(p).is_some_and(|pe| {
                        is_hidden(pe)
                            || (pe.tag != "head" && is_non_content(&pe.tag))
                            || matches!(pe.tag.as_str(), "svg" | "math")
                    });
            }
            let Some(e) = doc.element(id) else { continue };
            if let Some(v) = e.attr("id")
                && !v.is_empty()
            {
                ids.entry(v).or_insert(id);
            }
            match e.tag.as_str() {
                "label" => {
                    if let Some(f) = e.attr("for") {
                        labels.entry(f).or_insert(id);
                    }
                }
                "title" if title.is_none() && !inert[id] => {
                    let mut raw = String::new();
                    for &c in doc.children(id) {
                        if let NodeKind::Text(t) = &doc.node(c).kind {
                            raw.push_str(t);
                        }
                    }
                    title = Some(normalize(&raw, MAX_NAME_CHARS));
                }
                "meta"
                    if viewport_meta.is_none()
                        && !inert[id]
                        && e.attr("name")
                            .is_some_and(|n| n.trim().eq_ignore_ascii_case("viewport")) =>
                {
                    viewport_meta = Some(normalize(e.attr("content").unwrap_or(""), 400));
                }
                _ => {}
            }
        }
        Self {
            doc,
            ids,
            labels,
            texts: RefCell::new(HashMap::new()),
            out: Scan {
                title,
                viewport_meta,
                landmarks: Vec::new(),
                headings: Vec::new(),
                elements: Vec::new(),
                min_widths: Vec::new(),
                outline: Vec::new(),
            },
        }
    }

    /// Append the text of the subtree to `buf` (a gather bounded in bytes and in nodes
    /// visited). Returns false once a bound is reached.
    fn gather(&self, id: NodeId, buf: &mut String, depth: usize, visits: &mut usize) -> bool {
        *visits += 1;
        if buf.len() >= NAME_GATHER_BYTES || depth > MAX_DEPTH || *visits > NAME_GATHER_NODES {
            return false;
        }
        match &self.doc.node(id).kind {
            NodeKind::Text(t) => {
                for c in t.chars() {
                    if buf.len() >= NAME_GATHER_BYTES {
                        return false;
                    }
                    buf.push(c);
                }
                true
            }
            NodeKind::Element(e) => {
                if is_hidden(e)
                    || is_non_content(&e.tag)
                    || matches!(e.tag.as_str(), "select" | "textarea" | "input")
                {
                    return true;
                }
                if e.tag == "img" {
                    buf.push(' ');
                    buf.push_str(e.attr("alt").unwrap_or(""));
                    buf.push(' ');
                    return true;
                }
                let block = is_block(&e.tag);
                if block {
                    buf.push(' ');
                }
                for &c in self.doc.children(id) {
                    if !self.gather(c, buf, depth + 1, visits) {
                        return false;
                    }
                }
                if block {
                    buf.push(' ');
                }
                true
            }
            NodeKind::Document => true,
        }
    }

    fn text_of(&self, id: NodeId) -> String {
        if let Some(t) = self.texts.borrow().get(&id) {
            return t.clone();
        }
        let mut buf = String::new();
        self.gather(id, &mut buf, 0, &mut 0);
        let text = normalize(&buf, MAX_NAME_CHARS);
        self.texts.borrow_mut().insert(id, text.clone());
        text
    }

    /// The text of the elements `aria-labelledby` names.
    fn labelled_by(&self, e: &Element) -> String {
        let Some(refs) = e.attr("aria-labelledby") else {
            return String::new();
        };
        let mut parts = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for r in refs.split_whitespace() {
            // An id named twice counts once.
            if seen.contains(&r) {
                continue;
            }
            if seen.len() >= MAX_LABELLEDBY_IDS {
                break;
            }
            seen.push(r);
            let Some(&target) = self.ids.get(r) else {
                continue;
            };
            let mut text = self.text_of(target);
            if text.is_empty()
                && let Some(t) = self.doc.element(target)
            {
                text = normalize(t.attr("aria-label").unwrap_or(""), MAX_NAME_CHARS);
            }
            if !text.is_empty() {
                parts.push(text);
            }
        }
        normalize(&parts.join(" "), MAX_NAME_CHARS)
    }

    /// The text of the `<label>` associated with the control.
    fn label_text(&self, id: NodeId, e: &Element) -> String {
        if let Some(own) = e.attr("id")
            && let Some(&l) = self.labels.get(own)
        {
            let t = self.text_of(l);
            if !t.is_empty() {
                return t;
            }
        }
        match self.doc.ancestor(id, |a| a.tag == "label") {
            Some(l) => self.text_of(l),
            None => String::new(),
        }
    }

    fn accessible_name(&self, id: NodeId, e: &Element) -> String {
        let by = self.labelled_by(e);
        if !by.is_empty() {
            return by;
        }
        let aria = normalize(e.attr("aria-label").unwrap_or(""), MAX_NAME_CHARS);
        if !aria.is_empty() {
            return aria;
        }
        let attr = |n: &str| normalize(e.attr(n).unwrap_or(""), MAX_NAME_CHARS);
        let first = |candidates: [String; 3]| candidates.into_iter().find(|s| !s.is_empty());
        let none = String::new;
        let name = match e.tag.as_str() {
            "input" => {
                let ty = input_type(e);
                match ty.as_str() {
                    "submit" | "reset" | "button" => first([
                        attr("value"),
                        attr("title"),
                        match ty.as_str() {
                            "submit" => "Submit".to_string(),
                            "reset" => "Reset".to_string(),
                            _ => none(),
                        },
                    ]),
                    "image" => first([attr("alt"), attr("title"), none()]),
                    _ => first([self.label_text(id, e), attr("title"), attr("placeholder")]),
                }
            }
            "textarea" | "select" => {
                first([self.label_text(id, e), attr("title"), attr("placeholder")])
            }
            "button" => first([self.label_text(id, e), self.text_of(id), attr("title")]),
            _ => first([self.text_of(id), attr("title"), none()]),
        };
        name.unwrap_or_default()
    }

    fn quote(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    fn walk(&mut self, id: NodeId, landmark_depth: usize, depth: usize) {
        if depth > MAX_DEPTH + 1 {
            return;
        }
        let doc = self.doc;
        let Some(e) = doc.element(id) else {
            if id == doc.root() {
                for &c in doc.children(id) {
                    self.walk(c, landmark_depth, depth + 1);
                }
            }
            return;
        };
        if is_hidden(e) || is_non_content(&e.tag) {
            return;
        }
        let indent = "  ".repeat(landmark_depth);
        let mut child_depth = landmark_depth;
        if let Some(role) = landmark_role(e) {
            let name = {
                let by = self.labelled_by(e);
                let n = if by.is_empty() {
                    normalize(e.attr("aria-label").unwrap_or(""), MAX_NAME_CHARS)
                } else {
                    by
                };
                (!n.is_empty()).then_some(n)
            };
            let mut line = format!("{indent}{role}");
            if let Some(n) = &name {
                line.push(' ');
                line.push_str(&Self::quote(n));
            }
            self.out.outline.push(line);
            self.out.landmarks.push(Landmark {
                role,
                tag: e.tag.clone(),
                name,
            });
            child_depth += 1;
        }
        if let Some(level) = heading_level(e) {
            let text = self.text_of(id);
            self.out
                .outline
                .push(format!("{indent}heading[{level}] {}", Self::quote(&text)));
            self.out.headings.push(Heading { level, text });
        }
        let mut handle = None;
        if let Some(role) = interactive_role(e) {
            let h = format!("@e{}", self.out.elements.len() + 1);
            let name = self.accessible_name(id, e);
            let mut attrs = BTreeMap::new();
            let password = e.tag == "input" && input_type(e) == "password";
            for k in KEY_ATTRS {
                if *k == "value" && password {
                    continue;
                }
                if let Some(v) = e.attr(k) {
                    attrs.insert((*k).to_string(), normalize(v, MAX_NAME_CHARS));
                }
            }
            let opt = |n: &str| e.attr(n).map(|v| normalize(v, 200));
            self.out
                .outline
                .push(format!("{indent}{h} {role} {}", Self::quote(&name)));
            self.out.elements.push(SnapElement {
                handle: h.clone(),
                tag: e.tag.clone(),
                role,
                name,
                test_id: opt("data-testid"),
                href: opt("href"),
                autocomplete: opt("autocomplete"),
                attrs,
            });
            handle = Some(h);
        }
        if let Some(v) = style_value(e, "min-width")
            && let Some(px) = parse_px(&v)
        {
            self.out.min_widths.push(MinWidth {
                tag: e.tag.clone(),
                handle,
                px,
            });
        }
        for &c in doc.children(id) {
            self.walk(c, child_depth, depth + 1);
        }
    }
}

/// A CSS length in `px` (`320px`, `12.5PX`); `None` for anything else.
fn parse_px(v: &str) -> Option<f64> {
    let n = v.trim().strip_suffix("px")?.trim();
    let px: f64 = n.parse().ok()?;
    (px.is_finite() && px >= 0.0).then_some(px)
}

/// Walk `doc` once.
pub fn scan(doc: &Document) -> Scan {
    let mut s = Scanner::new(doc);
    s.walk(doc.root(), 0, 0);
    s.out
}

fn tsv(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

impl Snapshot {
    /// The part of the output an agent reads: title, viewport meta, outline, element table.
    /// Its length is `metrics.distilled_bytes`.
    pub fn render_body(&self) -> String {
        let mut s = String::new();
        s.push_str("title: ");
        s.push_str(&tsv(self.title.as_deref().unwrap_or("")));
        s.push_str("\nviewport-meta: ");
        s.push_str(&tsv(self.viewport_meta.as_deref().unwrap_or("(none)")));
        s.push_str("\n\n# outline\n");
        s.push_str(&self.outline);
        s.push_str("\n\n# elements\nhandle\ttag\trole\tname\ttest_id\thref\tautocomplete\tattrs\n");
        for e in &self.elements {
            let attrs = e
                .attrs
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            s.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                e.handle,
                e.tag,
                e.role,
                tsv(&e.name),
                tsv(e.test_id.as_deref().unwrap_or("")),
                tsv(e.href.as_deref().unwrap_or("")),
                tsv(e.autocomplete.as_deref().unwrap_or("")),
                tsv(&attrs),
            ));
        }
        s
    }
}

/// `round((1 - distilled / raw) * 1000) / 10`, or `0.0` for an empty document.
pub fn token_savings_pct(raw_bytes: usize, distilled_bytes: usize) -> f64 {
    if raw_bytes == 0 {
        return 0.0;
    }
    let ratio = 1.0 - distilled_bytes as f64 / raw_bytes as f64;
    // `+ 0.0` turns a negative zero into zero.
    (ratio * 1000.0).round() / 10.0 + 0.0
}

/// Distill a parsed document; `raw_bytes` is the length of the source it came from.
pub fn distill_document(doc: &Document, raw_bytes: usize) -> Snapshot {
    let s = scan(doc);
    let mut snap = Snapshot {
        title: s.title,
        viewport_meta: s.viewport_meta,
        landmarks: s.landmarks,
        headings: s.headings,
        elements: s.elements,
        outline: s.outline.join("\n"),
        metrics: Metrics {
            raw_bytes,
            distilled_bytes: 0,
            token_savings_pct: 0.0,
        },
        truncated: doc.notes.any(),
    };
    let distilled = snap.render_body().len();
    snap.metrics.distilled_bytes = distilled;
    snap.metrics.token_savings_pct = token_savings_pct(raw_bytes, distilled);
    snap
}

/// Parse and distill `html`.
pub fn distill_html(html: &str) -> Snapshot {
    distill_document(&crate::html::parse(html), html.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(html: &str) -> Snapshot {
        distill_html(html)
    }

    fn names(s: &Snapshot) -> Vec<(&str, &str, &str)> {
        s.elements
            .iter()
            .map(|e| (e.handle.as_str(), e.role.as_str(), e.name.as_str()))
            .collect()
    }

    #[test]
    fn handles_follow_document_order() {
        let s = snap(
            "<a href=/a>A</a><div><button>B</button><input name=c placeholder=C></div>\
             <select aria-label=D></select><textarea title=E></textarea>",
        );
        assert_eq!(
            names(&s),
            vec![
                ("@e1", "link", "A"),
                ("@e2", "button", "B"),
                ("@e3", "textbox", "C"),
                ("@e4", "combobox", "D"),
                ("@e5", "textbox", "E"),
            ]
        );
    }

    #[test]
    fn an_anchor_without_href_is_not_interactive_unless_it_has_a_role() {
        let s = snap("<a>plain</a><a role=button>act</a><a href=x>go</a>");
        assert_eq!(
            names(&s),
            vec![("@e1", "button", "act"), ("@e2", "link", "go")]
        );
    }

    #[test]
    fn roles_and_testids_make_elements_interactive() {
        let s = snap(
            "<div role=button>b</div><div role=tab>t</div><div role=SearchBox aria-label=q></div>\
             <div data-testid=card>c</div><span role=presentation data-testid=x>y</span>\
             <div role=banner>not</div>",
        );
        assert_eq!(
            names(&s),
            vec![
                ("@e1", "button", "b"),
                ("@e2", "tab", "t"),
                ("@e3", "searchbox", "q"),
                ("@e4", "generic", "c"),
                ("@e5", "presentation", "y"),
            ]
        );
        assert_eq!(s.elements[3].test_id.as_deref(), Some("card"));
    }

    #[test]
    fn an_explicit_role_overrides_the_tags_role() {
        let s = snap("<button role=tab>t</button><input type=checkbox role=switch aria-label=s>");
        assert_eq!(names(&s), vec![("@e1", "tab", "t"), ("@e2", "switch", "s")]);
    }

    #[test]
    fn input_types_map_to_roles() {
        let s = snap(
            "<input type=email aria-label=a><input type=search aria-label=b>\
             <input type=checkbox aria-label=c><input type=radio aria-label=d>\
             <input type=range aria-label=e><input type=number aria-label=f>\
             <input type=submit value=g><input aria-label=h>",
        );
        let roles: Vec<_> = s.elements.iter().map(|e| e.role.as_str()).collect();
        assert_eq!(
            roles,
            vec![
                "textbox",
                "searchbox",
                "checkbox",
                "radio",
                "slider",
                "spinbutton",
                "button",
                "textbox"
            ]
        );
    }

    #[test]
    fn hidden_content_gets_no_handle_and_hides_its_subtree() {
        let s = snap(
            "<button hidden>a</button><button aria-hidden=true>b</button>\
             <button aria-hidden=TRUE>b2</button>\
             <input type=hidden name=csrf><input type=HIDDEN name=x>\
             <div style='color:red; DISPLAY : none !important'><button>c</button></div>\
             <div hidden><a href=x>d</a></div>\
             <button aria-hidden=false>shown</button>\
             <div style='display:block'><button>shown2</button></div>\
             <h2 hidden>no</h2>",
        );
        assert_eq!(
            names(&s),
            vec![("@e1", "button", "shown"), ("@e2", "button", "shown2")]
        );
        assert!(s.headings.is_empty());
    }

    #[test]
    fn script_style_template_and_head_content_is_not_listed() {
        let s = snap(
            "<head><title>T</title></head><script><button>x</button></script>\
             <template><button>y</button></template><style>a{}</style><button>real</button>",
        );
        assert_eq!(names(&s), vec![("@e1", "button", "real")]);
        assert_eq!(s.title.as_deref(), Some("T"));
    }

    #[test]
    fn name_from_content_with_nested_inline_and_block_text() {
        let s = snap(
            "<a href=x>Foo<em>bar</em></a><button><div>Save</div><div>draft</div></button>\
             <a href=y><img src=i alt='Logo'> Home</a>",
        );
        assert_eq!(s.elements[0].name, "Foobar");
        assert_eq!(s.elements[1].name, "Save draft");
        assert_eq!(s.elements[2].name, "Logo Home");
    }

    #[test]
    fn name_priority_labelledby_then_aria_label_then_content_then_title() {
        let s = snap(
            "<span id=l1>First</span><span id=l2>Second</span>\
             <button aria-labelledby='l1 missing l2' aria-label=X title=Y>Z</button>\
             <button aria-label=X title=Y>Z</button>\
             <button title=Y>Z</button>\
             <button title=Y></button>\
             <button aria-labelledby=nope aria-label=X>Z</button>",
        );
        let n: Vec<_> = s.elements.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(n, vec!["First Second", "X", "Z", "Y", "X"]);
    }

    #[test]
    fn labelledby_falls_back_to_the_targets_aria_label() {
        let s = snap("<i id=a aria-label='Close'></i><button aria-labelledby=a></button>");
        assert_eq!(s.elements[0].name, "Close");
    }

    #[test]
    fn inputs_are_named_by_label_title_placeholder() {
        let s = snap(
            "<label for=e>Email</label><input id=e placeholder=ph>\
             <label>Name <input></label>\
             <input title=T placeholder=P>\
             <input placeholder=P2>\
             <textarea id=t></textarea><label for=t>Notes</label>\
             <label for=s>Pick</label><select id=s><option>One</option></select>",
        );
        let n: Vec<_> = s.elements.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(n, vec!["Email", "Name", "T", "P2", "Notes", "Pick"]);
    }

    #[test]
    fn button_inputs_use_value_alt_and_browser_defaults() {
        let s = snap(
            "<input type=submit value=Go><input type=submit><input type=reset>\
             <input type=button><input type=button value=Do><input type=image alt=Search>\
             <input type=image>",
        );
        let n: Vec<_> = s.elements.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(n, vec!["Go", "Submit", "Reset", "", "Do", "Search", ""]);
    }

    #[test]
    fn a_label_wrapping_a_select_does_not_take_the_option_text() {
        let s = snap("<label>Size <select><option>Small<option>Large</select></label>");
        assert_eq!(s.elements[0].name, "Size");
    }

    #[test]
    fn names_fold_whitespace_strip_control_characters_and_cut_long_text() {
        let long = "x".repeat(MAX_NAME_CHARS + 50);
        let s = snap(&format!(
            "<button>  a \n\t b\u{1b}[31m c </button><button>{long}</button>"
        ));
        assert_eq!(s.elements[0].name, "a b [31m c");
        assert_eq!(s.elements[1].name.chars().count(), MAX_NAME_CHARS + 1);
        assert!(s.elements[1].name.ends_with('…'));
    }

    #[test]
    fn attributes_keep_the_key_set_and_drop_a_password_value() {
        let s = snap(
            "<input type=text name=user id=u value=bob required autocomplete=username class=c>\
             <input type=password name=pw value=hunter2 autocomplete=current-password>",
        );
        let a = &s.elements[0];
        assert_eq!(a.autocomplete.as_deref(), Some("username"));
        assert_eq!(
            a.attrs
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>(),
            vec!["id=u", "name=user", "required=", "type=text", "value=bob"]
        );
        let p = &s.elements[1];
        assert!(!p.attrs.contains_key("value"));
        assert_eq!(p.attrs.get("type").map(String::as_str), Some("password"));
        assert!(!s.render_body().contains("hunter2"));
    }

    #[test]
    fn href_and_testid_are_reported() {
        let s = snap("<a href=' /next?a=1&amp;b=2 ' data-testid=nav-next>Next</a>");
        assert_eq!(s.elements[0].href.as_deref(), Some("/next?a=1&b=2"));
        assert_eq!(s.elements[0].test_id.as_deref(), Some("nav-next"));
    }

    #[test]
    fn title_and_viewport_meta() {
        let s = snap(
            "<title> My  &amp; Page </title>\
             <meta NAME=Viewport content='width=device-width, initial-scale=1'>\
             <meta name=viewport content='ignored'>",
        );
        assert_eq!(s.title.as_deref(), Some("My & Page"));
        assert_eq!(
            s.viewport_meta.as_deref(),
            Some("width=device-width, initial-scale=1")
        );
        let none = snap("<p>x</p>");
        assert_eq!(none.title, None);
        assert_eq!(none.viewport_meta, None);
        assert!(none.render_body().contains("viewport-meta: (none)"));
    }

    #[test]
    fn landmarks_headings_and_the_outline() {
        let s = snap(
            "<header><a href=/>Home</a></header>\
             <nav aria-label='Primary'><a href=/a>A</a></nav>\
             <main><h1>Title <small>sub</small></h1><section><h2>Part</h2>\
             <div role=search><input type=search aria-label=Find></div></section></main>\
             <aside><h3>Side</h3></aside><footer><button>Up</button></footer>\
             <div role=navigation aria-labelledby=pn></div><p id=pn>Pages</p>",
        );
        let roles: Vec<_> = s.landmarks.iter().map(|l| l.role.as_str()).collect();
        assert_eq!(
            roles,
            vec![
                "banner",
                "navigation",
                "main",
                "search",
                "complementary",
                "contentinfo",
                "navigation"
            ]
        );
        assert_eq!(s.landmarks[1].name.as_deref(), Some("Primary"));
        assert_eq!(s.landmarks[6].name.as_deref(), Some("Pages"));
        assert_eq!(
            s.headings
                .iter()
                .map(|h| (h.level, h.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "Title sub"), (2, "Part"), (3, "Side")]
        );
        let expected = [
            "banner",
            "  @e1 link \"Home\"",
            "navigation \"Primary\"",
            "  @e2 link \"A\"",
            "main",
            "  heading[1] \"Title sub\"",
            "  heading[2] \"Part\"",
            "  search",
            "    @e3 searchbox \"Find\"",
            "complementary",
            "  heading[3] \"Side\"",
            "contentinfo",
            "  @e4 button \"Up\"",
            "navigation \"Pages\"",
        ]
        .join("\n");
        assert_eq!(s.outline, expected);
    }

    #[test]
    fn a_role_that_is_not_a_landmark_cancels_the_tags_landmark() {
        let s = snap("<header role=presentation>x</header><nav role=menu></nav>");
        assert!(s.landmarks.is_empty());
    }

    #[test]
    fn quotes_in_names_are_escaped_in_the_outline() {
        let s = snap(r#"<button>say "hi" \o/</button>"#);
        assert_eq!(s.outline, r#"@e1 button "say \"hi\" \\o/""#);
    }

    #[test]
    fn min_width_declarations_are_collected_with_the_handle_of_an_interactive_element() {
        let s = scan(&crate::html::parse(
            "<div style='min-width: 400px'>x</div>\
             <button style='min-width:12.5PX'>b</button>\
             <div style='min-width:50%'></div><div style='min-width:10rem'></div>\
             <div style='min-width:300px; min-width: 500px'></div>\
             <div hidden style='min-width:900px'></div>\
             <div style='MIN-WIDTH:40px !important'></div>",
        ));
        let got: Vec<_> = s
            .min_widths
            .iter()
            .map(|m| (m.tag.as_str(), m.handle.as_deref(), m.px))
            .collect();
        assert_eq!(
            got,
            vec![
                ("div", None, 400.0),
                ("button", Some("@e1"), 12.5),
                ("div", None, 500.0),
                ("div", None, 40.0),
            ]
        );
    }

    #[test]
    fn style_comments_do_not_hide_declarations() {
        let e = Element {
            tag: "div".into(),
            attrs: vec![("style".into(), "/* a: b */ display: none /* x".into())],
        };
        assert!(is_hidden(&e));
        let e = Element {
            tag: "div".into(),
            attrs: vec![("style".into(), "/* display:none */ color: red".into())],
        };
        assert!(!is_hidden(&e));
    }

    #[test]
    fn savings_formula_is_rounded_to_one_decimal_and_signed() {
        assert_eq!(token_savings_pct(0, 0), 0.0);
        assert_eq!(token_savings_pct(1000, 250), 75.0);
        assert_eq!(token_savings_pct(3, 2), 33.3);
        assert_eq!(token_savings_pct(3, 1), 66.7);
        assert_eq!(token_savings_pct(100, 100), 0.0);
        assert!(!token_savings_pct(100, 100).is_sign_negative());
        assert_eq!(token_savings_pct(100, 150), -50.0);
    }

    #[test]
    fn metrics_count_the_source_and_the_rendered_body() {
        let html = format!(
            "<html><head><title>T</title><style>{}</style></head><body><script>{}</script>\
             <button>Go</button></body></html>",
            "a{color:red}".repeat(200),
            "var x=1;".repeat(200)
        );
        let s = snap(&html);
        assert_eq!(s.metrics.raw_bytes, html.len());
        assert_eq!(s.metrics.distilled_bytes, s.render_body().len());
        assert!(
            s.metrics.token_savings_pct > 90.0,
            "{}",
            s.metrics.token_savings_pct
        );
        let want = token_savings_pct(s.metrics.raw_bytes, s.metrics.distilled_bytes);
        assert_eq!(s.metrics.token_savings_pct, want);
    }

    #[test]
    fn output_is_deterministic() {
        let html = "<nav><a href=/a>A</a></nav><main><h1>T</h1><input name=q placeholder=Q>\
                    <button data-testid=go>Go</button></main>";
        let a = serde_json::to_string(&snap(html)).unwrap();
        let b = serde_json::to_string(&snap(html)).unwrap();
        assert_eq!(a, b);
        assert_eq!(snap(html).render_body(), snap(html).render_body());
    }

    #[test]
    fn tabs_and_newlines_in_attributes_cannot_break_the_table() {
        let s = snap("<a href='x\ty\nz' data-testid='a\tb'>n</a>");
        let body = s.render_body();
        let row = body.lines().find(|l| l.starts_with("@e1\t")).unwrap();
        assert_eq!(row.split('\t').count(), 8, "{row:?}");
    }

    #[test]
    fn an_empty_document_distills_to_an_empty_snapshot() {
        let s = snap("");
        assert!(s.elements.is_empty() && s.outline.is_empty());
        assert_eq!(s.metrics.raw_bytes, 0);
        assert_eq!(s.metrics.token_savings_pct, 0.0);
    }

    #[test]
    fn a_document_cut_by_a_parser_bound_says_so() {
        let s = snap(&"<div>".repeat(crate::html::MAX_DEPTH + 10));
        assert!(s.truncated);
        assert!(!snap("<p>x</p>").truncated);
    }

    #[test]
    fn a_hostile_deep_document_with_long_names_is_distilled_in_bounded_time() {
        let mut html = String::new();
        for _ in 0..200 {
            html.push_str("<a href=x><span>");
        }
        html.push_str(&"word ".repeat(100_000));
        let start = std::time::Instant::now();
        let s = snap(&html);
        assert!(s.elements.len() >= 100);
        assert!(start.elapsed() < std::time::Duration::from_secs(20));
    }

    #[test]
    fn a_missing_head_end_tag_does_not_hide_the_page() {
        let with_body = "<html><head><title>T</title><meta name=\"viewport\" \
            content=\"width=device-width\"><body><button></button><input></body></html>";
        let no_body = "<html><head><title>T</title><meta name=viewport content=width=device-width>\
            <button></button><input>";
        for html in [with_body, no_body] {
            let s = snap(html);
            assert_eq!(
                names(&s),
                vec![("@e1", "button", ""), ("@e2", "textbox", "")],
                "{html}"
            );
            assert_eq!(s.title.as_deref(), Some("T"));
            assert!(s.viewport_meta.is_some());
        }
    }

    #[test]
    fn important_may_be_separated_by_whitespace_and_any_case() {
        let s = snap(
            "<div style='display:none ! important'><button>a</button></div>\
             <div style='display: none !  IMPORTANT ;'><button>b</button></div>\
             <button>c</button>",
        );
        assert_eq!(names(&s), vec![("@e1", "button", "c")]);
        let sc = scan(&crate::html::parse(
            "<div style='min-width:900px ! important'></div>\
             <div style='min-width:800px!Important'></div>\
             <div style='min-width:700px !importantx'></div>",
        ));
        let px: Vec<f64> = sc.min_widths.iter().map(|m| m.px).collect();
        assert_eq!(px, vec![900.0, 800.0]);
    }

    #[test]
    fn title_and_viewport_meta_ignore_templates_and_hidden_subtrees() {
        let s = snap(
            "<template><title>no</title><meta name=viewport content=width=device-width></template>\
             <div hidden><title>no</title><meta name=viewport content=x></div>\
             <svg><title>icon</title></svg>\
             <head><title>Real</title><meta name=viewport content='width=device-width'></head>",
        );
        assert_eq!(s.title.as_deref(), Some("Real"));
        assert_eq!(s.viewport_meta.as_deref(), Some("width=device-width"));
        // Only the ignored ones: nothing is found.
        let none = snap(
            "<template><title>no</title><meta name=viewport content=width=device-width></template>",
        );
        assert_eq!(none.title, None);
        assert_eq!(none.viewport_meta, None);
    }

    #[test]
    fn labelledby_is_memoised_deduplicated_and_budgeted() {
        // Ordinary cases still name correctly, including a repeated id and a second use.
        let s = snap(
            "<span id=a>First</span><span id=b>Second</span>\
             <button aria-labelledby='a b a'></button><a href=/x aria-labelledby='b a b'></a>\
             <input aria-labelledby=a>",
        );
        assert_eq!(
            names(&s),
            vec![
                ("@e1", "button", "First Second"),
                ("@e2", "link", "Second First"),
                ("@e3", "textbox", "First"),
            ]
        );
        // A hostile page: one target holding very many empty elements, named by many links
        // that each repeat its id.
        let refs = vec!["x"; 16].join(" ");
        let mut html = String::from("<div id=x>");
        html.push_str(&"<i></i>".repeat(20_000));
        html.push_str("</div><div id=y>Label</div>");
        for _ in 0..6_000 {
            html.push_str(&format!("<a href=/p aria-labelledby='{refs} y'>z</a>"));
        }
        let start = std::time::Instant::now();
        let first = snap(&html);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}",
            start.elapsed()
        );
        assert_eq!(first.elements.len(), 6_000);
        assert!(first.elements.iter().all(|e| e.name == "Label"));
        assert_eq!(first.render_body(), snap(&html).render_body());
        // Empty nodes end a gather: a name found after more than the node budget is dropped.
        let deep = format!(
            "<div id=t>{}text</div><button aria-labelledby=t aria-label=fallback></button>",
            "<i></i>".repeat(NAME_GATHER_NODES + 10)
        );
        let s = snap(&deep);
        assert_eq!(names(&s), vec![("@e1", "button", "fallback")]);
    }
}
