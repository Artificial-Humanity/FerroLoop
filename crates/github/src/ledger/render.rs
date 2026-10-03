//! A decision comment (GitHub ledger spec §4.2): what an issue shows of a
//! decision the ledger holds. Pure — no request and no clock. A comment
//! posted as the decision is made and one recovered later are rendered
//! here alike; only the first says whether the state change completed.

use super::disclose::Visibility;
use fl_core::decision::{Decision, Outcome};
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use fl_core::verdict::Verdict;
use serde_json::{Value, json};

/// The most a comment's body may hold, in bytes of UTF-8 — under GitHub's
/// 65,536-character limit however it counts (spec §4.2).
pub const COMMENT_LIMIT: usize = 60_000;

/// The most one escaped table cell, or one name in the header, may hold.
/// A longer one is cut, and ends in `…`.
pub const CELL_LIMIT: usize = 200;

/// How many entries not found a comment names; the rest it counts.
pub const MISSING_SHOWN: usize = 20;

const MARKER_OPEN: &str = "<!-- fl:decision ";
const MARKER_CLOSE: &str = " -->";

/// One run a decision rests on, as its comment shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    /// What the run was for: its transition, or `reproduction`,
    /// `regression` or `neighbour`.
    pub role: String,
    /// The gate's name in the local catalog, or its IRI when the catalog
    /// does not hold it.
    pub gate: String,
    pub run: GateRun,
}

/// Everything a comment shows of one decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionView {
    pub decision: Decision,
    /// Who wrote the decision's line (spec §3.1).
    pub by: String,
    /// The ledger commit that holds it, when known.
    pub commit: Option<String>,
    /// The runs it rests on, in the order it names them.
    pub rows: Vec<RunRow>,
    /// The attempt it rests on, for an attempt.
    pub attempt: Option<Attempt>,
    /// Entries it rests on that were not found.
    pub missing: Vec<Iri>,
}

/// One output excerpt a private repository's comment shows, folded.
struct Block {
    title: String,
    text: String,
}

/// Whether `id` is one fl writes, and so may stand inside a marker — an
/// HTML comment: `urn:uuid:` and a lowercase, hyphenated UUID, the only
/// form fl mints. ⚠ Nothing else: `Iri::parse` accepts `urn:x:a--><b>`,
/// and a hand-written ledger line can carry it; written into a marker it
/// would close the comment early.
pub fn markable(id: &Iri) -> bool {
    let Some(uuid) = id.as_str().strip_prefix("urn:uuid:") else {
        return false;
    };
    let widths: Vec<usize> = uuid.split('-').map(str::len).collect();
    widths == [8, 4, 4, 4, 12]
        && uuid
            .bytes()
            .all(|b| b == b'-' || b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The marker every decision comment starts with (spec §4.3); `None` for
/// an id fl does not write ([`markable`]).
pub fn marker(id: &Iri) -> Option<String> {
    markable(id).then(|| {
        format!(
            "{MARKER_OPEN}{}{MARKER_CLOSE}",
            json!({ "id": id.as_str() })
        )
    })
}

/// The decision one line marks, if it is a well-formed marker.
fn marker_line(line: &str) -> Option<Iri> {
    let inner = line.strip_prefix(MARKER_OPEN)?.strip_suffix(MARKER_CLOSE)?;
    let v: Value = serde_json::from_str(inner).ok()?;
    Iri::parse(v.get("id")?.as_str()?).ok().filter(markable)
}

/// The decision a comment marks: its first well-formed marker line outside
/// a fenced block, wherever it is in the body — an edit that pushes it
/// down keeps it, and a broken marker-like line above it is passed over.
///
/// ⚠ Never a line inside a fence: an excerpt is fenced, and the gate output
/// it holds is the project's, so a marker quoted there marks nothing — not
/// even when someone deletes fl's own marker line. Only the first: a
/// second marker further down marks nothing. Whose comment may mark a
/// decision at all is the caller's to judge (`GithubLedger::posted`).
///
/// A fence here is any line of three or more backticks, whatever follows
/// them or however far it is indented; tildes open none. fl renders no
/// other kind, so an edit that adds one above the marker costs at most a
/// second comment, never a lost one.
pub fn marked(body: &str) -> Option<Iri> {
    // The open fence's length, while inside one.
    let mut fence: Option<usize> = None;
    for line in body.lines().map(str::trim) {
        let ticks = line.len() - line.trim_start_matches('`').len();
        if ticks >= 3 {
            match fence {
                None => {
                    fence = Some(ticks);
                    continue;
                }
                Some(open) if ticks >= open && line[ticks..].trim().is_empty() => {
                    fence = None;
                    continue;
                }
                Some(_) => {}
            }
        }
        if fence.is_some() {
            continue;
        }
        if let Some(id) = marker_line(line) {
            return Some(id);
        }
    }
    None
}

/// A full commit id: forty hex digits.
pub fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether the text before ends in `GH`, any case: a `-` there would make
/// `GH-<n>` a reference to issue `n`.
fn after_gh(before: &[char]) -> bool {
    matches!(before, [.., 'g' | 'G', 'h' | 'H'])
}

/// `c`, which follows `before`, as it is written in `ctx`.
fn push_escaped(out: &mut String, c: char, before: &[char], ctx: Context) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        // A zero-width space after each: `@name` no longer mentions anyone,
        // and `#1` and `GH-1` no longer link an issue.
        '@' => out.push_str("@&#8203;"),
        '#' => out.push_str("#&#8203;"),
        '-' if after_gh(before) => out.push_str("-&#8203;"),
        '\r' => {}
        '\n' if ctx == Context::Markdown => out.push_str("<br>"),
        '\n' => out.push(' '),
        // `$` too: GitHub renders `$…$` as math. Not `!`: it matters only
        // before `[`, which is escaped. Not inside an HTML element, where
        // markdown is not read and a backslash would show.
        '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '~' | '|' | '$'
            if ctx == Context::Markdown =>
        {
            out.push('\\');
            out.push(c);
        }
        c => out.push(c),
    }
}

