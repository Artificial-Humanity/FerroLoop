use fl_watch::usage::{SourceUsageDocument, UsageVendor};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct UsageSource {
    pub path: PathBuf,
    pub source_id: String,
    pub session_id: String,
    pub vendor: UsageVendor,
}
#[derive(Debug, Clone, Copy)]
pub struct ScanLimits {
    pub max_line_bytes: usize,
    pub max_batch_bytes: usize,
    pub max_lines: usize,
}
impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_line_bytes: 1024 * 1024,
            max_batch_bytes: 4 * 1024 * 1024,
            max_lines: 4096,
        }
    }
}
#[derive(Debug, thiserror::Error)]
pub enum UsageScanError {
    #[error("source I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid source or scan limits")]
    Invalid,
}
fn identity(metadata: &std::fs::Metadata) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}
fn unchanged(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if identity(before) != identity(after)
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.ctime() != after.ctime() || before.ctime_nsec() != after.ctime_nsec() {
            return false;
        }
    }
    true
}
fn anchor(file: &mut std::fs::File, offset: u64) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Seek, SeekFrom};
    // Checking the complete prefix detects rewrites even outside the last block.
    // Larger prefixes use a bounded tail anchor and are explicitly unverified.
    let start = if offset <= 4 * 1024 * 1024 {
        0
    } else {
        offset.saturating_sub(4096)
    };
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = vec![0; (offset - start) as usize];
    file.read_exact(&mut buffer)?;
    Ok(format!("{:x}", Sha256::digest(&buffer)))
}
fn merge(
    doc: &mut SourceUsageDocument,
    incoming: &fl_watch::usage::UsageBucket,
    remove: bool,
) -> bool {
    use fl_watch::usage::UsageBucket;
    let index = doc
        .buckets
        .iter()
        .position(|b| b.model == incoming.model && b.hour_ms == incoming.hour_ms);
    if remove && index.is_none() {
        return false;
    }
    let mut b = index
        .map(|i| doc.buckets[i].clone())
        .unwrap_or_else(|| UsageBucket {
            model: incoming.model.clone(),
            hour_ms: incoming.hour_ms,
            counts: Default::default(),
            turns: 0,
            turns_unavailable: false,
            unavailable: Default::default(),
            missing: Default::default(),
            observations: 0,
        });
    fn apply(a: &mut Option<u64>, b: Option<u64>, remove: bool) -> bool {
        if let Some(n) = b {
            let result = if remove {
                a.unwrap_or(0).checked_sub(n)
            } else {
                a.unwrap_or(0).checked_add(n)
            };
            if let Some(n) = result {
                *a = Some(n);
            } else {
                return false;
            }
        }
        true
    }
    if !apply(&mut b.counts.input, incoming.counts.input, remove)
        || !apply(&mut b.counts.output, incoming.counts.output, remove)
        || !apply(&mut b.counts.cache_read, incoming.counts.cache_read, remove)
        || !apply(
            &mut b.counts.cache_write,
            incoming.counts.cache_write,
            remove,
        )
    {
        return false;
    }
    let Some(turns) = (if remove {
        b.turns.checked_sub(incoming.turns)
    } else {
        b.turns.checked_add(incoming.turns)
    }) else {
        return false;
    };
    b.turns = turns;
    fn update(n: &mut u64, value: u64, remove: bool) -> bool {
        let next = if remove {
            n.checked_sub(value)
        } else {
            n.checked_add(value)
        };
        if let Some(next) = next {
            *n = next;
            true
        } else {
            false
        }
    }
    if !update(&mut b.observations, incoming.observations, remove)
        || !update(&mut b.missing.input, incoming.missing.input, remove)
        || !update(&mut b.missing.output, incoming.missing.output, remove)
        || !update(
            &mut b.missing.cache_read,
            incoming.missing.cache_read,
            remove,
        )
        || !update(
            &mut b.missing.cache_write,
            incoming.missing.cache_write,
            remove,
        )
    {
        return false;
    }
    b.turns_unavailable |= incoming.turns_unavailable;
    b.unavailable.input = b.missing.input > 0;
    b.unavailable.output = b.missing.output > 0;
    b.unavailable.cache_read = b.missing.cache_read > 0;
    b.unavailable.cache_write = b.missing.cache_write > 0;
    if let Some(i) = index {
        if b.observations == 0 {
            doc.buckets.remove(i);
        } else {
            doc.buckets[i] = b;
        }
    } else {
        doc.buckets.push(b);
    }
    true
}

