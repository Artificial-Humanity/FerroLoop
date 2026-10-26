pub mod claude;
pub mod harness;

pub use claude::{ClaudeAdapter, ClaudeDialect};
pub use harness::{
    CliDialect, EXCERPT_LIMIT, GitSnapshot, LivenessSink, ParsedOutput, ProcessHarness,
    detect_host, extract_text_from_object, extract_usage_from_value, now_at,
    parse_json_or_jsonl_envelope, truncate_utf8, usd_f64_to_micros,
};
