//! A small, tolerant HTML tokenizer and tree builder.
//!
//! It builds the DOM a distiller needs, not the one a browser renders: no script runs, no
//! stylesheet is applied, and the tree-construction rules of the HTML standard are reduced to
//! the few that decide where a control sits (void elements, raw-text and escapable raw-text
//! elements, the implied end of `p`, `li`, `dt`, `dd`, `option` and table parts, and an end tag
//! that closes everything open inside it).
//!
//! It never fails and never panics on any `&str`. Every loop consumes at least one byte, and
//! three bounds hold whatever the input:
//!
//! * the input is cut at [`MAX_HTML_BYTES`] (at a character boundary);
//! * an element opened at depth [`MAX_DEPTH`] is kept as a leaf (its children attach to its
//!   parent), so the tree is never deeper than the limit;
//! * parsing stops once the tree has [`MAX_NODES`] nodes.
//!
//! [`ParseNotes`] records which bound was hit, so a caller can say the result is partial.

use std::collections::HashMap;

/// The largest document the parser reads; the rest is dropped.
pub const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;
/// The deepest open-element stack. A deeper element is kept as a leaf.
pub const MAX_DEPTH: usize = 256;
/// The most nodes (elements and text runs) one document holds.
pub const MAX_NODES: usize = 200_000;
/// The most attributes kept on one element; the rest are dropped.
pub const MAX_ATTRS: usize = 64;
/// How many open elements from the top an implied-end-tag rule looks at.
const IMPLIED_END_WINDOW: usize = 8;

/// An index into a [`Document`].
pub type NodeId = usize;

/// An element: lowercase tag name and its attributes in source order (names lowercase, first
/// occurrence of a name wins, values entity-decoded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
}

impl Element {
    /// The value of attribute `name` (lowercase), or `None` when absent. A boolean attribute
    /// (`hidden`, `disabled`) has the value `""`.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn has_attr(&self, name: &str) -> bool {
        self.attr(name).is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// The root; always node 0.
    Document,
    Element(Element),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

/// Which parser bound a document hit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParseNotes {
    /// The input was longer than [`MAX_HTML_BYTES`].
    pub input_truncated: bool,
    /// An element was kept as a leaf because the stack was [`MAX_DEPTH`] deep.
    pub depth_limited: bool,
    /// Parsing stopped at [`MAX_NODES`] nodes.
    pub node_limited: bool,
}

impl ParseNotes {
    /// Whether any bound was hit, so the tree is not the whole document.
    pub fn any(&self) -> bool {
        self.input_truncated || self.depth_limited || self.node_limited
    }
}

/// A parsed document: an arena of nodes in document order (a parent's index is below its
/// children's, and siblings are in source order).
#[derive(Debug, Clone)]
pub struct Document {
    nodes: Vec<Node>,
    pub notes: ParseNotes,
}

impl Document {
    pub fn root(&self) -> NodeId {
        0
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn element(&self, id: NodeId) -> Option<&Element> {
        match &self.nodes[id].kind {
            NodeKind::Element(e) => Some(e),
            _ => None,
        }
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id].children
    }

    /// Every node id in document order.
    pub fn ids(&self) -> std::ops::Range<NodeId> {
        0..self.nodes.len()
    }

    /// The nearest ancestor element of `id` that satisfies `pred`, looking at most
    /// [`MAX_DEPTH`] levels up.
    pub fn ancestor(&self, id: NodeId, pred: impl Fn(&Element) -> bool) -> Option<NodeId> {
        let mut cur = self.nodes[id].parent;
        for _ in 0..=MAX_DEPTH {
            let p = cur?;
            if let Some(e) = self.element(p)
                && pred(e)
            {
                return Some(p);
            }
            cur = self.nodes[p].parent;
        }
        None
    }
}

/// Elements that have no content and no end tag.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr", "keygen",
];

