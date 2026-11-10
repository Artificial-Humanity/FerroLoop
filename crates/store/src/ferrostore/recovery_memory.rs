//! Recovery-only structural admission. No allocator instrumentation is installed.
use super::*;

// All estimates include simultaneous allocations and then a 2x safety factor.
// The retained state is counted even when a caller already owns its immutable pin.
fn admit(
    base: u64,
    scratch: u64,
    extra: u64,
    l: &OperationLimits,
    c: &Check<'_>,
) -> Result<(), RecoveryError> {
    let bytes = add(
        mul(
            add(base, scratch, c, "working memory")?,
            2,
            c,
            "working memory",
        )?,
        extra,
        c,
        "working memory",
    )?;
    l.admit_memory(bytes)
}

pub(super) fn encoding(
    state: &DocState,
    retained: u64,
    l: &OperationLimits,
    c: &Check<'_>,
) -> Result<(), RecoveryError> {
    // Snapshot framing is bounded per item below. JSON lengths are measured by
    // a streaming serializer before allocating escaped JSON or the output Vec.
    let mut output = 4096u64;
    let mut largest_json = 0;
    let mut graph = 0;
    let mut add_output = |n| -> Result<(), RecoveryError> {
        output = add(output, n, c, "encoding memory")?;
        Ok(())
    };
    for (collection, id, value) in state.docs.iter() {
        c.check()?;
        let (n, _) = crate::recovery::verify::serialized_checksum(value, l)?;
        largest_json = largest_json.max(n);
        add_output(add(
            add(
                collection.len() as u64,
                id.len() as u64,
                c,
                "encoding memory",
            )?,
            add(n, 12, c, "encoding memory")?,
            c,
            "encoding memory",
        )?)?;
    }
    for (name, table) in &state.tables {
        add_output(name.len() as u64 + 12)?;
        for column in &table.columns {
            add_output(column.name.len() as u64 + 5)?;
        }
        for group in &table.groups {
            c.check()?;
            add_output(group.len() as u64 + 4)?;
        }
    }
    for (name, col) in &state.vectors {
        c.check()?;
        add_output(name.len() as u64 + 32)?;
        if let Some(entry) = &col.entry {
            add_output(entry.len() as u64 + 4)?;
        }
        // Full graph validation builds indices, spans, a dequantized matrix,
        // and one temporary point. Indices/links are bounded by their counts.
        let mut scratch = mul(
            col.vectors.len() as u64 + 1,
            u64::from(col.dim) * 4,
            c,
            "encoding memory",
        )?;
        for (id, record) in &col.vectors {
            c.check()?;
            add_output(id.len() as u64 + record.bytes.len() as u64 + 10)?;
            scratch = add(scratch, 512, c, "encoding memory")?;
            if let Some(value) = &record.metadata {
                let (n, _) = crate::recovery::verify::serialized_checksum(value, l)?;
                largest_json = largest_json.max(n);
                add_output(n + 4)?;
            }
            for layer in &record.neighbors {
                add_output(4)?;
                scratch = add(
                    scratch,
                    64 + mul(layer.len() as u64, 16, c, "encoding memory")?,
                    c,
                    "encoding memory",
                )?;
                for neighbor in layer {
                    add_output(neighbor.len() as u64 + 4)?;
                }
            }
        }
        graph = graph.max(scratch);
    }
    // Vec growth can briefly retain old + new capacities (<3x final length).
    // JSON serialization's Vec has the same growth bound. Graph validation
    // completes before output construction; max, not sum, is simultaneous.
    let encode = add(
        mul(output, 3, c, "encoding memory")?,
        mul(largest_json, 3, c, "encoding memory")?,
        c,
        "encoding memory",
    )?;
    admit(retained, encode.max(graph), 65536, l, c)
}

