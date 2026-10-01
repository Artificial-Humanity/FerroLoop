//! The `fl/ledger` branch, format 1 (GitHub ledger spec §3.1): where each
//! entry goes, how a line is written and read, and when a segment closes.
//! Pure: no network, no clock.

use fl_core::at::At;
use fl_core::decision::Decision;
use fl_core::iri::Iri;
use fl_core::log::{Attempt, GateRun};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The branch: an orphan, with no `.github/`, so an append never starts a
/// workflow.
pub const BRANCH: &str = "fl/ledger";
/// The file that names the layout's version.
pub const FORMAT_FILE: &str = "format";
/// The one version this fl reads (spec §3.1: "a new field in any entry
/// means a new format").
pub const FORMAT: &str = "1";
pub const README_FILE: &str = "README.md";
pub const QUARANTINE_FILE: &str = "quarantine.jsonl";
/// A segment closes when the next line would take it past this many
/// bytes; a longer line gets a segment of its own (spec §3.1: 256 KB).
pub const SEGMENT_LIMIT: usize = 256 * 1024;

/// What the branch says about itself. No machine names.
pub const README: &str = "# fl ledger\n\n\
This branch is the shared ledger of fl (FerroLoop): the gate runs, attempts and decisions \
recorded against this repository's issues. fl only ever appends to it. Do not edit it by hand: \
every machine that reads it checks that each file only grows, and refuses a ledger that \
changed.\n\n\
- `format`: the layout's version.\n\
- `runs/<key>/<n>.jsonl`: gate runs, one directory per gate.\n\
- `attempts/<key>/<n>.jsonl`: attempts, one directory per project.\n\
- `decisions/<key>/<n>.jsonl`: decisions, one directory per record or finding.\n\
- `quarantine.jsonl`: lines readers skip, and why.\n\n\
A key is the first 32 hexadecimal digits of the SHA-256 of the item's IRI.\n";

/// The three kinds of directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    Runs,
    Attempts,
    Decisions,
}

impl Area {
    pub fn name(self) -> &'static str {
        match self {
            Area::Runs => "runs",
            Area::Attempts => "attempts",
            Area::Decisions => "decisions",
        }
    }

    fn from_name(name: &str) -> Option<Area> {
        [Area::Runs, Area::Attempts, Area::Decisions]
            .into_iter()
            .find(|a| a.name() == name)
    }
}