/// `s` escaped for `ctx`, holding at most `limit` bytes: cut after a whole
/// escaped character, and ending in `…` when cut.
fn escape_in(s: &str, limit: usize, ctx: Context) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut full = String::with_capacity(s.len());
    // How long the escaped text is after each character.
    let mut ends = Vec::with_capacity(chars.len());
    for (i, c) in chars.iter().enumerate() {
        push_escaped(&mut full, *c, &chars[..i], ctx);
        ends.push(full.len());
    }
    if full.len() <= limit {
        return full;
    }
    let room = limit.saturating_sub('…'.len_utf8());
    let keep = ends
        .iter()
        .copied()
        .take_while(|e| *e <= room)
        .last()
        .unwrap_or(0);
    let mut out = full[..keep].to_string();
    out.push('…');
    out
}

/// `s` as markdown text in a comment (spec §4.2): HTML and markdown
/// escaped, `@`, `#` and `GH-` neutralised, a newline a `<br>` — so a
/// name never notifies anyone, links an issue, opens a tag or breaks a
/// table.
pub fn escape(s: &str) -> String {
    escape_in(s, usize::MAX, Context::Markdown)
}

/// [`escape`], holding at most `limit` bytes, ending in `…` when cut.
pub fn escape_capped(s: &str, limit: usize) -> String {
    escape_in(s, limit, Context::Markdown)
}

/// `s` inside an HTML element: entities and the neutralised `@`, `#` and
/// `GH-` only, a newline a space; at most `limit` bytes.
fn escape_html_capped(s: &str, limit: usize) -> String {
    escape_in(s, limit, Context::Html)
}

/// The line a comment posted as the decision is made adds (spec §4.2):
/// whether its state change completed. `completed`: the command's state
/// change returned without error.
pub fn state_line(outcome: &Outcome, completed: bool) -> String {
    match outcome {
        Outcome::Move {
            allowed: false,
            from,
            ..
        } => format!(
            "The move was refused: the record stays `{}`.",
            from.as_wire()
        ),
        Outcome::Move { to, .. } if completed => {
            format!(
                "The state change completed: the record is now `{}`.",
                to.as_wire()
            )
        }
        Outcome::Move { from, .. } => format!(
            "The move was allowed, but its state change did not complete: the record may still \
             be `{}`.",
            from.as_wire()
        ),
        Outcome::Check { .. } => "A check changes no state.".into(),
        Outcome::Reproduce {
            accepted: false, ..
        } => "The reproduction was refused: the finding is unchanged.".into(),
        Outcome::Reproduce { .. } if completed => {
            "The state change completed: the finding records this gate as its reproduction.".into()
        }
        Outcome::Reproduce { .. } => {
            "The reproduction was accepted, but its state change did not complete.".into()
        }
        Outcome::Verify { closed: false, .. } => {
            "The finding stays open: the repair is not done.".into()
        }
        Outcome::Verify { .. } if completed => {
            "The state change completed: the finding is closed.".into()
        }
        Outcome::Verify { .. } => {
            "The finding passed its verification, but closing it did not complete.".into()
        }
        Outcome::Attempt { .. } => "An attempt changes no state.".into(),
    }
}