/// A start tag of one of these ends an open `p` that is the innermost open element.
const CLOSES_P: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "details",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "ul",
];

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// Parse `input` into a [`Document`]. See the module docs for the bounds.
pub fn parse(input: &str) -> Document {
    let mut notes = ParseNotes::default();
    let mut end = input.len().min(MAX_HTML_BYTES);
    if end < input.len() {
        notes.input_truncated = true;
        while !input.is_char_boundary(end) {
            end -= 1;
        }
    }
    let s = &input[..end];
    let mut b = Builder::new(notes);
    tokenize(s, &mut b);
    Document {
        nodes: b.nodes,
        notes: b.notes,
    }
}

struct Open {
    id: NodeId,
    foreign: bool,
}

struct Builder {
    nodes: Vec<Node>,
    stack: Vec<Open>,
    open_counts: HashMap<String, u32>,
    notes: ParseNotes,
    seen_html: bool,
    seen_head: bool,
    seen_body: bool,
}

impl Builder {
    fn new(notes: ParseNotes) -> Self {
        Self {
            nodes: vec![Node {
                kind: NodeKind::Document,
                parent: None,
                children: Vec::new(),
            }],
            stack: Vec::new(),
            open_counts: HashMap::new(),
            notes,
            seen_html: false,
            seen_head: false,
            seen_body: false,
        }
    }

    fn full(&self) -> bool {
        self.nodes.len() >= MAX_NODES
    }

    fn current(&self) -> NodeId {
        self.stack.last().map_or(0, |o| o.id)
    }

    fn tag_of(&self, id: NodeId) -> &str {
        match &self.nodes[id].kind {
            NodeKind::Element(e) => e.tag.as_str(),
            _ => "",
        }
    }

    fn alloc(&mut self, kind: NodeKind, parent: NodeId) -> Option<NodeId> {
        if self.full() {
            self.notes.node_limited = true;
            return None;
        }
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind,
            parent: Some(parent),
            children: Vec::new(),
        });
        self.nodes[parent].children.push(id);
        Some(id)
    }

    /// Add character data to `parent`, merging with a preceding text node.
    fn text_to(&mut self, parent: NodeId, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(&last) = self.nodes[parent].children.last()
            && let NodeKind::Text(t) = &mut self.nodes[last].kind
        {
            t.push_str(text);
            return;
        }
        self.alloc(NodeKind::Text(text.to_string()), parent);
    }

    fn text(&mut self, text: &str) {
        let parent = self.current();
        self.text_to(parent, text);
    }

    fn pop(&mut self) {
        if let Some(o) = self.stack.pop() {
            let tag = self.tag_of(o.id).to_string();
            if let Some(n) = self.open_counts.get_mut(&tag) {
                *n = n.saturating_sub(1);
            }
        }
    }

    fn is_open(&self, tag: &str) -> bool {
        self.open_counts.get(tag).copied().unwrap_or(0) > 0
    }

    /// Pop through the nearest open element named in `targets` when it lies within the top
    /// [`IMPLIED_END_WINDOW`] open elements and no element named in `boundaries` is above it.
    fn close_implied(&mut self, targets: &[&str], boundaries: &[&str]) {
        if !targets.iter().any(|t| self.is_open(t)) {
            return;
        }
        let mut found = None;
        for (depth, o) in self.stack.iter().rev().take(IMPLIED_END_WINDOW).enumerate() {
            let tag = self.tag_of(o.id);
            if targets.contains(&tag) {
                found = Some(depth);
                break;
            }
            if boundaries.contains(&tag) {
                return;
            }
        }
        if let Some(depth) = found {
            for _ in 0..=depth {
                self.pop();
            }
        }
    }

    fn auto_close(&mut self, tag: &str) {
        match tag {
            "li" => self.close_implied(&["li"], &["ul", "ol", "menu"]),
            "dt" | "dd" => self.close_implied(&["dt", "dd"], &["dl"]),
            "option" => self.close_implied(&["option"], &["select", "datalist"]),
            "optgroup" => self.close_implied(&["option", "optgroup"], &["select"]),
            "tr" => self.close_implied(&["tr"], &["table"]),
            "td" | "th" => self.close_implied(&["td", "th"], &["tr", "table"]),
            "thead" | "tbody" | "tfoot" => {
                self.close_implied(&["thead", "tbody", "tfoot"], &["table"])
            }
            _ => {}
        }
        if CLOSES_P.contains(&tag) && self.tag_of(self.current()) == "p" {
            self.pop();
        }
    }

    /// Insert an element. Returns its id when one was created (a second `html`, `head` or
    /// `body` is ignored, and a full tree creates nothing).
    fn start_tag(
        &mut self,
        tag: &str,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    ) -> Option<NodeId> {
        let seen = match tag {
            "html" => Some(&mut self.seen_html),
            "head" => Some(&mut self.seen_head),
            "body" => Some(&mut self.seen_body),
            _ => None,
        };
        if let Some(seen) = seen {
            if *seen {
                return None;
            }
            *seen = true;
        }
        self.auto_close(tag);
        let parent = self.current();
        let id = self.alloc(
            NodeKind::Element(Element {
                tag: tag.to_string(),
                attrs,
            }),
            parent,
        )?;
        let foreign = self.stack.last().is_some_and(|o| o.foreign) || tag == "svg" || tag == "math";
        let leaf = VOID.contains(&tag) || (foreign && self_closing);
        if !leaf {
            if self.stack.len() >= MAX_DEPTH {
                self.notes.depth_limited = true;
            } else {
                self.stack.push(Open { id, foreign });
                *self.open_counts.entry(tag.to_string()).or_insert(0) += 1;
            }
        }
        Some(id)
    }

    fn end_tag(&mut self, tag: &str) {
        if !self.is_open(tag) {
            return;
        }
        while let Some(o) = self.stack.last() {
            let matched = self.tag_of(o.id) == tag;
            self.pop();
            if matched {
                break;
            }
        }
    }
}

