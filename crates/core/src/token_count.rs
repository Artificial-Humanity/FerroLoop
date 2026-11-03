//! Counted token lengths for the code-index savings ledger.
//!
//! The unit is `o200k_base`, OpenAI's public BPE vocabulary: a real count of
//! BPE tokens, not the tokenizer of Claude, Gemini or any other model, whose
//! counts differ by vendor. It sits beside the `ceil(chars / 4)` estimate
//! ([`crate::state::estimate_tokens`]), which stays the recall-budget unit.
//!
//! Counting needs the `counted-tokens` feature (off by default, so light
//! dependents never build the vocabulary). Without it every count is `None`.

use serde::{Deserialize, Serialize};

/// The tokenizer a counted value was measured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CountUnit {
    /// OpenAI's public `o200k_base` BPE.
    #[serde(rename = "o200k_base")]
    O200kBase,
}

/// The unit [`counted_tokens`] counts in: `Some` only with the
/// `counted-tokens` feature.
pub fn count_unit() -> Option<CountUnit> {
    cfg!(feature = "counted-tokens").then_some(CountUnit::O200kBase)
}

/// The exact `o200k_base` token count of `text` (empty text is `0`), or
/// `None` without the `counted-tokens` feature. The tokenizer is a
/// process-wide lazy static, loaded on the first count.
#[cfg(feature = "counted-tokens")]
pub fn counted_tokens(text: &str) -> Option<u64> {
    if text.is_empty() {
        return Some(0);
    }
    Some(bpe_openai::o200k_base().count(text) as u64)
}

/// The exact `o200k_base` token count of `text`, or `None` without the
/// `counted-tokens` feature (this build).
#[cfg(not(feature = "counted-tokens"))]
pub fn counted_tokens(_text: &str) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_serializes_as_its_vocabulary_name() {
        assert_eq!(
            serde_json::to_value(CountUnit::O200kBase).unwrap(),
            serde_json::json!("o200k_base")
        );
        assert_eq!(
            serde_json::from_value::<CountUnit>(serde_json::json!("o200k_base")).unwrap(),
            CountUnit::O200kBase
        );
    }

    #[cfg(feature = "counted-tokens")]
    #[test]
    fn counts_are_exact_o200k_base_tokens() {
        assert_eq!(count_unit(), Some(CountUnit::O200kBase));
        assert_eq!(counted_tokens(""), Some(0));
        // Pinned against the crate and against tiktoken's `o200k_base`.
        assert_eq!(counted_tokens("hello world"), Some(2));
        assert_eq!(
            counted_tokens("pub fn alpha(x: u32) -> u32 {\n    x + 1\n}\n"),
            Some(19)
        );
    }

    #[cfg(not(feature = "counted-tokens"))]
    #[test]
    fn without_the_feature_nothing_is_counted() {
        assert_eq!(count_unit(), None);
        assert_eq!(counted_tokens(""), None);
        assert_eq!(counted_tokens("hello world"), None);
    }
}
