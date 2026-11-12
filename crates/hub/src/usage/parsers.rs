use fl_watch::usage::{ParserState, UsageBucket, UsageVendor};

#[derive(Debug, PartialEq, Eq)]
pub enum ParseOutcome {
    Ignore,
    Observation(UsageBucket),
    IncompleteObservation(UsageBucket),
    Correction { old: UsageBucket, new: UsageBucket },
    Incomplete,
    Malformed,
}

fn identifier(v: Option<&serde_json::Value>) -> Option<String> {
    let s = v?.as_str()?;
    (s.len() <= 256
        && !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b)))
    .then(|| s.to_owned())
}

fn timestamp(v: Option<&serde_json::Value>) -> Option<u64> {
    v.and_then(crate::discovery::parse_timestamp_value_ms)
        .map(|t| t / 3_600_000 * 3_600_000)
}
fn token(v: &serde_json::Value, key: &str) -> Option<u64> {
    v.get(key).and_then(serde_json::Value::as_u64)
}
fn counts(
    v: &serde_json::Value,
    input: &str,
    output: &str,
    read: &str,
    write: &str,
) -> fl_watch::usage::TokenCounts {
    fl_watch::usage::TokenCounts {
        input: token(v, input),
        output: token(v, output),
        cache_read: token(v, read),
        cache_write: token(v, write),
    }
}
fn difference(
    now: &fl_watch::usage::TokenCounts,
    prev: &fl_watch::usage::TokenCounts,
) -> Option<fl_watch::usage::TokenCounts> {
    fn delta(a: Option<u64>, b: Option<u64>) -> Option<Option<u64>> {
        match (a, b) {
            (Some(a), Some(b)) => a.checked_sub(b).map(Some),
            (None, None) => Some(None),
            _ => None,
        }
    }
    Some(fl_watch::usage::TokenCounts {
        input: delta(now.input, prev.input)?,
        output: delta(now.output, prev.output)?,
        cache_read: delta(now.cache_read, prev.cache_read)?,
        cache_write: delta(now.cache_write, prev.cache_write)?,
    })
}
fn empty(c: &fl_watch::usage::TokenCounts) -> bool {
    [c.input, c.output, c.cache_read, c.cache_write]
        .into_iter()
        .all(|x| x.unwrap_or(0) == 0)
}

fn bucket(
    model: Option<String>,
    hour_ms: Option<u64>,
    counts: fl_watch::usage::TokenCounts,
    turns: u64,
) -> UsageBucket {
    let unavailable = fl_watch::usage::CategoryUnavailable {
        input: counts.input.is_none(),
        output: counts.output.is_none(),
        cache_read: counts.cache_read.is_none(),
        cache_write: counts.cache_write.is_none(),
    };
    UsageBucket {
        model,
        hour_ms,
        counts,
        turns,
        turns_unavailable: true,
        missing: fl_watch::usage::MissingCategoryCounts {
            input: u64::from(unavailable.input),
            output: u64::from(unavailable.output),
            cache_read: u64::from(unavailable.cache_read),
            cache_write: u64::from(unavailable.cache_write),
        },
        unavailable,
        observations: 1,
    }
}

// A fixed-size conservative membership filter has no false negatives. A false
// positive drops an observation and marks coverage incomplete rather than
// allowing a replay outside the bounded correction window to inflate totals.
fn seen_before(bits: &mut Vec<u64>, key: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    const WORDS: usize = 8192;
    if bits.is_empty() {
        bits.resize(WORDS, 0);
    }
    let digest = Sha256::digest(key);
    let mut was_seen = true;
    for chunk in digest.as_chunks::<8>().0 {
        let n = u64::from_le_bytes(*chunk) as usize % (WORDS * 64);
        let mask = 1u64 << (n % 64);
        was_seen &= bits[n / 64] & mask != 0;
        bits[n / 64] |= mask;
    }
    was_seen
}