/// What was decided, in a word or two — refused ones included (decision
/// 11).
fn verdict_word(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Move { allowed: true, .. } => "allowed".into(),
        Outcome::Move { .. } => "refused".into(),
        Outcome::Check { transition } if transition.passed => "passed".into(),
        Outcome::Check { .. } => "failed".into(),
        Outcome::Reproduce { accepted: true, .. } => "accepted".into(),
        Outcome::Reproduce { .. } => "refused".into(),
        Outcome::Verify { closed: true, .. } => "closed".into(),
        Outcome::Verify { .. } => "not closed".into(),
        Outcome::Attempt { status } => status.as_wire().into(),
    }
}

/// The marker, what was decided, by whom, where its evidence is, a move's
/// states, and the state line when there is one.
fn head(view: &DecisionView, repo: &str, state: Option<&str>) -> String {
    let d = &view.decision;
    let mut out = String::new();
    // ⚠ An id fl does not write gets no marker (`markable`); no caller
    // posts such a comment.
    if let Some(m) = marker(&d.id) {
        out.push_str(&m);
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "### fl {}: {}\n\n",
        d.kind().as_wire(),
        verdict_word(&d.outcome)
    ));
    out.push_str(&format!(
        "Decided by {} at {}. ",
        escape_capped(&view.by, CELL_LIMIT),
        d.at.as_str()
    ));
    // ⚠ Only a full commit id is linked: anything else is not a commit.
    match view.commit.as_deref().filter(|c| is_sha(c)) {
        Some(c) => out.push_str(&format!(
            "Evidence: ledger commit [{}](https://github.com/{repo}/commit/{c}).\n\n",
            &c[..7]
        )),
        None => out
            .push_str("Evidence: the ledger holds it; this comment does not name its commit.\n\n"),
    }
    if let Outcome::Move { from, to, .. } = &d.outcome {
        out.push_str(&format!(
            "From `{}` to `{}`.\n\n",
            from.as_wire(),
            to.as_wire()
        ));
    }
    if let Some(line) = state {
        out.push_str(line);
        out.push_str("\n\n");
    }
    out
}

/// The attempt, or the first `shown` runs, as a table (spec §4.2), saying
/// how many runs it leaves out.
fn table(view: &DecisionView, shown: usize) -> String {
    if let Some(a) = &view.attempt {
        return format!(
            "| adapter | status | duration | tokens in | tokens out | cost |\n\
             |---|---|---|---|---|---|\n\
             | {} | {} | {} ms | {} | {} | {}.{:06} USD |\n",
            escape_capped(&a.adapter, CELL_LIMIT),
            a.status.as_wire(),
            a.duration_ms,
            a.tokens_in,
            a.tokens_out,
            a.cost_usd_micros / 1_000_000,
            a.cost_usd_micros % 1_000_000
        );
    }
    if view.rows.is_empty() {
        return "No gate ran for this decision.\n".into();
    }
    let mut out = String::from(
        "| for | gate | verdict | population | commit | duration |\n|---|---|---|---|---|---|\n",
    );
    for row in view.rows.iter().take(shown) {
        let r = &row.run;
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} ms |\n",
            escape_capped(&row.role, CELL_LIMIT),
            escape_capped(&row.gate, CELL_LIMIT),
            r.verdict.describe().0,
            r.verdict
                .population()
                .map_or_else(|| "—".to_string(), |p| p.to_string()),
            escape_capped(r.commit.get(..7).unwrap_or(&r.commit), CELL_LIMIT),
            r.duration_ms
        ));
    }
    let hidden = view.rows.len().saturating_sub(shown);
    if hidden > 0 {
        out.push_str(&format!(
            "\n… and {hidden} more runs: the ledger commit holds every one.\n"
        ));
    }
    out
}

/// The entries the decision names that were not found: the first
/// [`MISSING_SHOWN`], then how many more.
fn missing(view: &DecisionView) -> String {
    let mut out = String::new();
    for id in view.missing.iter().take(MISSING_SHOWN) {
        out.push_str(&format!(
            "\nNot found in the ledger: {}.\n",
            escape_capped(id.as_str(), CELL_LIMIT)
        ));
    }
    let more = view.missing.len().saturating_sub(MISSING_SHOWN);
    if more > 0 {
        out.push_str(&format!("\n… and {more} more entries not found.\n"));
    }
    out
}