/// The first 32 hexadecimal digits of the SHA-256 of `iri`.
pub fn key(iri: &Iri) -> String {
    Sha256::digest(iri.as_str().as_bytes())
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `subject`'s directory in `area`: `runs/<key>`.
pub fn dir(area: Area, subject: &Iri) -> String {
    format!("{}/{}", area.name(), key(subject))
}

pub fn segment_path(dir: &str, n: u64) -> String {
    format!("{dir}/{n}.jsonl")
}

/// `n` from a segment's file name `<n>.jsonl`: from 1, with no leading
/// zero, so each segment has one spelling.
pub fn segment_number(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(".jsonl")?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A segment's area, directory and number, from its path; `None` for any
/// path fl does not write as a segment.
pub fn parse_segment_path(path: &str) -> Option<(Area, String, u64)> {
    let mut parts = path.split('/');
    let (area, k, file) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let area = Area::from_name(area)?;
    if k.len() != 32
        || !k
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Some((area, format!("{}/{k}", area.name()), segment_number(file)?))
}

/// What a decision is filed under: its finding when it has one, else its
/// record (spec §3.1; ruling 7).
pub fn decision_subject(d: &Decision) -> &Iri {
    match &d.finding {
        Some(f) => f.iri(),
        None => d.record.iri(),
    }
}

/// One line of a segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Run(GateRun),
    Attempt(Attempt),
    Decision(Decision),
}

impl Line {
    pub fn area(&self) -> Area {
        match self {
            Line::Run(_) => Area::Runs,
            Line::Attempt(_) => Area::Attempts,
            Line::Decision(_) => Area::Decisions,
        }
    }

    /// What its directory is keyed by: the run's gate, the attempt's
    /// project, the decision's finding or record.
    pub fn subject(&self) -> &Iri {
        match self {
            Line::Run(r) => r.gate.iri(),
            Line::Attempt(a) => a.project.iri(),
            Line::Decision(d) => decision_subject(d),
        }
    }

    pub fn id(&self) -> Option<&Iri> {
        match self {
            Line::Run(r) => r.id.as_ref(),
            Line::Attempt(a) => a.id.as_ref(),
            Line::Decision(d) => Some(&d.id),
        }
    }

    pub fn dir(&self) -> String {
        dir(self.area(), self.subject())
    }

    fn value(&self) -> Value {
        match self {
            Line::Run(r) => serde_json::to_value(r),
            Line::Attempt(a) => serde_json::to_value(a),
            Line::Decision(d) => serde_json::to_value(d),
        }
        .expect("an entry always serializes")
    }

    /// The entry and `by`, as one line of JSON with no newline.
    pub fn encode(&self, by: &str) -> String {
        let mut v = self.value();
        if let Value::Object(map) = &mut v {
            map.insert("by".into(), Value::String(by.to_string()));
        }
        v.to_string()
    }
}

/// One line of an `area` segment, and who wrote it.
///
/// ⚠ Strict (ruling 11): the line must be byte-for-byte what the entry's
/// own [`Line::encode`] would write, not merely structurally equal to it.
/// A field fl does not write, a value spelled as fl never spells it, a
/// duplicate key (JSON lets the last one win, silently dropping whatever
/// came before it) or a reordered one (parses to the same `Value`, but is
/// not fl's own bytes) is unreadable — a new field means a new format
/// (spec §3.1). A published run or attempt always carries `id` and `at`.
pub fn decode(area: Area, text: &str) -> Result<(Line, String), String> {
    let mut v: Value = serde_json::from_str(text).map_err(|e| format!("it is not JSON ({e})"))?;
    let Some(map) = v.as_object_mut() else {
        return Err("it is not a JSON object".into());
    };
    let by = match map.remove("by") {
        Some(Value::String(s)) if !s.trim().is_empty() => s,
        Some(_) => return Err("its `by` is not a name".into()),
        None => return Err("it names no writer (`by`)".into()),
    };
    let line = match area {
        Area::Runs => serde_json::from_value(v.clone())
            .map(Line::Run)
            .map_err(|e| format!("it is not a gate run ({e})"))?,
        Area::Attempts => serde_json::from_value(v.clone())
            .map(Line::Attempt)
            .map_err(|e| format!("it is not an attempt ({e})"))?,
        Area::Decisions => serde_json::from_value(v.clone())
            .map(Line::Decision)
            .map_err(|e| format!("it is not a decision ({e})"))?,
    };
    if line.encode(&by) != text {
        return Err(
            "it is not byte-for-byte what fl itself would write: a field fl does not write, a \
             value spelled as fl never spells it, or a duplicate or reordered key"
                .into(),
        );
    }
    let stamped = match &line {
        Line::Run(r) => r.id.is_some() && r.at.is_some(),
        Line::Attempt(a) => a.id.is_some() && a.at.is_some(),
        Line::Decision(_) => true,
    };
    if !stamped {
        return Err("it has no `id` or no `at`, which every published entry carries".into());
    }
    Ok((line, by))
}

/// The lines of a segment, numbered from 1.
///
/// ⚠ Every line fl writes ends with a newline: an empty line, or text after
/// the last newline (a line cut short), comes back as `Err` in its place,
/// with its number, for the reader to report.
pub fn lines(text: &str) -> Vec<(u64, Result<&str, &'static str>)> {
    let mut out = Vec::new();
    if text.is_empty() {
        return out;
    }
    let mut pieces: Vec<&str> = text.split('\n').collect();
    let tail = pieces.pop().unwrap_or("");
    for (i, p) in pieces.iter().enumerate() {
        let line = if p.is_empty() {
            Err("it is empty")
        } else {
            Ok(*p)
        };
        out.push(((i + 1) as u64, line));
    }
    if !tail.is_empty() {
        out.push((
            (out.len() + 1) as u64,
            Err("it does not end with a newline: it was cut short"),
        ));
    }
    out
}

/// Whether `is` could be `was` after only growth (spec §3.5 checks 3 and
/// 4): `was` is empty or ends with a newline, and `is` starts with `was`
/// exactly. Shared by every reader (`ledger::read::grown`) and by
/// `ledger::verify`'s `only_adds`, so the rule can never drift between
/// them.
///
/// ⚠ A `was` that does NOT end in a newline is refused even when `is`
/// starts with it: completing a line `was` broke off mid-way through would
/// otherwise trivially "start with" the cut copy, whatever the completion
/// said — a false growth that would let a tampered commit verify clean.
///
/// ⚠ Bytes, not text: `verify` compares raw blob bytes, never a decoded
/// string — two different invalid-UTF-8 byte sequences must never compare
/// equal just because a lossy decode of both collapses to the same
/// replacement characters.
pub fn grows_only(was: &[u8], is: &[u8]) -> bool {
    (was.is_empty() || was.ends_with(b"\n")) && is.starts_with(was)
}

/// The files to write so `new` lines join a directory whose segments are
/// `segments` — (number, text), oldest first. Only the last segment grows;
/// it closes when the next line would take it past [`SEGMENT_LIMIT`].
/// Returns (number, its whole new text) for each segment written; nothing
/// to add writes nothing.
pub fn plan_append(segments: &[(u64, String)], new: &[String]) -> Vec<(u64, String)> {
    let (mut n, mut text) = match segments.last() {
        Some((n, t)) => (*n, t.clone()),
        None => (1, String::new()),
    };
    let mut changed = false;
    let mut out = Vec::new();
    for line in new {
        debug_assert!(
            !line.contains('\n'),
            "a new line never itself contains a newline"
        );
        if !text.is_empty() && text.len() + line.len() + 1 > SEGMENT_LIMIT {
            if changed {
                out.push((n, std::mem::take(&mut text)));
            } else {
                text.clear();
            }
            n += 1;
        }
        text.push_str(line);
        text.push('\n');
        changed = true;
    }
    if changed {
        out.push((n, text));
    }
    out
}

/// One line of `quarantine.jsonl` (spec §3.6): a segment's line readers
/// skip, who decided, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantineLine {
    /// Minted by the caller, so a retried quarantine adds one line.
    pub id: Iri,
    pub at: At,
    /// The segment, as a path on the branch.
    pub file: String,
    /// From 1.
    pub line: u64,
    /// The person who decided, as given with `--by`.
    pub quarantined_by: String,
    pub reason: String,
    /// The GitHub identity that wrote this line.
    pub by: String,
}