/// Elements whose content is text up to the matching end tag. The flag says whether character
/// references in it are decoded.
fn raw_text_kind(tag: &str) -> Option<bool> {
    match tag {
        "script" | "style" => Some(false),
        "title" | "textarea" => Some(true),
        _ => None,
    }
}

/// The offset of `</tag` (any case) that is followed by whitespace, `/`, `>` or the end.
fn find_close(s: &str, from: usize, tag: &str) -> Option<usize> {
    let b = s.as_bytes();
    let t = tag.as_bytes();
    let mut i = from;
    while let Some(off) = b.get(i..)?.iter().position(|&c| c == b'<') {
        i += off;
        if b.get(i + 1) == Some(&b'/')
            && let Some(name) = b.get(i + 2..i + 2 + t.len())
            && name.eq_ignore_ascii_case(t)
        {
            let after = i + 2 + t.len();
            if after == b.len() || is_ws(b[after]) || b[after] == b'/' || b[after] == b'>' {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

struct StartTag {
    name: String,
    attrs: Vec<(String, String)>,
    self_closing: bool,
}

/// Read a start tag whose `<` is at `at`. `None` when the input ends inside the tag.
/// The second value is the offset just past the tag.
fn read_start_tag(s: &str, at: usize) -> Option<(StartTag, usize)> {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = at + 1;
    let name_start = i;
    while i < n && !is_ws(b[i]) && b[i] != b'/' && b[i] != b'>' {
        i += 1;
    }
    let name = s[name_start..i].to_ascii_lowercase();
    let mut attrs: Vec<(String, String)> = Vec::new();
    let mut slash = false;
    loop {
        while i < n && (is_ws(b[i]) || b[i] == b'/') {
            slash = b[i] == b'/';
            i += 1;
        }
        if i >= n {
            return None;
        }
        if b[i] == b'>' {
            return Some((
                StartTag {
                    name,
                    attrs,
                    self_closing: slash,
                },
                i + 1,
            ));
        }
        slash = false;
        // An attribute name; the first byte is always taken, so the loop makes progress.
        let ns = i;
        i += 1;
        while i < n && !is_ws(b[i]) && !matches!(b[i], b'=' | b'/' | b'>') {
            i += 1;
        }
        let key = s[ns..i].to_ascii_lowercase();
        while i < n && is_ws(b[i]) {
            i += 1;
        }
        let mut value = String::new();
        if i < n && b[i] == b'=' {
            i += 1;
            while i < n && is_ws(b[i]) {
                i += 1;
            }
            if i < n && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                i += 1;
                let vs = i;
                while i < n && b[i] != q {
                    i += 1;
                }
                if i >= n {
                    return None;
                }
                value = decode_entities(&s[vs..i]);
                i += 1;
            } else {
                let vs = i;
                while i < n && !is_ws(b[i]) && b[i] != b'>' {
                    i += 1;
                }
                value = decode_entities(&s[vs..i]);
            }
        }
        if attrs.len() < MAX_ATTRS && !attrs.iter().any(|(k, _)| *k == key) {
            attrs.push((key, value));
        }
    }
}

fn tokenize(s: &str, b: &mut Builder) {
    let bytes = s.as_bytes();
    let n = bytes.len();
    let mut pos = 0;
    while pos < n && !b.full() {
        if bytes[pos] != b'<' {
            let end = s[pos..].find('<').map_or(n, |o| pos + o);
            let text = decode_entities(&s[pos..end]);
            b.text(&text);
            pos = end;
            continue;
        }
        let rest = &s[pos..];
        if let Some(body) = rest.strip_prefix("<!--") {
            // `<!-->` and `<!--->` are empty comments.
            pos = if body.starts_with('>') {
                pos + 5
            } else if body.starts_with("->") {
                pos + 6
            } else {
                body.find("-->").map_or(n, |o| pos + 4 + o + 3)
            };
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            // A doctype, a CDATA section or a processing instruction: dropped.
            pos = rest.find('>').map_or(n, |o| pos + o + 1);
        } else if rest.starts_with("</") {
            let first = bytes.get(pos + 2).copied();
            if first.is_some_and(|c| c.is_ascii_alphabetic()) {
                let ns = pos + 2;
                let mut i = ns;
                while i < n && !is_ws(bytes[i]) && bytes[i] != b'/' && bytes[i] != b'>' {
                    i += 1;
                }
                let name = s[ns..i].to_ascii_lowercase();
                b.end_tag(&name);
            }
            // `</` not followed by a letter is a bogus comment up to `>`.
            pos = rest.find('>').map_or(n, |o| pos + o + 1);
        } else if bytes.get(pos + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
            let Some((tag, next)) = read_start_tag(s, pos) else {
                // The input ends inside the tag: it is dropped.
                break;
            };
            pos = next;
            let raw = raw_text_kind(&tag.name);
            let name = tag.name.clone();
            let Some(id) = b.start_tag(&tag.name, tag.attrs, tag.self_closing) else {
                continue;
            };
            if let Some(decode) = raw {
                let (content_end, after) = match find_close(s, pos, &name) {
                    Some(c) => (c, s[c..].find('>').map_or(n, |o| c + o + 1)),
                    None => (n, n),
                };
                let mut content = &s[pos..content_end];
                if name == "textarea" {
                    content = content
                        .strip_prefix("\r\n")
                        .or_else(|| content.strip_prefix('\n'))
                        .unwrap_or(content);
                }
                if decode {
                    b.text_to(id, &decode_entities(content));
                } else {
                    b.text_to(id, content);
                }
                // The element opened and closed here; take it off the stack.
                if b.stack.last().is_some_and(|o| o.id == id) {
                    b.pop();
                }
                pos = after;
            }
        } else {
            // A `<` that starts nothing is text.
            b.text("<");
            pos += 1;
        }
    }
    if pos < n && b.full() {
        b.notes.node_limited = true;
    }
}

/// Decode the character references this crate knows: `&amp; &lt; &gt; &quot; &apos; &nbsp;`
/// and a few typographic ones, and numeric references (`&#39;`, `&#x27;`). An unknown or
/// unterminated reference is kept as written.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        match decode_reference(tail) {
            Some((ch, used)) => {
                out.push(ch);
                rest = &tail[used..];
            }
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decode the reference after an `&`: the character and the bytes used (through the `;`).
fn decode_reference(tail: &str) -> Option<(char, usize)> {
    let end = tail.bytes().take(33).position(|c| c == b';')?;
    let body = &tail[..end];
    let ch = if let Some(num) = body.strip_prefix('#') {
        let (digits, radix) = match num.strip_prefix(['x', 'X']) {
            Some(h) => (h, 16),
            None => (num, 10),
        };
        if digits.is_empty() || digits.len() > 8 || !digits.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        let v = u32::from_str_radix(digits, radix).ok()?;
        match char::from_u32(v) {
            Some(c) if v != 0 => c,
            _ => '\u{fffd}',
        }
    } else {
        match body {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            "nbsp" => '\u{a0}',
            "copy" => '\u{a9}',
            "reg" => '\u{ae}',
            "hellip" => '\u{2026}',
            "mdash" => '\u{2014}',
            "ndash" => '\u{2013}',
            "lsquo" => '\u{2018}',
            "rsquo" => '\u{2019}',
            "ldquo" => '\u{201c}',
            "rdquo" => '\u{201d}',
            "bull" => '\u{2022}',
            "middot" => '\u{b7}',
            "times" => '\u{d7}',
            "laquo" => '\u{ab}',
            "raquo" => '\u{bb}',
            "trade" => '\u{2122}',
            "euro" => '\u{20ac}',
            _ => return None,
        }
    };
    Some((ch, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document as `tag(children)` / `"text"`, for compact structural assertions.
    fn shape(doc: &Document) -> String {
        fn walk(doc: &Document, id: NodeId, out: &mut String) {
            match &doc.node(id).kind {
                NodeKind::Document => {
                    for (i, &c) in doc.children(id).iter().enumerate() {
                        if i > 0 {
                            out.push(' ');
                        }
                        walk(doc, c, out);
                    }
                }
                NodeKind::Text(t) => out.push_str(&format!("{t:?}")),
                NodeKind::Element(e) => {
                    out.push_str(&e.tag);
                    if !doc.children(id).is_empty() {
                        out.push('(');
                        for (i, &c) in doc.children(id).iter().enumerate() {
                            if i > 0 {
                                out.push(' ');
                            }
                            walk(doc, c, out);
                        }
                        out.push(')');
                    }
                }
            }
        }
        let mut out = String::new();
        walk(doc, doc.root(), &mut out);
        out
    }

    fn first(doc: &Document, tag: &str) -> NodeId {
        doc.ids()
            .find(|&i| doc.element(i).is_some_and(|e| e.tag == tag))
            .unwrap_or_else(|| panic!("no <{tag}>"))
    }

    #[test]
    fn builds_a_nested_tree_in_document_order() {
        let d = parse("<div><p>one</p><p>two</p></div>");
        assert_eq!(shape(&d), r#"div(p("one") p("two"))"#);
    }

    #[test]
    fn tag_names_are_lowercased_and_end_tags_match_any_case() {
        let d = parse("<DIV><SPAN>x</Span></dIv>");
        assert_eq!(shape(&d), r#"div(span("x"))"#);
    }

    #[test]
    fn void_elements_take_no_children_and_no_end_tag() {
        let d = parse("<p>a<br>b<img src=x>c<input></p>");
        assert_eq!(shape(&d), r#"p("a" br "b" img "c" input)"#);
    }

    #[test]
    fn a_void_end_tag_is_ignored() {
        let d = parse("<p>a</br>b</p>");
        assert_eq!(shape(&d), r#"p("ab")"#);
    }

    #[test]
    fn comments_doctype_and_processing_instructions_are_dropped() {
        let d = parse("<!DOCTYPE html><!-- a <b> --><?xml v?><p>x<!-->y<!--->z</p>");
        assert_eq!(shape(&d), r#"p("xyz")"#);
    }

    #[test]
    fn an_unterminated_comment_swallows_the_rest() {
        let d = parse("<p>x</p><!-- never closed <p>y</p>");
        assert_eq!(shape(&d), r#"p("x")"#);
    }

    #[test]
    fn script_and_style_content_is_raw_text() {
        let d = parse("<script>if (a < b && c) { document.write('<p>x</p>') }</script><p>t</p>");
        assert_eq!(
            shape(&d),
            r#"script("if (a < b && c) { document.write('<p>x</p>') }") p("t")"#
        );
        let d = parse("<style>a > b { color: red }</style>");
        assert_eq!(shape(&d), r#"style("a > b { color: red }")"#);
    }

    #[test]
    fn a_raw_text_close_tag_matches_in_any_case_and_not_as_a_prefix() {
        let d = parse("<script>a</scripty>b</SCRIPT >c");
        assert_eq!(shape(&d), r#"script("a</scripty>b") "c""#);
    }

    #[test]
    fn an_unterminated_script_runs_to_the_end() {
        let d = parse("<script>var a = '<div>';");
        assert_eq!(shape(&d), r#"script("var a = '<div>';")"#);
    }

    #[test]
    fn title_and_textarea_decode_entities_but_do_not_parse_tags() {
        let d = parse("<title>A &amp; <b>B</title><textarea>\n&lt;p&gt; <i></textarea>");
        assert_eq!(
            shape(&d),
            r#"title("A & <b>B") textarea("<p> <i>")"#,
            "the first newline of a textarea is dropped"
        );
    }

    #[test]
    fn attributes_quoted_unquoted_boolean_and_duplicated() {
        let d = parse(
            "<input TYPE=Text name='a b' value=\"x&amp;y\" disabled class=c1 class=c2 data-x=1/>",
        );
        let e = d.element(first(&d, "input")).unwrap();
        assert_eq!(e.attr("type"), Some("Text"));
        assert_eq!(e.attr("name"), Some("a b"));
        assert_eq!(e.attr("value"), Some("x&y"));
        assert_eq!(e.attr("disabled"), Some(""));
        assert_eq!(e.attr("class"), Some("c1"), "the first occurrence wins");
        assert_eq!(e.attr("data-x"), Some("1/"), "a slash in an unquoted value");
    }

    #[test]
    fn an_attribute_name_may_begin_with_equals() {
        let d = parse("<p =x y>t</p>");
        let e = d.element(first(&d, "p")).unwrap();
        assert_eq!(e.attr("=x"), Some(""));
        assert_eq!(e.attr("y"), Some(""));
    }

    #[test]
    fn attribute_values_may_hold_a_greater_than_sign() {
        let d = parse("<a title=\"a > b\" href='x'>t</a>");
        let e = d.element(first(&d, "a")).unwrap();
        assert_eq!(e.attr("title"), Some("a > b"));
        assert_eq!(e.attr("href"), Some("x"));
    }

    #[test]
    fn a_tag_cut_by_the_end_of_input_is_dropped() {
        assert_eq!(shape(&parse("<p>a</p><div class=\"x")), r#"p("a")"#);
        assert_eq!(shape(&parse("<p>a</p><div class=x")), r#"p("a")"#);
        assert_eq!(shape(&parse("<p>a</p><div")), r#"p("a")"#);
    }

    #[test]
    fn a_stray_less_than_is_text() {
        assert_eq!(shape(&parse("a < b <3 <")), r#""a < b <3 <""#);
        assert_eq!(shape(&parse("1 </ 2")), r#""1 ""#);
    }

    #[test]
    fn unclosed_tags_close_at_the_end() {
        assert_eq!(shape(&parse("<div><span>a")), r#"div(span("a"))"#);
    }

    #[test]
    fn a_stray_end_tag_is_ignored() {
        assert_eq!(shape(&parse("</div><p>a</span></p>")), r#"p("a")"#);
    }

    #[test]
    fn misnested_tags_close_what_is_inside_the_end_tag() {
        assert_eq!(
            shape(&parse("<div><b><i>x</div>y</i></b>")),
            r#"div(b(i("x"))) "y""#
        );
    }

    #[test]
    fn li_dt_dd_and_option_end_implicitly() {
        assert_eq!(
            shape(&parse("<ul><li>a<li>b<li>c</ul>")),
            r#"ul(li("a") li("b") li("c"))"#
        );
        assert_eq!(
            shape(&parse("<dl><dt>a<dd>b<dt>c</dl>")),
            r#"dl(dt("a") dd("b") dt("c"))"#
        );
        assert_eq!(
            shape(&parse("<select><option>a<option>b</select>")),
            r#"select(option("a") option("b"))"#
        );
    }

    #[test]
    fn a_nested_list_item_is_not_closed_by_an_inner_list() {
        assert_eq!(
            shape(&parse("<ul><li>a<ul><li>b</ul><li>c</ul>")),
            r#"ul(li("a" ul(li("b"))) li("c"))"#
        );
    }

    #[test]
    fn table_parts_end_implicitly() {
        assert_eq!(
            shape(&parse("<table><tr><td>a<td>b<tr><td>c</table>")),
            r#"table(tr(td("a") td("b")) tr(td("c")))"#
        );
    }

    #[test]
    fn a_block_start_tag_ends_an_open_paragraph() {
        assert_eq!(
            shape(&parse("<p>a<div>b</div><p>c<p>d")),
            r#"p("a") div("b") p("c") p("d")"#
        );
    }

    #[test]
    fn a_second_html_head_or_body_is_ignored() {
        let d = parse("<html><head></head><body><p>a</p></body><body class=x><p>b</p></body>");
        let bodies = d
            .ids()
            .filter(|&i| d.element(i).is_some_and(|e| e.tag == "body"))
            .count();
        assert_eq!(bodies, 1);
    }

    #[test]
    fn a_self_closing_slash_counts_only_in_svg_and_math() {
        assert_eq!(shape(&parse("<div/>a")), r#"div("a")"#);
        assert_eq!(
            shape(&parse("<svg><path d=x /><g/></svg>a")),
            r#"svg(path g) "a""#
        );
    }

    #[test]
    fn entities_named_numeric_and_unknown() {
        assert_eq!(
            decode_entities("a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#x27; &nbsp;|"),
            "a & b <c> \"d\" 'e' \u{a0}|"
        );
        assert_eq!(decode_entities("&#65;&#x42;&#X43;"), "ABC");
        assert_eq!(
            decode_entities("&bogus; &amp &#; &#x; &#xZ; &"),
            "&bogus; &amp &#; &#x; &#xZ; &"
        );
        assert_eq!(
            decode_entities("&#0;&#xD800;&#x110000;&#99999999999;"),
            "\u{fffd}\u{fffd}\u{fffd}&#99999999999;"
        );
        assert_eq!(
            decode_entities("fish &amp chips &amp; more"),
            "fish &amp chips & more"
        );
    }

    #[test]
    fn multibyte_text_survives_every_path() {
        let d = parse(
            "<p title='h\u{e9}llo'>\u{4e16}\u{754c} &amp; \u{1f600}</p><script>\u{1f600}</script>",
        );
        assert_eq!(
            shape(&d),
            "p(\"\u{4e16}\u{754c} & \u{1f600}\") script(\"\u{1f600}\")"
        );
    }

    #[test]
    fn empty_and_whitespace_input_make_an_empty_document() {
        assert!(parse("").is_empty());
        assert_eq!(shape(&parse("   ")), r#""   ""#);
    }

    #[test]
    fn deep_nesting_is_bounded_and_keeps_every_element() {
        let html = "<div>".repeat(MAX_DEPTH * 4) + "x";
        let d = parse(&html);
        assert!(d.notes.depth_limited);
        let divs = d.ids().filter(|&i| d.element(i).is_some()).count();
        assert_eq!(divs, MAX_DEPTH * 4, "no element is lost, it is a leaf");
        // The tree is never deeper than the bound (plus the root).
        let mut deepest = 0;
        for id in d.ids() {
            let mut depth = 0;
            let mut cur = Some(id);
            while let Some(c) = cur {
                depth += 1;
                cur = d.node(c).parent;
            }
            deepest = deepest.max(depth);
        }
        assert!(deepest <= MAX_DEPTH + 2, "deepest was {deepest}");
    }

    #[test]
    fn many_unmatched_end_tags_are_cheap() {
        let html = format!("{}{}", "<b>".repeat(200), "</i>".repeat(500_000));
        let start = std::time::Instant::now();
        let d = parse(&html);
        assert!(!d.is_empty());
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn a_flood_of_implied_end_tag_starts_is_cheap() {
        let html = format!("<ul>{}{}", "<span>".repeat(250), "<li>".repeat(100_000));
        let start = std::time::Instant::now();
        let _ = parse(&html);
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn the_node_count_is_bounded() {
        let html = "<i></i>".repeat(MAX_NODES);
        let d = parse(&html);
        assert!(d.notes.node_limited);
        assert!(d.len() <= MAX_NODES);
    }

    #[test]
    fn the_input_is_cut_at_the_size_bound_on_a_character_boundary() {
        // A 3-byte character straddles the limit.
        let mut html = "a".repeat(MAX_HTML_BYTES - 1);
        html.push('\u{20ac}');
        html.push_str("<p>tail</p>");
        let d = parse(&html);
        assert!(d.notes.input_truncated);
        assert!(!d.ids().any(|i| d.element(i).is_some_and(|e| e.tag == "p")));
    }

    #[test]
    fn hostile_fragments_terminate_without_panicking() {
        let pieces = [
            "<",
            "<<",
            "<<<<",
            "</",
            "<//",
            "<!",
            "<!-",
            "<!--",
            "<!---",
            "<![CDATA[",
            "<?",
            "<a",
            "<a ",
            "<a =",
            "<a ='",
            "<a b='",
            "<a b=\"",
            "<a b=>",
            "<a/",
            "<a //>",
            "</a",
            "</a ",
            "</>",
            "<>",
            "< a>",
            "&",
            "&#",
            "&#x",
            "&;",
            "\0",
            "<\0>",
            "<a\0b>",
            "<script",
            "<script>",
            "<script></",
            "<textarea>",
            "<title>",
            "\u{1f600}",
            "<svg/>",
            "<p>",
            "</p>",
            "<li>",
            "<td>",
            "<option>",
        ];
        // Every concatenation of up to three pieces.
        for a in pieces {
            for b in pieces {
                for c in pieces {
                    let _ = parse(&format!("{a}{b}{c}"));
                }
            }
        }
    }

    #[test]
    fn a_pseudo_random_byte_soup_terminates() {
        let alphabet: Vec<char> = "<>/=\"' !-?&#;abcpdivscriptxyz\n\u{e9}".chars().collect();
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..300 {
            let mut s = String::new();
            for _ in 0..400 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                s.push(alphabet[(x % alphabet.len() as u64) as usize]);
            }
            let _ = parse(&s);
        }
    }
}