pub fn scan_source(
    source: &UsageSource,
    current: Option<&SourceUsageDocument>,
    limits: ScanLimits,
) -> Result<SourceUsageDocument, UsageScanError> {
    scan_source_with_hook(source, current, limits, || {})
}

fn scan_source_with_hook(
    source: &UsageSource,
    current: Option<&SourceUsageDocument>,
    limits: ScanLimits,
    after_read: impl FnOnce(),
) -> Result<SourceUsageDocument, UsageScanError> {
    use super::parsers::{ParseOutcome, parse_line};
    use std::io::{Read, Seek, SeekFrom};
    if limits.max_line_bytes == 0
        || limits.max_batch_bytes <= limits.max_line_bytes
        || limits.max_lines == 0
        || source.source_id.is_empty()
        || source.session_id.is_empty()
    {
        return Err(UsageScanError::Invalid);
    }
    if let Some(old) = current
        && (old.source_id != source.source_id
            || old.session_id != source.session_id
            || old.vendor != source.vendor)
    {
        return Err(UsageScanError::Invalid);
    }
    let mut file = std::fs::File::open(&source.path)?;
    let meta = file.metadata()?;
    let mut doc = current.cloned().unwrap_or_else(|| SourceUsageDocument {
        source_id: source.source_id.clone(),
        session_id: source.session_id.clone(),
        vendor: source.vendor,
        offset: 0,
        generation: 0,
        file_identity: identity(&meta),
        anchor: String::new(),
        discarding_line: false,
        parser: Default::default(),
        buckets: Vec::new(),
        coverage: Default::default(),
    });
    if source.vendor == UsageVendor::Grok {
        doc.coverage.unavailable = true;
    }
    if current.is_some()
        && (meta.len() < doc.offset
            || identity(&meta) != doc.file_identity
            || (!doc.anchor.is_empty() && anchor(&mut file, doc.offset)? != doc.anchor))
    {
        let original = doc.clone();
        doc.generation = doc.generation.saturating_add(1);
        doc.coverage.incomplete = true;
        doc.coverage.ambiguous_generation = true;
        doc.coverage.pending = false;
        // Existing content in the replacement may overlap prior counts. Skip it,
        // including an unfinished trailing record; later appends can be scanned.
        doc.offset = meta.len();
        doc.file_identity = identity(&meta);
        doc.discarding_line = if doc.offset == 0 {
            false
        } else {
            file.seek(SeekFrom::Start(doc.offset - 1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            last[0] != b'\n'
        };
        doc.anchor = anchor(&mut file, doc.offset)?;
        if doc.offset > 4 * 1024 * 1024 {
            doc.coverage.unverified_prefix = true;
        }
        let after = file.metadata()?;
        let path_meta = std::fs::metadata(&source.path)?;
        if !unchanged(&meta, &after) || identity(&after) != identity(&path_meta) {
            let mut retry = original;
            retry.coverage.incomplete = true;
            return Ok(retry);
        }
        match source.vendor {
            UsageVendor::Codex => doc.parser.codex_baseline_only = true,
            UsageVendor::Antigravity => doc.parser.antigravity_unsafe = true,
            _ => {}
        }
        return Ok(doc);
    }
    let original = doc.clone();
    file.seek(SeekFrom::Start(doc.offset))?;
    let mut buffer = Vec::with_capacity(limits.max_batch_bytes.min(1024 * 1024));
    (&mut file)
        .take(limits.max_batch_bytes as u64)
        .read_to_end(&mut buffer)?;
    after_read();
    let mut at = 0;
    let mut lines = 0;
    while at < buffer.len() && lines < limits.max_lines {
        let rest = &buffer[at..];
        let end = rest.iter().position(|&b| b == b'\n');
        if doc.discarding_line {
            match end {
                Some(i) => {
                    at += i + 1;
                    doc.discarding_line = false;
                    lines += 1;
                }
                None => {
                    at = buffer.len();
                }
            }
            continue;
        }
        match end {
            Some(i) => {
                if i > limits.max_line_bytes {
                    doc.coverage.incomplete = true;
                    doc.coverage.oversized_lines = doc.coverage.oversized_lines.saturating_add(1);
                } else if i > 0 {
                    match parse_line(source.vendor, &rest[..i], &mut doc.parser) {
                        ParseOutcome::Ignore => {}
                        ParseOutcome::Incomplete => {
                            doc.coverage.incomplete = true;
                            if source.vendor == UsageVendor::Grok {
                                doc.coverage.unavailable = true;
                            }
                        }
                        ParseOutcome::Malformed => {
                            doc.coverage.incomplete = true;
                            doc.coverage.malformed_lines =
                                doc.coverage.malformed_lines.saturating_add(1);
                        }
                        ParseOutcome::IncompleteObservation(bucket) => {
                            doc.coverage.incomplete = true;
                            if !merge(&mut doc, &bucket, false) {
                                doc.coverage.incomplete = true;
                            }
                        }
                        ParseOutcome::Observation(bucket) => {
                            if !merge(&mut doc, &bucket, false) {
                                doc.coverage.incomplete = true;
                            }
                        }
                        ParseOutcome::Correction { old, new } => {
                            let mut candidate = doc.clone();
                            if merge(&mut candidate, &old, true)
                                && merge(&mut candidate, &new, false)
                            {
                                doc.buckets = candidate.buckets;
                            } else {
                                doc.coverage.incomplete = true;
                            }
                        }
                    }
                }
                at += i + 1;
                lines += 1;
            }
            None if rest.len() > limits.max_line_bytes => {
                doc.coverage.incomplete = true;
                doc.coverage.oversized_lines = doc.coverage.oversized_lines.saturating_add(1);
                doc.discarding_line = true;
                at = buffer.len();
            }
            None => break,
        }
    }
    doc.offset = doc.offset.saturating_add(at as u64);
    doc.coverage.pending = doc.offset < meta.len();
    if doc.offset > 4 * 1024 * 1024 {
        doc.coverage.incomplete = true;
        doc.coverage.unverified_prefix = true;
    }
    // An in-place rewrite or replacement while reading cannot be paired with
    // the candidate counts. Retry later from the old cursor instead.
    let new_anchor = match anchor(&mut file, doc.offset) {
        Ok(anchor) => anchor,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            let mut retry = original;
            retry.coverage.incomplete = true;
            return Ok(retry);
        }
        Err(error) => return Err(error.into()),
    };
    let after = file.metadata()?;
    let path_meta = std::fs::metadata(&source.path)?;
    if identity(&after) != identity(&path_meta) || !unchanged(&meta, &after) {
        let mut retry = original;
        retry.coverage.incomplete = true;
        return Ok(retry);
    }
    doc.anchor = new_anchor;
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn scan_appends_only_complete_lines_and_handles_partial_lines() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let line = b"{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":42}\n";
        f.write_all(line).unwrap();
        let source = UsageSource {
            path: f.path().to_owned(),
            source_id: "fixture".into(),
            session_id: "session".into(),
            vendor: UsageVendor::Antigravity,
        };
        let first = scan_source(&source, None, ScanLimits::default()).unwrap();
        assert_eq!(first.buckets[0].counts.input, Some(42));
        let second = b"{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":43}\n";
        f.write_all(&second[..second.len() - 1]).unwrap();
        let partial = scan_source(&source, Some(&first), ScanLimits::default()).unwrap();
        assert_eq!(partial.offset, first.offset);
        f.write_all(b"\n").unwrap();
        let complete = scan_source(&source, Some(&partial), ScanLimits::default()).unwrap();
        assert_eq!(complete.buckets[0].counts.input, Some(85));
        assert_eq!(
            scan_source(&source, Some(&complete), ScanLimits::default()).unwrap(),
            complete
        );
    }
    fn source(path: &std::path::Path) -> UsageSource {
        UsageSource {
            path: path.to_owned(),
            source_id: "fixture".into(),
            session_id: "s".into(),
            vendor: UsageVendor::Antigravity,
        }
    }
    #[test]
    fn unsafe_repeated_antigravity_rows_are_flagged() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":2}\n{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":2}\n").unwrap();
        let doc = scan_source(&source(f.path()), None, ScanLimits::default()).unwrap();
        assert!(
            doc.coverage.incomplete,
            "without stable event identities duplicate usage is ambiguous"
        );
        assert_eq!(doc.buckets[0].counts.input, Some(2));
    }
    #[test]
    fn malformed_oversized_and_batch_limits_preserve_later_rows() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"not json\n").unwrap();
        f.write_all(&[b'x'; 150]).unwrap();
        f.write_all(b"\n{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":2}\n")
            .unwrap();
        let limits = ScanLimits {
            max_line_bytes: 100,
            max_batch_bytes: 120,
            max_lines: 1,
        };
        let mut doc = scan_source(&source(f.path()), None, limits).unwrap();
        for _ in 0..5 {
            if !doc.coverage.pending {
                break;
            }
            doc = scan_source(&source(f.path()), Some(&doc), limits).unwrap();
        }
        assert_eq!(doc.coverage.malformed_lines, 1);
        assert_eq!(doc.coverage.oversized_lines, 1);
        assert!(doc.coverage.incomplete);
        assert_eq!(doc.buckets[0].counts.input, Some(2));
        assert!(!doc.coverage.pending);
    }
    #[test]
    fn truncation_preserves_counts_and_refuses_ambiguous_new_generation() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":10}\n")
            .unwrap();
        let src = source(f.path());
        let old = scan_source(&src, None, ScanLimits::default()).unwrap();
        std::fs::write(f.path(), b"{}\n").unwrap();
        let changed = scan_source(&src, Some(&old), ScanLimits::default()).unwrap();
        assert_eq!(changed.buckets, old.buckets);
        assert_eq!(changed.generation, old.generation + 1);
        assert!(changed.coverage.ambiguous_generation);
    }
    #[test]
    fn replacement_preserves_counts_even_for_a_larger_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        std::fs::write(
            &path,
            b"{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":10}\n",
        )
        .unwrap();
        let src = source(&path);
        let old = scan_source(&src, None, ScanLimits::default()).unwrap();
        let replacement = dir.path().join("replacement");
        std::fs::write(
            &replacement,
            b"{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":1234567890}\n",
        )
        .unwrap();
        std::fs::rename(replacement, &path).unwrap();
        let changed = scan_source(&src, Some(&old), ScanLimits::default()).unwrap();
        assert!(changed.coverage.ambiguous_generation);
        assert_eq!(changed.buckets, old.buckets);
    }
    #[test]
    fn empty_grok_source_reports_unavailable() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let mut src = source(f.path());
        src.vendor = UsageVendor::Grok;
        let doc = scan_source(&src, None, ScanLimits::default()).unwrap();
        assert!(doc.coverage.unavailable);
    }
    #[test]
    fn overflow_does_not_partially_add_an_observation() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":1,\"output_tokens\":18446744073709551615}\n").unwrap();
        let src = source(f.path());
        let first = scan_source(&src, None, ScanLimits::default()).unwrap();
        f.write_all(b"{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":1,\"output_tokens\":1}\n")
            .unwrap();
        let next = scan_source(&src, Some(&first), ScanLimits::default()).unwrap();
        assert!(next.coverage.incomplete);
        assert_eq!(next.buckets, first.buckets);
    }
    #[test]
    fn rewrite_outside_tail_anchor_is_detected() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":1}\n")
            .unwrap();
        f.write_all(&vec![b' '; 5000]).unwrap();
        f.write_all(b"\n").unwrap();
        let src = source(f.path());
        let first = scan_source(&src, None, ScanLimits::default()).unwrap();
        use std::io::{Seek, SeekFrom};
        f.seek(SeekFrom::Start(1)).unwrap();
        f.write_all(b"X").unwrap();
        let next = scan_source(&src, Some(&first), ScanLimits::default()).unwrap();
        assert!(next.coverage.ambiguous_generation);
    }
    #[test]
    fn claude_correction_restores_category_availability() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"{\"type\":\"assistant\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1}}}\n").unwrap();
        let mut src = source(f.path());
        src.vendor = UsageVendor::Claude;
        let first = scan_source(&src, None, ScanLimits::default()).unwrap();
        assert!(first.buckets[0].unavailable.output);
        f.write_all(b"{\"type\":\"assistant\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n").unwrap();
        let next = scan_source(&src, Some(&first), ScanLimits::default()).unwrap();
        assert!(!next.buckets[0].unavailable.output);
        assert_eq!(next.buckets[0].counts.output, Some(2));
    }
    #[test]
    fn large_prefix_is_explicitly_unverified() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for _ in 0..1100 {
            f.write_all(&vec![b' '; 4000]).unwrap();
            f.write_all(b"\n").unwrap();
        }
        let src = source(f.path());
        let mut doc = scan_source(&src, None, ScanLimits::default()).unwrap();
        while doc.coverage.pending {
            doc = scan_source(&src, Some(&doc), ScanLimits::default()).unwrap();
        }
        assert!(doc.coverage.incomplete);
        assert!(doc.coverage.unverified_prefix);
    }
    #[test]
    fn corrected_message_moves_between_buckets_and_clears_missing_flag() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"{\"type\":\"assistant\",\"timestamp\":\"2026-11-10T12:00:00Z\",\"message\":{\"id\":\"a\",\"usage\":{\"input_tokens\":1}}}\n").unwrap();
        f.write_all(b"{\"type\":\"assistant\",\"timestamp\":\"2026-11-10T12:00:00Z\",\"message\":{\"id\":\"b\",\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}}\n").unwrap();
        let mut src = source(f.path());
        src.vendor = UsageVendor::Claude;
        let first = scan_source(&src, None, ScanLimits::default()).unwrap();
        f.write_all(b"{\"type\":\"assistant\",\"timestamp\":\"2026-11-10T13:00:00Z\",\"message\":{\"id\":\"a\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n").unwrap();
        let next = scan_source(&src, Some(&first), ScanLimits::default()).unwrap();
        assert_eq!(next.buckets.len(), 2);
        assert!(next.buckets.iter().all(|b| !b.unavailable.output));
        assert_eq!(
            next.buckets
                .iter()
                .map(|b| b.counts.output.unwrap())
                .sum::<u64>(),
            5
        );
    }
    #[test]
    fn rotated_source_can_count_distinct_future_steps() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"{\"type\":\"PLANNER_RESPONSE\",\"step_index\":10,\"input_tokens\":10}\n")
            .unwrap();
        let src = source(f.path());
        let first = scan_source(&src, None, ScanLimits::default()).unwrap();
        std::fs::write(f.path(), b"{}\n").unwrap();
        let rotated = scan_source(&src, Some(&first), ScanLimits::default()).unwrap();
        assert!(rotated.coverage.ambiguous_generation);
        use std::io::{Seek, SeekFrom};
        f.seek(SeekFrom::End(0)).unwrap();
        f.write_all(b"{\"type\":\"PLANNER_RESPONSE\",\"step_index\":11,\"input_tokens\":3}\n")
            .unwrap();
        let resumed = scan_source(&src, Some(&rotated), ScanLimits::default()).unwrap();
        assert_eq!(resumed.buckets[0].counts.input, Some(13));
        assert!(resumed.coverage.incomplete);
    }
    #[test]
    fn rewrite_during_scan_does_not_commit_mixed_candidate() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"{\"type\":\"PLANNER_RESPONSE\",\"step_index\":1,\"input_tokens\":1}\n")
            .unwrap();
        let src = source(f.path());
        let old = scan_source(&src, None, ScanLimits::default()).unwrap();
        f.write_all(b"{\"type\":\"PLANNER_RESPONSE\",\"step_index\":2,\"input_tokens\":2}\n")
            .unwrap();
        let path = f.path().to_owned();
        let result = scan_source_with_hook(&src, Some(&old), ScanLimits::default(), || {
            std::fs::write(
                path,
                b"{\"type\":\"PLANNER_RESPONSE\",\"step_index\":9,\"input_tokens\":9}\n",
            )
            .unwrap();
        })
        .unwrap();
        assert_eq!(result.buckets, old.buckets);
        assert_eq!(result.offset, old.offset);
        assert!(result.coverage.incomplete);
    }
    #[test]
    fn codex_rotation_establishes_new_baseline_before_counting_deltas() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":10}}}}\n").unwrap();
        let mut src = source(f.path());
        src.vendor = UsageVendor::Codex;
        let old = scan_source(&src, None, ScanLimits::default()).unwrap();
        std::fs::write(f.path(), b"{}\n").unwrap();
        let rotated = scan_source(&src, Some(&old), ScanLimits::default()).unwrap();
        use std::io::{Seek, SeekFrom};
        f.seek(SeekFrom::End(0)).unwrap();
        f.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":100}}}}\n").unwrap();
        let baseline = scan_source(&src, Some(&rotated), ScanLimits::default()).unwrap();
        assert_eq!(baseline.buckets[0].counts.input, Some(10));
        f.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":105}}}}\n").unwrap();
        let next = scan_source(&src, Some(&baseline), ScanLimits::default()).unwrap();
        assert_eq!(next.buckets[0].counts.input, Some(15));
        assert!(next.coverage.incomplete);
    }
}