/// Each excerpt `view` holds, with what it is about: a run's error detail
/// and output, an attempt's output. ⚠ Asked only for a private repository
/// (decision 2).
fn blocks(view: &DecisionView) -> Vec<Block> {
    let mut out = Vec::new();
    for row in &view.rows {
        let (label, detail) = row.run.verdict.describe();
        let mut text = String::new();
        if matches!(row.run.verdict, Verdict::Error { .. }) {
            text.push_str(&format!("error: {detail}\n"));
        }
        if let Some(e) = row.run.output_excerpt.as_deref() {
            text.push_str(e);
        }
        if !text.is_empty() {
            out.push(Block {
                title: escape_html_capped(
                    &format!("{} / {}: {label}", row.role, row.gate),
                    CELL_LIMIT,
                ),
                text,
            });
        }
    }
    if let Some(a) = &view.attempt
        && let Some(e) = a.output_excerpt.as_deref()
        && !e.is_empty()
    {
        out.push(Block {
            title: escape_html_capped(
                &format!("{}: {}", a.adapter, a.status.as_wire()),
                CELL_LIMIT,
            ),
            text: e.to_string(),
        });
    }
    out
}

/// A fence longer than any run of backticks in `text`, and never shorter
/// than three.
fn fence_for(text: &str) -> String {
    let (mut longest, mut run) = (0usize, 0usize);
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

/// One excerpt, folded (spec §4.2). Inside the fence nothing is markup:
/// mentions and references neither notify nor link.
fn block(title: &str, text: &str) -> String {
    let fence = fence_for(text);
    let end = if text.ends_with('\n') { "" } else { "\n" };
    format!("<details><summary>{title}</summary>\n\n{fence}\n{text}{end}{fence}\n\n</details>\n")
}

fn assemble(view: &DecisionView, repo: &str, state: Option<&str>, blocks: &[Block]) -> String {
    let mut out = head(view, repo, state);
    out.push_str(&table(view, view.rows.len()));
    out.push_str(&missing(view));
    for b in blocks {
        out.push('\n');
        out.push_str(&block(&b.title, &b.text));
    }
    out
}

/// The comment for `view` on `repo` (spec §4.2). `state`: the line a
/// comment posted as the decision is made adds; `None` for one recovered
/// later.
pub fn render(
    view: &DecisionView,
    repo: &str,
    visibility: Visibility,
    state: Option<&str>,
) -> String {
    // ⚠ Decision 2: excerpts only on a private repository.
    let blocks = match visibility {
        Visibility::Private => blocks(view),
        Visibility::NotPrivate => Vec::new(),
    };
    assemble(view, repo, state, &blocks)
}

/// Where a name is written: in markdown text (a table cell, the header),
/// or inside an HTML element (an excerpt's `<summary>`), where markdown is
/// not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    Markdown,
    Html,
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::at::At;
    use fl_core::decision::TransitionOutcome;
    use fl_core::ids::{GateId, ProjectId, RecordId, seq_iri};
    use fl_core::log::{AttemptStatus, PathsTouched};
    use fl_core::model::State;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn record() -> RecordId {
        RecordId(Iri::parse("https://github.com/acme/widgets/issues/1").unwrap())
    }

    fn decision(outcome: Outcome, rests_on: Vec<Iri>) -> Decision {
        Decision {
            id: seq_iri(90),
            at: At::from_unix_millis(1),
            record: record(),
            finding: None,
            outcome,
            rests_on,
        }
    }

    fn moved(allowed: bool) -> Outcome {
        Outcome::Move {
            from: State::Review,
            to: State::Done,
            transitions: vec![TransitionOutcome {
                transition: "launch".into(),
                passed: allowed,
            }],
            allowed,
        }
    }

    fn check(passed: bool) -> Outcome {
        Outcome::Check {
            transition: TransitionOutcome {
                transition: "launch".into(),
                passed,
            },
        }
    }

    fn reproduce(accepted: bool) -> Outcome {
        Outcome::Reproduce {
            gate: GateId(seq_iri(7)),
            accepted,
        }
    }

    fn verify(closed: bool) -> Outcome {
        Outcome::Verify {
            reproduction: GateId(seq_iri(7)),
            reproduction_passed: closed,
            regressions: vec![],
            closed,
        }
    }

    fn run(n: u64, verdict: Verdict, excerpt: Option<&str>) -> GateRun {
        GateRun {
            id: Some(seq_iri(n)),
            at: Some(At::from_unix_millis(n)),
            gate: GateId(seq_iri(7)),
            record: Some(record()),
            commit: "abcdef0123".into(),
            verdict,
            population: 3,
            output_excerpt: excerpt.map(str::to_string),
            duration_ms: 5,
            cost_usd_micros: 0,
        }
    }

    fn row(role: &str, gate: &str, run: GateRun) -> RunRow {
        RunRow {
            role: role.into(),
            gate: gate.into(),
            run,
        }
    }

    fn attempt(excerpt: Option<&str>) -> Attempt {
        Attempt {
            id: Some(seq_iri(50)),
            at: Some(At::from_unix_millis(2)),
            project: ProjectId(seq_iri(2)),
            record: record(),
            adapter: "claude".into(),
            status: AttemptStatus::Completed,
            duration_ms: 7,
            tokens_in: 11,
            tokens_out: 13,
            cost_usd_micros: 1_234_567,
            paths_touched: PathsTouched::Counted(2),
            output_excerpt: excerpt.map(str::to_string),
        }
    }

    fn view_of(outcome: Outcome, rows: Vec<RunRow>) -> DecisionView {
        DecisionView {
            decision: decision(
                outcome,
                rows.iter().filter_map(|r| r.run.id.clone()).collect(),
            ),
            by: "fake-user".into(),
            commit: Some(SHA.into()),
            rows,
            attempt: None,
            missing: vec![],
        }
    }

    #[test]
    fn escape_neutralises_markup_mentions_and_references() {
        assert_eq!(
            escape("a|b `c` @x #1 <!-- y\r\nz & *w* _u_ [l](t) !i ~s~ $m$ GH-1 gh-2 \\"),
            concat!(
                r"a\|b \`c\` @&#8203;x #&#8203;1 &lt;!-- y<br>z &amp; \*w\* \_u\_ ",
                r"\[l\]\(t\) !i \~s\~ \$m\$ GH-&#8203;1 gh-&#8203;2 \\"
            )
        );
        assert_eq!(escape("a > b"), "a &gt; b");
        assert_eq!(
            escape("plain-text.v1: ok/fine-G-H-"),
            "plain-text.v1: ok/fine-G-H-",
            "nothing else changes, and a `-` not after `GH` stays"
        );
    }

    // Inside `<summary>` markdown is not read: entities and the zero-width
    // spaces only, and a newline becomes a space.
    #[test]
    fn a_name_inside_an_html_element_gets_no_markdown_escapes() {
        assert_eq!(
            escape_html_capped("a|b_c @x #1 <i> GH-1 & \\\nz", CELL_LIMIT),
            r"a|b_c @&#8203;x #&#8203;1 &lt;i&gt; GH-&#8203;1 &amp; \ z"
        );
    }

    #[test]
    fn a_long_name_is_cut_on_a_whole_escape_and_says_so() {
        assert_eq!(escape_capped("short", CELL_LIMIT), "short");
        let cut = escape_capped(&"é".repeat(500), CELL_LIMIT);
        assert!(cut.len() <= CELL_LIMIT && cut.ends_with('…'), "{cut}");
        // Never half an escape: four `&` in ten bytes keep one whole `&amp;`.
        assert_eq!(escape_capped("&&&&", 10), "&amp;…");
        // Exactly at the limit: whole.
        assert_eq!(
            escape_capped(&"a".repeat(CELL_LIMIT), CELL_LIMIT),
            "a".repeat(CELL_LIMIT)
        );
        // A cut whose kept text and ellipsis fill the limit exactly.
        assert_eq!(escape_capped("abcdef", 4), "a…");
        assert_eq!(escape_html_capped("abcdef", 4), "a…");
    }

    // ⚠ The first marker line counts wherever it is: an edit that pushes it
    // down keeps the comment posted. Only the first: a marker quoted
    // further down marks nothing.
    #[test]
    fn the_first_marker_line_counts_wherever_it_is() {
        let id = seq_iri(90);
        let m = marker(&id).expect("an id fl writes");
        assert_eq!(m, format!("<!-- fl:decision {{\"id\":\"{id}\"}} -->"));
        assert_eq!(marked(&format!("{m}\n\nbody")), Some(id.clone()));
        assert_eq!(
            marked(&format!("{m}\r\nbody")),
            Some(id.clone()),
            "GitHub may send CRLF"
        );
        assert_eq!(
            marked(&format!("A maintainer's note.\n\n{m}\n\nbody")),
            Some(id.clone()),
            "moved down by an edit"
        );
        let other = marker(&seq_iri(91)).unwrap();
        assert_eq!(
            marked(&format!("{m}\n````\n{other}\n````")),
            Some(id.clone())
        );
        assert_eq!(
            marked(&format!("{m}\n{other}")),
            Some(id.clone()),
            "only the first"
        );
        // A broken marker-like line above the real one is passed over.
        assert_eq!(
            marked(&format!("<!-- fl:decision broken -->\n{m}")),
            Some(id.clone())
        );
        // fl's marker deleted: a marker quoted inside an excerpt's fence
        // marks nothing, however long the fence.
        assert_eq!(marked(&format!("A note.\n\n````\n{other}\n````\n")), None);
        assert_eq!(
            marked(&format!("````\n```\n{other}\n````\n")),
            None,
            "a shorter run does not close"
        );
        assert_eq!(
            marked(&format!("```\n``` x\n{other}\n```\n")),
            None,
            "nor a run with text after it"
        );
        // After a fence closes, a marker counts again.
        assert_eq!(marked(&format!("```\nx\n```\n{m}")), Some(id.clone()));
        for body in [
            "",
            "no marker at all",
            "<!-- fl:decision {} -->",
            "<!-- fl:decision {\"id\":\"not an iri\"} -->",
            "<!-- fl:decision {\"id\":1} -->",
            "<!-- fl:decision not json -->",
            "<!-- fl:decision {\"id\":\"urn:uuid:00000000-0000-7000-8000-00000000005a\"}",
            "<!-- fl:decision {\"id\":\"urn:x:a\"} -->",
        ] {
            assert_eq!(marked(body), None, "{body}");
        }
    }

    // ⚠ An id fl does not write never reaches the comment: no marker, and
    // nothing in the body opens or closes an HTML comment.
    #[test]
    fn a_hostile_decision_id_gets_no_marker_and_nothing_closes_a_comment() {
        let hostile = Iri::parse("urn:x:a--><b>").unwrap();
        assert!(!markable(&hostile));
        assert_eq!(marker(&hostile), None);
        let mut v = view_of(moved(true), vec![]);
        v.decision.id = hostile;
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(!body.contains("<!--") && !body.contains("-->"), "{body}");
        assert!(!body.contains("<b>"), "{body}");
        assert_eq!(marked(&body), None);
        assert!(markable(&seq_iri(90)));
        for bad in [
            "urn:uuid:not-a-uuid",
            "urn:uuid:0000000-0000-7000-8000-00000000005a0",
            "urn:uuid:gggggggg-0000-7000-8000-000000000000",
            "fl:gate/no-bug.v1",
            "https://example.com/x",
        ] {
            assert!(!markable(&Iri::parse(bad).unwrap()), "{bad}");
        }
    }

    // A run of backticks sets the fence; separate runs do not add up.
    #[test]
    fn the_fence_outruns_the_longest_run_of_backticks_not_their_sum() {
        assert_eq!(fence_for(""), "```");
        assert_eq!(fence_for("a `` b"), "```");
        assert_eq!(fence_for("a ``` b ```` c ``` d"), "`````");
        assert_eq!(fence_for("````"), "`````");
    }

    // ⚠ What a ledger line carries is escaped like any name: who decided,
    // the attempt's adapter, the run's commit, and an excerpt's title.
    #[test]
    fn who_decided_the_adapter_the_commit_and_a_title_are_escaped() {
        let hostile = "@x #1 <!-- |";
        let mut r = run(1, Verdict::from_predicate(false, 1), Some("out"));
        r.commit = "@x#<|`ab".into();
        let mut v = view_of(moved(true), vec![row("launch", hostile, r)]);
        v.by = hostile.into();
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(
            body.contains(r"Decided by @&#8203;x #&#8203;1 &lt;!-- \| at "),
            "{body}"
        );
        assert!(
            body.contains(r"| @&#8203;x#&#8203;&lt;\|\`a |"),
            "the commit's first seven: {body}"
        );
        assert!(
            body.contains("<summary>launch / @&#8203;x #&#8203;1 &lt;!-- |: FAIL</summary>"),
            "{body}"
        );
        assert_eq!(body.matches("<!--").count(), 1, "only the marker: {body}");
        let mut v = view_of(
            Outcome::Attempt {
                status: AttemptStatus::Completed,
            },
            vec![],
        );
        let mut a = attempt(Some("x"));
        a.adapter = hostile.into();
        v.attempt = Some(a);
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(
            body.contains(r"| @&#8203;x #&#8203;1 &lt;!-- \| | completed |"),
            "{body}"
        );
        assert!(
            body.contains("<summary>@&#8203;x #&#8203;1 &lt;!-- |: completed</summary>"),
            "{body}"
        );
        assert_eq!(body.matches("<!--").count(), 1, "only the marker: {body}");
    }

    #[test]
    fn only_a_full_commit_id_is_linked() {
        assert!(is_sha(SHA));
        assert!(!is_sha("0123456"), "too short");
        assert!(!is_sha(&"g".repeat(40)), "not hex");
        let mut v = view_of(moved(true), vec![]);
        v.commit = Some("unknown (GitHub's blame of `x` did not name it)".into());
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(
            body.contains("Evidence: the ledger holds it; this comment does not name its commit."),
            "{body}"
        );
        assert!(!body.contains("/commit/"), "{body}");
        v.commit = None;
        assert!(
            render(&v, "acme/widgets", Visibility::Private, None)
                .contains("does not name its commit")
        );
    }

    // Spec §4.2: the header, the move's states, the state line and the
    // table, exactly.
    #[test]
    fn a_moves_comment_says_what_was_decided_by_whom_and_where_its_evidence_is() {
        let v = view_of(
            moved(true),
            vec![row(
                "launch",
                "no-bug",
                run(1, Verdict::from_predicate(true, 3), Some("ok")),
            )],
        );
        let body = render(
            &v,
            "acme/widgets",
            Visibility::NotPrivate,
            Some("The state change completed: the record is now `done`."),
        );
        let expected = format!(
            concat!(
                "<!-- fl:decision {{\"id\":\"{id}\"}} -->\n\n",
                "### fl move: allowed\n\n",
                "Decided by fake-user at 1970-01-01T00:00:00.001Z. Evidence: ledger commit ",
                "[0123456](https://github.com/acme/widgets/commit/{sha}).\n\n",
                "From `review` to `done`.\n\n",
                "The state change completed: the record is now `done`.\n\n",
                "| for | gate | verdict | population | commit | duration |\n",
                "|---|---|---|---|---|---|\n",
                "| launch | no-bug | PASS | 3 | abcdef0 | 5 ms |\n",
            ),
            id = seq_iri(90),
            sha = SHA
        );
        assert_eq!(body, expected);
    }

    // Decision 11: a refused decision's comment says it was refused.
    #[test]
    fn the_heading_names_the_kind_and_the_outcome_refused_ones_included() {
        let cases = vec![
            (moved(true), "### fl move: allowed"),
            (moved(false), "### fl move: refused"),
            (check(true), "### fl check: passed"),
            (check(false), "### fl check: failed"),
            (reproduce(true), "### fl reproduce: accepted"),
            (reproduce(false), "### fl reproduce: refused"),
            (verify(true), "### fl verify: closed"),
            (verify(false), "### fl verify: not closed"),
            (
                Outcome::Attempt {
                    status: AttemptStatus::Timeout,
                },
                "### fl attempt: timeout",
            ),
        ];
        for (o, heading) in cases {
            let body = render(
                &view_of(o.clone(), vec![]),
                "acme/widgets",
                Visibility::Private,
                None,
            );
            assert!(
                body.contains(&format!("\n\n{heading}\n\n")),
                "{o:?}: {body}"
            );
        }
        let body = render(
            &view_of(moved(false), vec![]),
            "acme/widgets",
            Visibility::Private,
            None,
        );
        assert!(body.contains("No gate ran for this decision."), "{body}");
        assert!(
            !body.contains("The state change"),
            "no state line unless given: {body}"
        );
    }

    #[test]
    fn the_state_line_says_whether_the_state_change_completed() {
        let cases: Vec<(Outcome, bool, &str)> = vec![
            (
                moved(true),
                true,
                "The state change completed: the record is now `done`.",
            ),
            (
                moved(true),
                false,
                "The move was allowed, but its state change did not complete: the record may \
                 still be `review`.",
            ),
            (
                moved(false),
                true,
                "The move was refused: the record stays `review`.",
            ),
            (
                moved(false),
                false,
                "The move was refused: the record stays `review`.",
            ),
            (check(true), true, "A check changes no state."),
            (check(false), false, "A check changes no state."),
            (
                reproduce(true),
                true,
                "The state change completed: the finding records this gate as its reproduction.",
            ),
            (
                reproduce(true),
                false,
                "The reproduction was accepted, but its state change did not complete.",
            ),
            (
                reproduce(false),
                false,
                "The reproduction was refused: the finding is unchanged.",
            ),
            (
                verify(true),
                true,
                "The state change completed: the finding is closed.",
            ),
            (
                verify(true),
                false,
                "The finding passed its verification, but closing it did not complete.",
            ),
            (
                verify(false),
                true,
                "The finding stays open: the repair is not done.",
            ),
            (
                Outcome::Attempt {
                    status: AttemptStatus::Completed,
                },
                true,
                "An attempt changes no state.",
            ),
        ];
        for (outcome, completed, line) in cases {
            assert_eq!(
                state_line(&outcome, completed),
                line,
                "{outcome:?} {completed}"
            );
        }
    }

    #[test]
    fn an_attempts_comment_shows_its_adapter_status_duration_tokens_and_cost() {
        let mut v = view_of(
            Outcome::Attempt {
                status: AttemptStatus::Completed,
            },
            vec![],
        );
        v.decision.rests_on = vec![seq_iri(50)];
        v.attempt = Some(attempt(None));
        let body = render(&v, "acme/widgets", Visibility::NotPrivate, None);
        assert!(
            body.contains(concat!(
                "| adapter | status | duration | tokens in | tokens out | cost |\n",
                "|---|---|---|---|---|---|\n",
                "| claude | completed | 7 ms | 11 | 13 | 1.234567 USD |\n"
            )),
            "{body}"
        );
        assert!(!body.contains("No gate ran"), "{body}");
    }

    #[test]
    fn a_name_in_the_table_is_escaped_and_cut_and_an_error_has_no_population() {
        let long = format!("a|b @x {}", "z".repeat(10_000));
        let v = view_of(
            moved(true),
            vec![row(&long, &long, run(1, Verdict::error("broke"), None))],
        );
        let body = render(&v, "acme/widgets", Visibility::NotPrivate, None);
        let line = body
            .lines()
            .find(|l| l.starts_with(r"| a\|b"))
            .expect("the row");
        assert!(line.contains("@&#8203;x"), "{line}");
        assert!(line.len() < 3 * CELL_LIMIT, "{} bytes", line.len());
        assert!(line.contains("| ERROR | — |"), "{line}");
    }

    #[test]
    fn a_table_shown_in_part_and_entries_not_found_say_how_many_are_left_out() {
        let rows: Vec<RunRow> = (1..=3)
            .map(|n| {
                row(
                    "launch",
                    "g",
                    run(n, Verdict::from_predicate(true, 1), None),
                )
            })
            .collect();
        let mut v = view_of(moved(true), rows);
        let shown = table(&v, 1);
        assert_eq!(shown.matches("| launch | g |").count(), 1, "{shown}");
        assert!(
            shown.contains("… and 2 more runs: the ledger commit holds every one."),
            "{shown}"
        );
        assert!(!table(&v, 3).contains("more runs"));
        v.missing = (100..100 + MISSING_SHOWN as u64 + 2).map(seq_iri).collect();
        let named = missing(&v);
        assert_eq!(
            named.matches("Not found in the ledger: ").count(),
            MISSING_SHOWN,
            "{named}"
        );
        assert!(named.contains("… and 2 more entries not found."), "{named}");
        v.missing.truncate(1);
        assert!(!missing(&v).contains("more entries"));
    }

    // ⚠ Spec §4.2: excerpts only on a private repository, each in a fence
    // longer than any run of backticks in it — so nothing in it closes the
    // block, opens a tag, or forges a marker.
    #[test]
    fn excerpts_show_only_on_a_private_repository_each_in_a_fence_longer_than_its_backticks() {
        let text = "line ```` four\n</details>\n<!-- fl:decision \
                    {\"id\":\"urn:uuid:00000000-0000-7000-8000-000000000001\"} -->\n@someone #1";
        let v = view_of(
            moved(true),
            vec![row(
                "launch",
                "no-bug",
                run(1, Verdict::from_predicate(false, 3), Some(text)),
            )],
        );
        let private = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(
            private.contains(&format!(
                "<details><summary>launch / no-bug: FAIL</summary>\n\n`````\n{text}\n`````\n\n\
                 </details>\n"
            )),
            "{private}"
        );
        assert_eq!(
            marked(&private),
            Some(seq_iri(90)),
            "only fl's own marker, the first, marks it"
        );
        let public = render(&v, "acme/widgets", Visibility::NotPrivate, None);
        assert!(!public.contains("<details>"), "{public}");
        assert!(!public.contains("four"), "{public}");
    }

    #[test]
    fn an_errors_detail_and_an_attempts_excerpt_show_and_an_empty_one_does_not() {
        let v = view_of(
            moved(true),
            vec![
                row(
                    "launch",
                    "a",
                    run(1, Verdict::error("spawn failed"), Some("")),
                ),
                row(
                    "launch",
                    "b",
                    run(2, Verdict::from_predicate(true, 1), Some("")),
                ),
                row(
                    "launch",
                    "c",
                    run(3, Verdict::from_predicate(true, 1), None),
                ),
            ],
        );
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(
            body.contains("<summary>launch / a: ERROR</summary>\n\n```\nerror: spawn failed\n```"),
            "{body}"
        );
        assert_eq!(body.matches("<details>").count(), 1, "{body}");

        let mut v = view_of(
            Outcome::Attempt {
                status: AttemptStatus::Completed,
            },
            vec![],
        );
        v.attempt = Some(attempt(Some("did it")));
        let body = render(&v, "acme/widgets", Visibility::Private, None);
        assert!(
            body.contains("<summary>claude: completed</summary>\n\n```\ndid it\n```"),
            "{body}"
        );
        v.attempt = Some(attempt(Some("")));
        assert!(!render(&v, "acme/widgets", Visibility::Private, None).contains("<details>"));
    }
}