impl QuarantineLine {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("a quarantine line always serializes")
    }

    pub fn decode(text: &str) -> Result<QuarantineLine, String> {
        let q: QuarantineLine =
            serde_json::from_str(text).map_err(|e| format!("it is not a quarantine line ({e})"))?;
        if q.by.trim().is_empty() || q.quarantined_by.trim().is_empty() {
            return Err("it names no one".into());
        }
        if q.line == 0 {
            return Err("its `line` is not numbered from 1".into());
        }
        if parse_segment_path(&q.file).is_none() {
            return Err("its `file` is not a segment path".into());
        }
        Ok(q)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::conformance::{sample_attempt, sample_decision, sample_record_run};
    use fl_core::decision::Outcome;
    use fl_core::ids::{FindingId, GateId, ProjectId, RecordId, seq_iri};
    use fl_core::log::PathsTouched;
    use fl_core::model::State;

    fn record() -> RecordId {
        RecordId(Iri::parse("https://github.com/acme/widgets/issues/1").unwrap())
    }

    fn seg(lines: &[&str]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    #[test]
    fn a_key_is_the_first_32_hex_digits_of_the_sha_256_of_the_iri() {
        let iri = seq_iri(1);
        let full: String = Sha256::digest(iri.as_str().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(key(&iri), &full[..32]);
        assert_ne!(key(&iri), key(&seq_iri(2)));
    }

    // Spec §3.1 and ruling 7.
    #[test]
    fn each_entry_is_filed_under_its_gate_project_finding_or_record() {
        let g = GateId(seq_iri(7));
        let p = ProjectId(seq_iri(8));
        let run = sample_record_run(1, &g, Some(&record()));
        assert_eq!(Line::Run(run).dir(), format!("runs/{}", key(g.iri())));
        let a = sample_attempt(2, &p, &record());
        assert_eq!(Line::Attempt(a).dir(), format!("attempts/{}", key(p.iri())));
        let mut d = sample_decision(3, &record(), vec![]);
        assert_eq!(
            Line::Decision(d.clone()).dir(),
            format!("decisions/{}", key(record().iri()))
        );
        let f = FindingId(Iri::parse("https://github.com/acme/widgets/issues/2").unwrap());
        d.finding = Some(f.clone());
        assert_eq!(
            Line::Decision(d).dir(),
            format!("decisions/{}", key(f.iri())),
            "a finding's decision is filed under the finding"
        );
    }

    #[test]
    fn segment_names_are_canonical_numbers_from_one() {
        assert_eq!(segment_number("1.jsonl"), Some(1));
        assert_eq!(segment_number("12.jsonl"), Some(12));
        for bad in [
            "0.jsonl",
            "01.jsonl",
            "1.json",
            "a.jsonl",
            ".jsonl",
            "-1.jsonl",
            // `u64::from_str` accepts a leading `+` (unlike `-`, which it
            // already refuses on its own): only the all-digits guard
            // catches this one.
            "+1.jsonl",
            "1.jsonl.bak",
        ] {
            assert_eq!(segment_number(bad), None, "{bad}");
        }
        assert_eq!(segment_path("runs/k", 3), "runs/k/3.jsonl");
    }

    #[test]
    fn a_segment_path_is_an_area_a_key_and_a_number() {
        let k = key(&seq_iri(1));
        assert_eq!(
            parse_segment_path(&format!("runs/{k}/3.jsonl")),
            Some((Area::Runs, format!("runs/{k}"), 3))
        );
        assert_eq!(
            parse_segment_path(&format!("decisions/{k}/1.jsonl")),
            Some((Area::Decisions, format!("decisions/{k}"), 1))
        );
        for bad in [
            format!("runs/{k}"),
            format!("runs/{k}/0.jsonl"),
            format!("other/{k}/1.jsonl"),
            "runs/abc/1.jsonl".to_string(),
            format!("runs/{}/1.jsonl", k.to_uppercase()),
            format!("runs/{k}/x/1.jsonl"),
            // A valid area, key and segment number, with one extra
            // component after: fails only the trailing-component guard,
            // not any earlier check.
            format!("runs/{k}/1.jsonl/x"),
            format!("runs/{k}/+1.jsonl"),
            QUARANTINE_FILE.to_string(),
            FORMAT_FILE.to_string(),
        ] {
            assert_eq!(parse_segment_path(&bad), None, "{bad}");
        }
    }

    #[test]
    fn every_kind_of_line_round_trips_with_its_writer() {
        let g = GateId(seq_iri(7));
        let p = ProjectId(seq_iri(8));
        let mut withheld = sample_attempt(2, &p, &record());
        withheld.output_excerpt = None;
        withheld.paths_touched = PathsTouched::Counted(4);
        let mut refused = sample_decision(3, &record(), vec![]);
        refused.outcome = Outcome::Move {
            from: State::Review,
            to: State::Done,
            transitions: vec![],
            allowed: false,
        };
        for (area, line) in [
            (
                Area::Runs,
                Line::Run(sample_record_run(1, &g, Some(&record()))),
            ),
            (
                Area::Attempts,
                Line::Attempt(sample_attempt(2, &p, &record())),
            ),
            (Area::Attempts, Line::Attempt(withheld)),
            (Area::Decisions, Line::Decision(refused)),
        ] {
            let text = line.encode("fake-user");
            assert!(!text.contains('\n'), "{text}");
            assert_eq!(decode(area, &text), Ok((line, "fake-user".to_string())));
        }
    }

    // Ruling 11 and spec §3.1: a new field means a new format, so a line
    // with anything fl does not write is unreadable.
    #[test]
    fn a_line_is_read_strictly() {
        let g = GateId(seq_iri(7));
        let p = ProjectId(seq_iri(8));
        let good = Line::Run(sample_record_run(1, &g, Some(&record()))).encode("fake-user");
        let edit = |f: &dyn Fn(&mut Value)| {
            let mut v: Value = serde_json::from_str(&good).unwrap();
            f(&mut v);
            v.to_string()
        };
        for (case, text) in [
            (
                "an unknown field",
                edit(&|v| v["host"] = Value::String("somewhere".into())),
            ),
            (
                "no writer",
                edit(&|v| {
                    v.as_object_mut().unwrap().remove("by");
                }),
            ),
            (
                "an empty writer",
                edit(&|v| v["by"] = Value::String(" ".into())),
            ),
            ("no id", edit(&|v| v["id"] = Value::Null)),
            ("no time", edit(&|v| v["at"] = Value::Null)),
            (
                "a count spelled as a float",
                edit(&|v| v["population"] = serde_json::json!(1.0)),
            ),
            ("not JSON", "{".to_string()),
            ("not an object", "[]".to_string()),
        ] {
            assert!(decode(Area::Runs, &text).is_err(), "{case}: {text}");
        }
        assert!(
            decode(Area::Attempts, &good).is_err(),
            "a run is not an attempt"
        );
        assert!(decode(Area::Runs, &good).is_ok());

        // The `stamped` check has one arm per kind; a run's `id`/`at`
        // cases above never reach the attempt arm.
        let good_attempt = Line::Attempt(sample_attempt(2, &p, &record())).encode("fake-user");
        let edit_attempt = |f: &dyn Fn(&mut Value)| {
            let mut v: Value = serde_json::from_str(&good_attempt).unwrap();
            f(&mut v);
            v.to_string()
        };
        assert!(
            decode(Area::Attempts, &edit_attempt(&|v| v["id"] = Value::Null)).is_err(),
            "an attempt with no id"
        );
        assert!(
            decode(Area::Attempts, &edit_attempt(&|v| v["at"] = Value::Null)).is_err(),
            "an attempt with no time"
        );
        assert!(decode(Area::Attempts, &good_attempt).is_ok());

        // Ruling: the round trip is byte-for-byte against the raw line,
        // not merely structural equality of the parsed `Value` — JSON
        // itself lets a duplicate key's last occurrence win, so a hidden
        // value ahead of the real one would otherwise ride along
        // undetected. Built by hand: a parsed `Value` can never hold a
        // duplicate key, so `edit` cannot produce this case.
        let duplicate_key = good.replacen(
            "\"output_excerpt\":\"run 1\"",
            "\"output_excerpt\":\"SECRET\",\"output_excerpt\":\"run 1\"",
            1,
        );
        assert_ne!(duplicate_key, good);
        assert!(
            decode(Area::Runs, &duplicate_key).is_err(),
            "a hidden duplicate key"
        );

        // Ruling: reordered keys parse to the same `Value` as the
        // canonical line, but are not the bytes fl itself writes, so they
        // are refused too.
        let reordered = {
            let v: Value = serde_json::from_str(&good).unwrap();
            let mut entries: Vec<(String, Value)> = v
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            entries.reverse();
            let body: String = entries
                .iter()
                .map(|(k, v)| format!("{}:{v}", serde_json::to_string(k).unwrap()))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        };
        assert_ne!(reordered, good);
        assert!(
            decode(Area::Runs, &reordered).is_err(),
            "keys in any order but fl's own"
        );
    }

    #[test]
    fn lines_are_numbered_from_one_and_a_cut_or_empty_line_is_flagged_in_place() {
        assert!(lines("").is_empty());
        assert_eq!(lines("a\nb\n"), vec![(1, Ok("a")), (2, Ok("b"))]);
        assert_eq!(
            lines("a\n\nb\n"),
            vec![(1, Ok("a")), (2, Err("it is empty")), (3, Ok("b"))]
        );
        let cut = lines("a\nb");
        assert_eq!(cut[0], (1, Ok("a")));
        assert_eq!(cut[1].0, 2);
        assert!(cut[1].1.is_err(), "{cut:?}");
    }

    // Spec §3.5 checks 3 and 4, the shared rule: ordinary growth, a plain
    // rewrite, and the specific hole this closes — completing (or merely
    // repeating) a line `was` itself left cut short must never read as
    // growth just because the result trivially "starts with" it.
    #[test]
    fn grows_only_refuses_completing_a_cut_line() {
        assert!(
            grows_only(b"", b"a\n"),
            "an empty baseline may grow into anything"
        );
        assert!(grows_only(b"a\n", b"a\nb\n"), "ordinary growth");
        assert!(
            grows_only(b"a\n", b"a\n"),
            "unchanged is its own, trivial growth"
        );
        assert!(!grows_only(b"a\n", b"b\n"), "a rewrite, not a growth");
        assert!(
            !grows_only(b"a\nnot terminated", b"a\nnot terminated, now finished\n"),
            "completing a cut line is not growth, however the completion starts"
        );
        assert!(
            !grows_only(b"a\nnot terminated", b"a\nnot terminated"),
            "an unchanged cut copy is not growth either: `was` must end cleanly"
        );
        assert!(
            !grows_only(&[0xffu8, b'\n'], &[0xfeu8, b'\n']),
            "two different invalid-UTF-8 byte sequences are never equal, lossy decoding aside"
        );
    }

    #[test]
    fn new_lines_grow_the_last_segment_and_a_closed_one_is_never_rewritten() {
        let segments = vec![(1, seg(&["a"])), (2, seg(&["b"]))];
        assert_eq!(
            plan_append(&segments, &["c".into(), "d".into()]),
            vec![(2, seg(&["b", "c", "d"]))]
        );
        assert_eq!(
            plan_append(&[], &["a".into()]),
            vec![(1, seg(&["a"]))],
            "an empty directory starts at 1"
        );
        assert!(
            plan_append(&segments, &[]).is_empty(),
            "nothing to add writes nothing"
        );
    }

    // Spec §3.1: segments roll over at 256 KB.
    #[test]
    fn a_segment_closes_when_the_next_line_would_pass_the_limit() {
        // The segment holds SEGMENT_LIMIT - 9 bytes: a line of 8 and its
        // newline fill it exactly; a line of 9 does not fit.
        let fill = "x".repeat(SEGMENT_LIMIT - 10);
        let segments = vec![(1, format!("{fill}\n"))];
        let eight = "y".repeat(8);
        assert_eq!(
            plan_append(&segments, std::slice::from_ref(&eight)),
            vec![(1, format!("{fill}\n{eight}\n"))]
        );
        let nine = "z".repeat(9);
        assert_eq!(
            plan_append(&segments, std::slice::from_ref(&nine)),
            vec![(2, format!("{nine}\n"))]
        );
        let out = plan_append(&segments, &[eight.clone(), nine.clone()]);
        assert_eq!(
            out,
            vec![(1, format!("{fill}\n{eight}\n")), (2, format!("{nine}\n"))]
        );
        assert!(out.iter().all(|(_, t)| t.len() <= SEGMENT_LIMIT));
    }

    #[test]
    fn a_line_longer_than_a_segment_gets_a_segment_of_its_own() {
        let huge = "h".repeat(SEGMENT_LIMIT + 1);
        assert_eq!(
            plan_append(&[(1, seg(&["a"]))], &[huge.clone(), "b".into()]),
            vec![(2, format!("{huge}\n")), (3, seg(&["b"]))]
        );
        assert_eq!(
            plan_append(&[], std::slice::from_ref(&huge)),
            vec![(1, format!("{huge}\n"))],
            "an empty segment takes it"
        );
    }

    #[test]
    fn a_quarantine_line_round_trips_and_is_read_strictly() {
        let q = QuarantineLine {
            id: seq_iri(5),
            at: At::from_unix_millis(5),
            file: segment_path(&dir(Area::Runs, &seq_iri(1)), 1),
            line: 3,
            quarantined_by: "Ada".into(),
            reason: "a hand edit".into(),
            by: "fake-user".into(),
        };
        assert_eq!(QuarantineLine::decode(&q.encode()), Ok(q.clone()));
        let mut v: Value = serde_json::from_str(&q.encode()).unwrap();
        v["extra"] = Value::Bool(true);
        assert!(QuarantineLine::decode(&v.to_string()).is_err());
        let mut nobody = q.clone();
        nobody.quarantined_by = " ".into();
        assert!(QuarantineLine::decode(&nobody.encode()).is_err());
        let mut no_writer = q.clone();
        no_writer.by = " ".into();
        assert!(
            QuarantineLine::decode(&no_writer.encode()).is_err(),
            "a blank `by` names no one either"
        );
        let mut zero_line = q.clone();
        zero_line.line = 0;
        assert!(
            QuarantineLine::decode(&zero_line.encode()).is_err(),
            "a line number is from 1"
        );
        let mut bad_file = q.clone();
        bad_file.file = "not-a-segment-path".into();
        assert!(
            QuarantineLine::decode(&bad_file.encode()).is_err(),
            "its file is not a segment path"
        );
    }
}