pub fn parse_line(vendor: UsageVendor, bytes: &[u8], state: &mut ParserState) -> ParseOutcome {
    use serde_json::Value;
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
        return ParseOutcome::Malformed;
    };
    if vendor == UsageVendor::Grok {
        return ParseOutcome::Incomplete;
    }
    let hour = timestamp(v.get("timestamp").or_else(|| v.get("created_at")));
    let kind = v.get("type").and_then(Value::as_str);
    match vendor {
        UsageVendor::Antigravity => {
            if kind == Some("GENERIC")
                && let Some(id) = v
                    .get("content")
                    .and_then(Value::as_str)
                    .and_then(|content| {
                        content
                            .split_once("Message sent to \"")
                            .map(|(_, suffix)| suffix)
                    })
                    .and_then(|suffix| suffix.split('\"').next())
                    .and_then(|id| uuid::Uuid::parse_str(id).ok())
            {
                let id = id.to_string();
                if !state.antigravity_parents.contains(&id) {
                    if state.antigravity_parents.len() >= 8 {
                        return ParseOutcome::Incomplete;
                    }
                    state.antigravity_parents.push(id);
                }
            }
            if state.antigravity_unsafe {
                match (
                    v.get("step_index").and_then(Value::as_u64),
                    state.antigravity_last_step,
                ) {
                    (Some(step), Some(previous)) if step > previous => {
                        state.antigravity_unsafe = false
                    }
                    _ => return ParseOutcome::Incomplete,
                }
            }
            if let Some(step) = v.get("step_index").and_then(Value::as_u64) {
                if let Some(prev) = state.antigravity_last_step
                    && (step < prev
                        || (step <= 1 && step <= prev)
                        || (kind == Some("PLANNER_RESPONSE") && step == prev))
                {
                    state.antigravity_unsafe = true;
                    return ParseOutcome::Incomplete;
                }
                state.antigravity_last_step = Some(step);
            }
            if kind != Some("PLANNER_RESPONSE") {
                return if [
                    "input_tokens",
                    "output_tokens",
                    "cache_read_tokens",
                    "cache_write_tokens",
                ]
                .iter()
                .any(|key| v.get(key).is_some())
                {
                    ParseOutcome::Incomplete
                } else {
                    ParseOutcome::Ignore
                };
            }
            let c = counts(
                &v,
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
            );
            if c.input.is_none()
                && c.output.is_none()
                && c.cache_read.is_none()
                && c.cache_write.is_none()
            {
                return ParseOutcome::Ignore;
            }
            // No stable charged-call identifier has been established for these records.
            // Suppress an indistinguishable replay and report that it may also be a
            // separate call; never hash or retain arbitrary transcript fields.
            use sha2::{Digest, Sha256};
            let signature = format!(
                "{:?}|{:?}|{:?}|{:?}|{:?}",
                c,
                v.get("step_index").and_then(Value::as_u64),
                v.get("created_at")
                    .and_then(crate::discovery::parse_timestamp_value_ms),
                identifier(v.get("model")),
                hour
            );
            let digest = Sha256::digest(signature.as_bytes());
            if seen_before(&mut state.antigravity_seen, &digest) {
                return ParseOutcome::Incomplete;
            }
            ParseOutcome::Observation(bucket(identifier(v.get("model")), hour, c, 0))
        }
        UsageVendor::Codex => {
            if kind == Some("turn_context") {
                let model = identifier(v.pointer("/payload/model"));
                if state.codex_totals.is_some() && state.model != model {
                    state.codex_model_changed = true;
                }
                state.model = model;
                return ParseOutcome::Ignore;
            }
            if kind != Some("event_msg")
                || v.pointer("/payload/type").and_then(Value::as_str) != Some("token_count")
            {
                return ParseOutcome::Ignore;
            }
            let Some(total) = v
                .pointer("/payload/info/total_token_usage")
                .filter(|x| x.is_object())
            else {
                return ParseOutcome::Ignore;
            };
            let c = counts(
                total,
                "input_tokens",
                "output_tokens",
                "cached_input_tokens",
                "cache_creation_input_tokens",
            );
            if state.codex_baseline_only {
                state.codex_totals = Some(c);
                state.codex_baseline_only = false;
                state.codex_unsafe = false;
                return ParseOutcome::Incomplete;
            }
            if state.codex_unsafe {
                return ParseOutcome::Incomplete;
            }
            let delta = if let Some(previous) = &state.codex_totals {
                let Some(delta) = difference(&c, previous) else {
                    state.codex_unsafe = true;
                    return ParseOutcome::Incomplete;
                };
                delta
            } else {
                c.clone()
            };
            state.codex_totals = Some(c);
            if empty(&delta) {
                return ParseOutcome::Ignore;
            }
            if state.codex_model_changed {
                state.codex_model_changed = false;
                return ParseOutcome::IncompleteObservation(bucket(None, hour, delta, 0));
            }
            ParseOutcome::Observation(bucket(state.model.clone(), hour, delta, 0))
        }
        UsageVendor::Claude => {
            if kind != Some("assistant") {
                return ParseOutcome::Ignore;
            }
            let Some(usage) = v.pointer("/message/usage").filter(|x| x.is_object()) else {
                return ParseOutcome::Ignore;
            };
            let Some(id) = identifier(v.pointer("/message/id")) else {
                return ParseOutcome::Incomplete;
            };
            let c = counts(
                usage,
                "input_tokens",
                "output_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            );
            let model = identifier(v.pointer("/message/model"));
            let new = bucket(model.clone(), hour, c.clone(), 0);
            if let Some(old) = state.claude_recent.iter_mut().find(|e| e.message_id == id) {
                let previous = bucket(old.model.clone(), old.hour_ms, old.counts.clone(), 0);
                if previous == new {
                    return ParseOutcome::Ignore;
                }
                old.model = model;
                old.hour_ms = hour;
                old.counts = c;
                return ParseOutcome::Correction { old: previous, new };
            }
            if seen_before(&mut state.claude_seen, id.as_bytes()) {
                return ParseOutcome::Incomplete;
            }
            if state.claude_recent.len() >= 512 {
                state.claude_recent.remove(0);
            }
            state.claude_recent.push(fl_watch::usage::ClaudeBaseline {
                message_id: id,
                model,
                hour_ms: hour,
                counts: c,
            });
            ParseOutcome::Observation(new)
        }
        UsageVendor::Muse => {
            if v.pointer("/payload/event/kind").and_then(Value::as_str) != Some("model_completed") {
                return ParseOutcome::Ignore;
            }
            let Some(usage) = v.pointer("/payload/event/usage").filter(|x| x.is_object()) else {
                return ParseOutcome::Incomplete;
            };
            ParseOutcome::Observation(bucket(
                identifier(v.pointer("/payload/event/model")),
                hour,
                counts(
                    usage,
                    "input_tokens",
                    "output_tokens",
                    "cache_read_tokens",
                    "cache_write_tokens",
                ),
                0,
            ))
        }
        UsageVendor::Grok => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_watch::usage::TokenCounts;
    fn counts(
        input: Option<u64>,
        output: Option<u64>,
        read: Option<u64>,
        write: Option<u64>,
    ) -> TokenCounts {
        TokenCounts {
            input,
            output,
            cache_read: read,
            cache_write: write,
        }
    }
    fn observation(outcome: ParseOutcome) -> UsageBucket {
        if let ParseOutcome::Observation(bucket) = outcome {
            bucket
        } else {
            panic!("expected observation: {outcome:?}")
        }
    }
    #[test]
    fn antigravity_planner_records_real_fields_and_marks_missing_write() {
        let mut s = ParserState::default();
        let b = observation(parse_line(UsageVendor::Antigravity, br#"{"type":"PLANNER_RESPONSE","input_tokens":42,"output_tokens":7,"cache_read_tokens":13,"created_at":"2026-11-10T12:30:00Z","model":"gemini"}"#, &mut s));
        assert_eq!(b.counts, counts(Some(42), Some(7), Some(13), None));
        assert_eq!(b.model.as_deref(), Some("gemini"));
        assert!(b.turns_unavailable);
        assert_eq!(b.hour_ms, Some(1794312000000));
    }
    #[test]
    fn codex_cumulative_snapshots_are_deltas_and_resets_are_incomplete() {
        let mut s = ParserState::default();
        let first = br#"{"timestamp":"2026-11-10T12:30:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":20,"cached_input_tokens":5,"output_tokens":3}}}}"#;
        assert_eq!(
            observation(parse_line(UsageVendor::Codex, first, &mut s)).counts,
            counts(Some(20), Some(3), Some(5), None)
        );
        assert_eq!(
            parse_line(UsageVendor::Codex, first, &mut s),
            ParseOutcome::Ignore
        );
        let second = br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":25,"cached_input_tokens":7,"output_tokens":4}}}}"#;
        assert_eq!(
            observation(parse_line(UsageVendor::Codex, second, &mut s)).counts,
            counts(Some(5), Some(1), Some(2), None)
        );
        let reset = br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1,"cached_input_tokens":1,"output_tokens":1}}}}"#;
        assert_eq!(
            parse_line(UsageVendor::Codex, reset, &mut s),
            ParseOutcome::Incomplete
        );
    }
    #[test]
    fn claude_streamed_revisions_correct_previous_message() {
        let mut s = ParserState::default();
        let a=br#"{"type":"assistant","timestamp":"2026-11-10T12:30:00Z","message":{"id":"msg_1","model":"claude","usage":{"input_tokens":12,"output_tokens":1,"cache_read_input_tokens":2,"cache_creation_input_tokens":3}}}"#;
        let b=br#"{"type":"assistant","timestamp":"2026-11-10T12:30:00Z","message":{"id":"msg_1","model":"claude","usage":{"input_tokens":12,"output_tokens":9,"cache_read_input_tokens":2,"cache_creation_input_tokens":3}}}"#;
        let old = observation(parse_line(UsageVendor::Claude, a, &mut s));
        match parse_line(UsageVendor::Claude, b, &mut s) {
            ParseOutcome::Correction { old: o, new } => {
                assert_eq!(o, old);
                assert_eq!(new.counts.output, Some(9));
            }
            other => panic!("expected correction: {other:?}"),
        }
        assert_eq!(
            parse_line(UsageVendor::Claude, b, &mut s),
            ParseOutcome::Ignore
        );
    }
    #[test]
    fn muse_model_completed_uses_its_own_usage_shape() {
        let mut s = ParserState::default();
        let a=br#"{"timestamp":"2026-11-10T12:30:00Z","payload":{"event":{"kind":"model_completed","model":"muse-model","usage":{"input_tokens":11,"output_tokens":4}}}}"#;
        let b = observation(parse_line(UsageVendor::Muse, a, &mut s));
        assert_eq!(b.counts, counts(Some(11), Some(4), None, None));
        assert_eq!(b.model.as_deref(), Some("muse-model"));
    }
    #[test]
    fn grok_does_not_invent_counts() {
        assert_eq!(
            parse_line(
                UsageVendor::Grok,
                br#"{"kind":"summary"}"#,
                &mut ParserState::default()
            ),
            ParseOutcome::Incomplete
        );
    }
    #[test]
    fn unknown_antigravity_usage_bearing_kind_marks_incomplete() {
        let line = br#"{"type":"OTHER_STEP","input_tokens":12}"#;
        assert_eq!(
            parse_line(UsageVendor::Antigravity, line, &mut ParserState::default()),
            ParseOutcome::Incomplete
        );
    }
    #[test]
    fn codex_model_change_does_not_guess_delta_attribution() {
        let mut state = ParserState::default();
        let snapshot=br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":20,"output_tokens":3}}}}"#;
        let next=br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":30,"output_tokens":4}}}}"#;
        parse_line(
            UsageVendor::Codex,
            br#"{"type":"turn_context","payload":{"model":"model-a"}}"#,
            &mut state,
        );
        observation(parse_line(UsageVendor::Codex, snapshot, &mut state));
        parse_line(
            UsageVendor::Codex,
            br#"{"type":"turn_context","payload":{"model":"model-b"}}"#,
            &mut state,
        );
        let ParseOutcome::IncompleteObservation(bucket) =
            parse_line(UsageVendor::Codex, next, &mut state)
        else {
            panic!("ambiguous model delta must be retained")
        };
        assert_eq!(bucket.model, None);
        assert_eq!(bucket.counts.input, Some(10));
        assert_eq!(bucket.counts.output, Some(1));
    }

    #[test]
    fn agy_rewind_with_changed_usage_is_not_counted() {
        let mut s = ParserState::default();
        observation(parse_line(
            UsageVendor::Antigravity,
            br#"{"type":"PLANNER_RESPONSE","step_index":10,"input_tokens":2}"#,
            &mut s,
        ));
        assert_eq!(
            parse_line(
                UsageVendor::Antigravity,
                br#"{"type":"USER_INPUT","step_index":1}"#,
                &mut s
            ),
            ParseOutcome::Incomplete
        );
        let resumed = observation(parse_line(
            UsageVendor::Antigravity,
            br#"{"type":"PLANNER_RESPONSE","step_index":11,"input_tokens":3}"#,
            &mut s,
        ));
        assert_eq!(resumed.counts.input, Some(3));
    }
    #[test]
    fn claude_recent_eviction_does_not_stop_new_messages_or_recount_old() {
        let mut s = ParserState::default();
        for i in 0..513 {
            let line = format!(
                r#"{{"type":"assistant","message":{{"id":"msg_{i}","usage":{{"input_tokens":1}}}}}}"#
            );
            observation(parse_line(UsageVendor::Claude, line.as_bytes(), &mut s));
        }
        let replay = br#"{"type":"assistant","message":{"id":"msg_0","usage":{"input_tokens":1}}}"#;
        assert_eq!(
            parse_line(UsageVendor::Claude, replay, &mut s),
            ParseOutcome::Incomplete
        );
    }
    #[test]
    fn antigravity_can_continue_past_recent_limit() {
        let mut s = ParserState::default();
        for i in 0..8200 {
            let line =
                format!(r#"{{"type":"PLANNER_RESPONSE","step_index":{i},"input_tokens":1}}"#);
            observation(parse_line(
                UsageVendor::Antigravity,
                line.as_bytes(),
                &mut s,
            ));
        }
    }
    #[test]
    fn parent_link_extracts_only_conversation_identifier() {
        let mut state = ParserState::default();
        let parent = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let line = format!(
            r#"{{"type":"GENERIC","content":"Message sent to \"{}\"."}}"#,
            parent
        );
        parse_line(UsageVendor::Antigravity, line.as_bytes(), &mut state);
        assert_eq!(state.antigravity_parents, vec![parent]);
    }
}
