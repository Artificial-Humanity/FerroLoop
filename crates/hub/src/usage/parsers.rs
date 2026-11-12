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
    // Existing persisted filters may have a different length. Keep their
    // original modulus so every previously set bit remains meaningful.
    let bit_count = bits.len() * 64;
    let digest = Sha256::digest(key);
    let mut was_seen = true;
    for chunk in digest.as_chunks::<8>().0 {
        let n = u64::from_le_bytes(*chunk) as usize % bit_count;
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
    let event_time = ["timestamp", "created_at", "recorded_at"]
        .into_iter()
        .find_map(|key| {
            v.get(key)
                .and_then(crate::discovery::parse_timestamp_value_ms)
        });
    let hour = event_time.map(|t| t / 3_600_000 * 3_600_000);
    let kind = v.get("type").and_then(Value::as_str);
    match vendor {
        UsageVendor::Antigravity => {
            // An invoke_subagent tool result records its own creation timestamp and
            // the newly created conversation ID. Retain only these fields; neither
            // the result body nor its workspace paths belong in usage documents.
            if kind == Some("GENERIC")
                && let Some(content) = v.get("content").and_then(Value::as_str)
                && let Some(created) = content
                    .lines()
                    .next()
                    .and_then(|line| line.strip_prefix("Created At: "))
                && let Some(launch_time_ms) =
                    crate::discovery::parse_timestamp_value_ms(&Value::String(created.to_owned()))
                && let Some((_, object)) = content.split_once("Created the following subagent:\n")
                && let Some(Ok(result)) = serde_json::Deserializer::from_str(object.trim_start())
                    .into_iter::<Value>()
                    .next()
                && let Some(id) = result
                    .get("conversationId")
                    .and_then(Value::as_str)
                    .and_then(|id| uuid::Uuid::parse_str(id).ok())
            {
                let launch = fl_watch::usage::AntigravityLaunch {
                    session_id: id.to_string(),
                    launch_time_ms,
                };
                if !state.antigravity_launches.contains(&launch) {
                    if state.antigravity_launches.len() >= 4096 {
                        return ParseOutcome::Incomplete;
                    }
                    state.antigravity_launches.push(launch);
                }
            }
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
            let step = v.get("step_index").and_then(Value::as_u64);
            let newer_than_planner = match (event_time, state.antigravity_last_planner_ms) {
                (Some(current), Some(previous)) if current > previous => true,
                (Some(current), Some(previous)) if current == previous => matches!(
                    (step, state.antigravity_last_planner_step),
                    (Some(current), Some(previous)) if current > previous
                ),
                _ => false,
            };
            if kind == Some("PLANNER_RESPONSE")
                && event_time.is_some()
                && state.antigravity_last_planner_ms.is_some()
                && !newer_than_planner
            {
                return ParseOutcome::Incomplete;
            }
            if state.antigravity_unsafe && newer_than_planner {
                state.antigravity_unsafe = false;
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
                    && !newer_than_planner
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
            if let Some(event_time) = event_time {
                state.antigravity_last_planner_ms = Some(event_time);
                state.antigravity_last_planner_step = step;
            }
            ParseOutcome::Observation(bucket(identifier(v.get("model")), hour, c, 0))
        }
        UsageVendor::Codex => {
            if kind == Some("session_meta") {
                if let Some(parent) = v
                    .pointer("/payload/parent_thread_id")
                    .and_then(Value::as_str)
                {
                    state.codex_parent_id =
                        uuid::Uuid::parse_str(parent).ok().map(|id| id.to_string());
                    state.codex_launch_time_ms = v
                        .pointer("/payload/timestamp")
                        .and_then(crate::discovery::parse_timestamp_value_ms);
                    state.codex_fork_start_ordinal = v
                        .pointer("/payload/subagent_history_start_ordinal")
                        .and_then(Value::as_u64);
                    state.codex_fork_unbounded = state.codex_fork_start_ordinal.is_none();
                }
                return ParseOutcome::Ignore;
            }
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
                "cache_write_input_tokens",
            );
            if state.codex_fork_unbounded {
                state.codex_fork_unbounded = false;
                let last = v
                    .pointer("/payload/info/last_token_usage")
                    .filter(|value| value.is_object())
                    .map(|value| {
                        counts(
                            value,
                            "input_tokens",
                            "output_tokens",
                            "cached_input_tokens",
                            "cache_write_input_tokens",
                        )
                    });
                if last.as_ref() != Some(&c)
                    || [c.input, c.output, c.cache_read, c.cache_write]
                        .iter()
                        .all(Option::is_none)
                {
                    state.codex_totals = Some(c);
                    return ParseOutcome::Incomplete;
                }
            }
            if let Some(start) = state.codex_fork_start_ordinal {
                match v.get("ordinal").and_then(Value::as_u64) {
                    Some(ordinal) if ordinal < start => {
                        state.codex_totals = Some(c);
                        state.codex_fork_baseline_seen = true;
                        return ParseOutcome::Ignore;
                    }
                    Some(_) if !state.codex_fork_baseline_seen => {
                        state.codex_totals = Some(c);
                        state.codex_fork_baseline_seen = true;
                        return ParseOutcome::Incomplete;
                    }
                    None => {
                        state.codex_totals = Some(c);
                        return ParseOutcome::Incomplete;
                    }
                    Some(_) => {}
                }
            }
            if state.codex_baseline_only {
                state.codex_totals = Some(c);
                state.codex_baseline_only = false;
                state.codex_unsafe = false;
                return ParseOutcome::Incomplete;
            }
            if state.codex_unsafe {
                // Older documents could be left unsafe indefinitely after a
                // counter reset. Establish one conservative baseline and resume.
                state.codex_totals = Some(c);
                state.codex_unsafe = false;
                return ParseOutcome::Incomplete;
            }
            let delta = if let Some(previous) = &state.codex_totals {
                let Some(delta) = difference(&c, previous) else {
                    state.codex_totals = Some(c);
                    state.codex_unsafe = false;
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
                // The last-response counters prove the entire delta belongs to
                // the new model. Otherwise a late snapshot may straddle models.
                let last = v
                    .pointer("/payload/info/last_token_usage")
                    .filter(|value| value.is_object())
                    .map(|value| {
                        counts(
                            value,
                            "input_tokens",
                            "output_tokens",
                            "cached_input_tokens",
                            "cache_write_input_tokens",
                        )
                    });
                if last.as_ref() != Some(&delta) {
                    return ParseOutcome::IncompleteObservation(bucket(None, hour, delta, 0));
                }
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
    fn unbounded_fork_counts_fresh_snapshot_and_baselines_inherited() {
        let meta = br#"{"type":"session_meta","payload":{"parent_thread_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}}"#;
        let event = |total, last| {
            serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":total},"last_token_usage":{"input_tokens":last}}}}).to_string()
        };
        let mut fresh = ParserState::default();
        assert_eq!(
            parse_line(UsageVendor::Codex, meta, &mut fresh),
            ParseOutcome::Ignore
        );
        assert_eq!(
            observation(parse_line(
                UsageVendor::Codex,
                event(10, 10).as_bytes(),
                &mut fresh
            ))
            .counts
            .input,
            Some(10)
        );
        assert_eq!(
            observation(parse_line(
                UsageVendor::Codex,
                event(15, 5).as_bytes(),
                &mut fresh
            ))
            .counts
            .input,
            Some(5)
        );
        let mut inherited = ParserState::default();
        parse_line(UsageVendor::Codex, meta, &mut inherited);
        assert_eq!(
            parse_line(UsageVendor::Codex, event(100, 5).as_bytes(), &mut inherited),
            ParseOutcome::Incomplete
        );
        assert_eq!(
            observation(parse_line(
                UsageVendor::Codex,
                event(110, 10).as_bytes(),
                &mut inherited
            ))
            .counts
            .input,
            Some(10)
        );
    }
    #[test]
    fn compact_small_filter_stays_small_and_preserves_membership() {
        let mut state = ParserState::default();
        for key in [b"first".as_slice(), b"second"] {
            assert!(!seen_before(&mut state.claude_seen, key));
        }
        let encoded = serde_json::to_value(&state).unwrap();
        assert!(encoded["claude_seen"].as_str().unwrap().len() < 200);
        let mut decoded: ParserState = serde_json::from_value(encoded).unwrap();
        assert!(seen_before(&mut decoded.claude_seen, b"first"));
        assert!(seen_before(&mut decoded.claude_seen, b"second"));
    }
    #[test]
    fn dense_filter_remains_compatible_when_sparse_would_be_larger() {
        let state = ParserState {
            claude_seen: vec![u64::MAX; 8192],
            ..Default::default()
        };
        let encoded = serde_json::to_value(&state).unwrap();
        let encoded_filter = encoded["claude_seen"].as_str().unwrap();
        assert!(!encoded_filter.starts_with("s:"));
        assert_eq!(encoded_filter.len(), 87384);
        let decoded: ParserState = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.claude_seen, state.claude_seen);
    }
    #[test]
    fn antigravity_same_second_distinct_steps_are_counted() {
        let mut state = ParserState::default();
        let event = |step| {
            serde_json::json!({"type":"PLANNER_RESPONSE","created_at":"2026-11-10T12:30:00Z","step_index":step,"input_tokens":1}).to_string()
        };
        observation(parse_line(
            UsageVendor::Antigravity,
            event(1).as_bytes(),
            &mut state,
        ));
        observation(parse_line(
            UsageVendor::Antigravity,
            event(2).as_bytes(),
            &mut state,
        ));
        assert_eq!(
            parse_line(UsageVendor::Antigravity, event(1).as_bytes(), &mut state),
            ParseOutcome::Incomplete
        );
    }
    #[test]
    fn codex_legacy_unsafe_baseline_recovers_without_counting_snapshot() {
        let mut state = ParserState {
            codex_unsafe: true,
            ..Default::default()
        };
        let event = |n| {
            serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":n}}}}).to_string()
        };
        assert_eq!(
            parse_line(UsageVendor::Codex, event(100).as_bytes(), &mut state),
            ParseOutcome::Incomplete
        );
        assert!(!state.codex_unsafe);
        assert_eq!(
            observation(parse_line(
                UsageVendor::Codex,
                event(105).as_bytes(),
                &mut state
            ))
            .counts
            .input,
            Some(5)
        );
    }
    #[test]
    fn new_replay_filters_are_bounded_and_legacy_filters_keep_their_membership() {
        let mut new_filter = Vec::new();
        assert!(!seen_before(&mut new_filter, b"new-key"));
        assert_eq!(new_filter.len(), 8192);
        assert!(seen_before(&mut new_filter, b"new-key"));
        let mut legacy = vec![0; 8192];
        assert!(!seen_before(&mut legacy, b"legacy-key"));
        let encoded = serde_json::to_string(&legacy).unwrap();
        let mut decoded: Vec<u64> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.len(), 8192);
        assert!(seen_before(&mut decoded, b"legacy-key"));
        let state = ParserState {
            claude_seen: decoded,
            ..Default::default()
        };
        let serialized = serde_json::to_value(&state).unwrap();
        assert!(serialized["claude_seen"].is_string());
        let restored: ParserState = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored.claude_seen, state.claude_seen);
        let mut legacy = serde_json::to_value(&state).unwrap();
        legacy["claude_seen"] = serde_json::json!(state.claude_seen);
        let restored: ParserState = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.claude_seen, state.claude_seen);
    }
    #[test]
    fn null_timestamp_falls_back_to_created_at() {
        let mut state = ParserState::default();
        let line = br#"{"type":"PLANNER_RESPONSE","timestamp":null,"created_at":"2026-11-10T12:30:00Z","input_tokens":1}"#;
        let result = observation(parse_line(UsageVendor::Antigravity, line, &mut state));
        assert_eq!(result.hour_ms, Some(1794312000000));
    }
    #[test]
    fn muse_recorded_at_microseconds_sets_hour() {
        let mut state = ParserState::default();
        let line = br#"{"recorded_at":1794313800000000,"payload":{"event":{"kind":"model_completed","usage":{"input_tokens":25,"output_tokens":3}}}}"#;
        let bucket = observation(parse_line(UsageVendor::Muse, line, &mut state));
        assert_eq!(bucket.hour_ms, Some(1794312000000));
        assert_eq!(bucket.counts.input, Some(25));
    }
    #[test]
    fn launch_result_requires_structured_identity_and_recorded_time() {
        let mut state = ParserState::default();
        let result = serde_json::json!({"type":"GENERIC", "content":"Created the following subagent:\n{\"conversationId\":\"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\"}"});
        parse_line(
            UsageVendor::Antigravity,
            result.to_string().as_bytes(),
            &mut state,
        );
        assert!(state.antigravity_launches.is_empty());
        let result = serde_json::json!({"type":"GENERIC", "content":"Created At: 2026-11-10T12:30:00Z\nCreated the following subagent:\n{\"conversationId\":\"invalid\"}"});
        parse_line(
            UsageVendor::Antigravity,
            result.to_string().as_bytes(),
            &mut state,
        );
        assert!(state.antigravity_launches.is_empty());
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
        let next=br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":30,"output_tokens":4},"last_token_usage":{"input_tokens":10,"output_tokens":1}}}}"#;
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
        let bucket = observation(parse_line(UsageVendor::Codex, next, &mut state));
        assert_eq!(bucket.model.as_deref(), Some("model-b"));
        assert_eq!(bucket.counts.input, Some(10));
        assert_eq!(bucket.counts.output, Some(1));
    }

    #[test]
    fn codex_model_change_unknown_when_last_response_cannot_prove_delta() {
        let mut state = ParserState::default();
        let event = |total| {
            serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":total}}}}).to_string()
        };
        parse_line(
            UsageVendor::Codex,
            br#"{"type":"turn_context","payload":{"model":"a"}}"#,
            &mut state,
        );
        observation(parse_line(
            UsageVendor::Codex,
            event(20).as_bytes(),
            &mut state,
        ));
        parse_line(
            UsageVendor::Codex,
            br#"{"type":"turn_context","payload":{"model":"b"}}"#,
            &mut state,
        );
        let ParseOutcome::IncompleteObservation(bucket) =
            parse_line(UsageVendor::Codex, event(30).as_bytes(), &mut state)
        else {
            panic!("ambiguous delta")
        };
        assert_eq!(bucket.model, None);
        assert_eq!(bucket.counts.input, Some(10));
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