fn lexical(
    value: &Value,
    bytes: &mut u64,
    tokens: &mut u64,
    c: &Check<'_>,
) -> Result<(), RecoveryError> {
    c.check()?;
    match value {
        Value::String(s) => {
            let mut in_token = false;
            for (i, ch) in s.chars().enumerate() {
                if i % 16384 == 0 {
                    c.check()?;
                }
                if ch.is_alphanumeric() {
                    if !in_token {
                        *tokens = add(*tokens, 1, c, "lexical memory")?;
                    }
                    in_token = true;
                    for lower in ch.to_lowercase() {
                        *bytes = add(*bytes, lower.len_utf8() as u64, c, "lexical memory")?;
                    }
                } else {
                    in_token = false;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                lexical(value, bytes, tokens, c)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                lexical(value, bytes, tokens, c)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// State search tokenizes serialized Runtime JSON (including keys/numbers),
// unlike document BM25. Count that ASCII token stream without constructing it.
fn state_search_shape(value: &Value, c: &Check<'_>) -> Result<(u64, u64), RecoveryError> {
    struct Counter<'a, 'b> {
        check: &'a Check<'b>,
        bytes: u64,
        tokens: u64,
        in_token: bool,
    }
    impl std::io::Write for Counter<'_, '_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.check.check().map_err(std::io::Error::other)?;
            for chunk in bytes.chunks(16384) {
                self.check.check().map_err(std::io::Error::other)?;
                self.bytes = self
                    .bytes
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| std::io::Error::other("memory overflow"))?;
                for byte in chunk {
                    let word = byte.is_ascii_alphanumeric();
                    if word && !self.in_token {
                        self.tokens = self
                            .tokens
                            .checked_add(1)
                            .ok_or_else(|| std::io::Error::other("memory overflow"))?;
                    }
                    self.in_token = word;
                }
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Counter {
        check: c,
        bytes: 0,
        tokens: 0,
        in_token: false,
    };
    let result = serde_json::to_writer(&mut count, value);
    c.check()?;
    result.map_err(|_| c.verify())?;
    Ok((count.bytes, count.tokens))
}

pub(super) fn validation(
    state: &DocState,
    profile: &crate::recovery::ValidationProfile,
    profile_bytes: u64,
    l: &OperationLimits,
    c: &Check<'_>,
) -> Result<(), RecoveryError> {
    let retained = retained_bytes(state, l.memory_byte_limit, c)?;
    let mut docs = Account {
        bytes: 0,
        limit: l.memory_byte_limit,
        check: c,
    };
    let (mut token_bytes, mut tokens) = (0, 0);
    let mut state_search = 0;
    let mut state_rows = Account {
        bytes: 0,
        limit: l.memory_byte_limit,
        check: c,
    };
    for (collection, id, value) in state.docs.iter() {
        docs.charge(4096)?;
        docs.string(collection)?;
        docs.string(id)?;
        docs.value(value)?;
        lexical(value, &mut token_bytes, &mut tokens, c)?;
        if collection == super::super::doc::STATE_ENTRIES {
            state_rows.charge(4096)?;
            state_rows.string(id)?;
            state_rows.value(value)?;
            let (bytes, tokens) = state_search_shape(value, c)?;
            // SearchDoc owns a cloned entry, summary/search text, frequency
            // map and present set. Serialized Runtime keys/values are included.
            state_search = add(
                state_search,
                add(
                    mul(bytes, 6, c, "state search memory")?,
                    mul(tokens, 1024, c, "state search memory")?,
                    c,
                    "state search memory",
                )?,
                c,
                "state search memory",
            )?;
        }
    }
    // Caller lexical probes can carry larger queries than automatic probes.
    // Charge their normalized shape too, before either API tokenizes it.
    for probe in &profile.probes {
        if let crate::recovery::profiles::QueryProbe::Lexical { query, .. } = probe {
            // Borrowed string visitor avoids cloning the profile for admission.
            let mut in_token = false;
            for (i, ch) in query.chars().enumerate() {
                if i % 16384 == 0 {
                    c.check()?;
                }
                if ch.is_alphanumeric() {
                    if !in_token {
                        tokens = add(tokens, 1, c, "lexical memory")?;
                    }
                    in_token = true;
                    for lower in ch.to_lowercase() {
                        token_bytes =
                            add(token_bytes, lower.len_utf8() as u64, c, "lexical memory")?;
                    }
                } else {
                    in_token = false;
                }
            }
        }
    }
    // Token Vec + term-frequency maps + global document-frequency map + the
    // per-document seen set: <=6 normalized string payload copies, 256 bytes
    // per occurrence covers Vec growth/map capacity/node and minimum String
    // capacity. Counting occurrences (not distinct terms) is conservative.
    let lexical = add(
        mul(token_bytes, 6, c, "lexical memory")?,
        mul(tokens, 256, c, "lexical memory")?,
        c,
        "lexical memory",
    )?;
    // Scan/domain APIs can own typed/cloned rows and a serialized JSON result
    // simultaneously. State remains shared; no mutable cache is installed.
    let mut scratch = mul(docs.bytes, 2, c, "verification memory")?.max(lexical);
    // Column codecs reject expansion before allocating; their admitted scratch
    // includes all columns and row materialization for one group.
    scratch = scratch.max(l.memory_byte_limit / 8);
    // State probes keep entries + exportable live while materializing another
    // query and its JSON result, or while holding the search's cloned rows.
    let state_live = mul(state_rows.bytes, 4, c, "state probe memory")?;
    scratch = scratch.max(add(state_live, state_search, c, "state probe memory")?);
    for col in state.vectors.values() {
        // Query dequantization + visited/heap/ranking IDs. Payloads and graph
        // remain borrowed, and search never builds a whole N*dimension matrix.
        let mut vector = u64::from(col.dim) * 64;
        for id in col.vectors.keys() {
            vector = add(
                vector,
                mul(id.len() as u64 + 256, 16, c, "vector memory")?,
                c,
                "vector memory",
            )?;
        }
        scratch = scratch.max(vector);
    }
    // Profile JSON and persistent probe/exception/result arrays have separate
    // bounded shares. The latter is enforced again as each result is appended.
    let extra = add(
        mul(profile_bytes, 32, c, "profile memory")?,
        l.memory_byte_limit / 8,
        c,
        "verification memory",
    )?;
    admit(retained, scratch, extra, l, c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::{ProfileKind, ValidationProfile};

    fn capture_text(text: String) -> (tempfile::TempDir, FerroStore, CapturedStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FerroStore::open(dir.path().join("fixture.fls")).unwrap();
        store
            .put_doc("fixture", "id", &serde_json::json!({"text": text}))
            .unwrap();
        let capture = store
            .capture_recovery(
                &CaptureBudget::new(
                    Instant::now() + Duration::from_secs(30),
                    64 << 20,
                    64 << 20,
                    Duration::from_secs(1),
                ),
                &CancelToken::new(),
            )
            .unwrap();
        (dir, store, capture)
    }
    fn limits(memory: u64) -> OperationLimits {
        OperationLimits::new(
            Instant::now() + Duration::from_secs(30),
            16 << 20,
            memory,
            CancelToken::new(),
        )
    }
    #[test]
    fn lexical_shape_refuses_many_short_tokens_before_query_allocation() {
        let profile = ValidationProfile::new(ProfileKind::SmallEmpty, "fixture");
        let (_dir, _store, long) = capture_text("a".repeat(200_000));
        assert!(long.expectations(&profile, &limits(16 << 20)).is_ok());
        let (_dir, _store, short) = capture_text("a ".repeat(100_000));
        assert!(matches!(
            short.expectations(&profile, &limits(16 << 20)),
            Err(RecoveryError::Quota {
                resource: "working memory",
                ..
            })
        ));
    }
    #[test]
    fn escaped_json_is_admitted_before_destination_write() {
        for (text, should_fit) in [("x".repeat(65536), true), ("\0".repeat(65536), false)] {
            let (dir, _store, capture) = capture_text(text);
            let mut target = std::fs::File::create(dir.path().join("image.fls")).unwrap();
            let result = capture.write_image(&mut target, &limits(4 << 20));
            if should_fit {
                assert!(result.is_ok());
                assert!(target.metadata().unwrap().len() > 0);
            } else {
                assert!(matches!(
                    result,
                    Err(RecoveryError::Quota {
                        resource: "working memory",
                        ..
                    })
                ));
                assert_eq!(target.metadata().unwrap().len(), 0);
            }
        }
    }
}
