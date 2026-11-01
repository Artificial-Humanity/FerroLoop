//! Deterministic CPU text embedder with no runtime dependencies beyond
//! `thiserror` and `sha2`.
//!
//! # Honest scope
//!
//! [`HashingEmbedder`] is a hashed lexical and subword feature embedder:
//! word unigrams, word bigrams and character n-grams are feature-hashed
//! into a fixed-dimension signed vector, weighted sublinearly by term
//! frequency, and L2 normalised. It captures lexical and subword overlap
//! (typos, inflections, shared stems, identifier pieces such as the parts
//! of `snake_case` or `camelCase` names). It is **not** a learned semantic
//! model: synonyms that share no subwords are not close. A learned model
//! is a future drop-in behind the [`Embedder`] trait.
//!
//! # Determinism
//!
//! The output is bit-for-bit reproducible for a given config and input on
//! one toolchain: hashing is an in-file 64-bit FNV-1a (not std's
//! `DefaultHasher`), features are processed in sorted order, the
//! accumulator is a fixed array, and all float math uses only IEEE
//! `+ - * /` and `sqrt` (the sublinear `1 + ln(tf)` weight uses an in-file
//! series rather than the platform `ln`). Output for non-ASCII text follows
//! the compiler's Unicode tables (case mapping) and may change when the
//! Unicode version changes; if a toolchain upgrade changes the golden value,
//! bump the `hashing-v1` tag in [`Embedder::space_id`].
//!
//! # Limits
//!
//! Input is capped at [`MAX_EMBED_BYTES`] (64 KiB); longer text is
//! truncated on a char boundary, so a huge body cannot stall a caller.

use sha2::{Digest, Sha256};
use thiserror::Error;

/// Maximum number of input bytes considered by [`HashingEmbedder::embed`].
pub const MAX_EMBED_BYTES: usize = 64 * 1024;

/// Smallest and largest accepted embedding dimension.
const DIM_RANGE: std::ops::RangeInclusive<usize> = 16..=4096;
/// Largest accepted character n-gram length.
const NGRAM_LIMIT: usize = 8;

/// Why an embedder could not be configured or its weights decoded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EmbedError {
    /// The dimension is outside `16..=4096`.
    #[error("embedding dimension must be within 16..=4096")]
    InvalidDim,
    /// The n-gram range is not `1 <= min <= max <= 8`.
    #[error("character n-gram range must satisfy 1 <= min <= max <= 8")]
    InvalidNgramRange,
    /// A weight is non-finite or negative, or a weight vector has the wrong
    /// length or byte encoding.
    #[error("weights must be finite, non-negative, and match the dimension")]
    InvalidWeights,
}

/// Maps text to a point in a fixed embedding space.
pub trait Embedder {
    /// Output dimension.
    fn dim(&self) -> usize;
    /// A unit-length vector of `dim()` finite f32 values, or the zero vector
    /// for text with no features.
    fn embed(&self, text: &str) -> Vec<f32>;
    /// Stable identifier of the embedding space (name + parameters). Vectors
    /// from different ids must not be compared.
    fn space_id(&self) -> String;
}

/// Parameters of a [`HashingEmbedder`].
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedConfig {
    /// Output dimension, `16..=4096`.
    pub dim: usize,
    /// Shortest character n-gram (over the `^word$` padded word).
    pub ngram_min: usize,
    /// Longest character n-gram.
    pub ngram_max: usize,
    /// Weight of word unigram features.
    pub word_weight: f32,
    /// Weight of word bigram features.
    pub bigram_weight: f32,
    /// Weight of character n-gram features.
    pub ngram_weight: f32,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            dim: 256,
            ngram_min: 3,
            ngram_max: 5,
            word_weight: 1.0,
            bigram_weight: 0.5,
            ngram_weight: 0.35,
        }
    }
}

/// Hashed lexical and subword embedder. See the module docs for scope.
#[derive(Debug, Clone)]
pub struct HashingEmbedder {
    config: EmbedConfig,
    bucket_weights: Option<Vec<f32>>,
    space_id: String,
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self::new(EmbedConfig::default()).expect("the default embed config is valid")
    }
}

impl HashingEmbedder {
    /// Validate `config` and build an embedder.
    pub fn new(config: EmbedConfig) -> Result<Self, EmbedError> {
        if !DIM_RANGE.contains(&config.dim) {
            return Err(EmbedError::InvalidDim);
        }
        if config.ngram_min < 1
            || config.ngram_min > config.ngram_max
            || config.ngram_max > NGRAM_LIMIT
        {
            return Err(EmbedError::InvalidNgramRange);
        }
        for w in [
            config.word_weight,
            config.bigram_weight,
            config.ngram_weight,
        ] {
            if !w.is_finite() || w < 0.0 {
                return Err(EmbedError::InvalidWeights);
            }
        }
        let mut embedder = Self {
            config,
            bucket_weights: None,
            space_id: String::new(),
        };
        embedder.space_id = embedder.compute_space_id();
        Ok(embedder)
    }

    /// Attach per-bucket weights. `weights.len()` must equal `dim`; every
    /// value must be finite and `>= 0`. Each weight multiplies its bucket
    /// before normalisation, and the weights are part of
    /// [`Embedder::space_id`].
    pub fn with_bucket_weights(mut self, weights: Vec<f32>) -> Result<Self, EmbedError> {
        if weights.len() != self.config.dim || weights.iter().any(|w| !w.is_finite() || *w < 0.0) {
            return Err(EmbedError::InvalidWeights);
        }
        self.bucket_weights = Some(weights);
        self.space_id = self.compute_space_id();
        Ok(self)
    }

    fn compute_space_id(&self) -> String {
        let c = &self.config;
        let bw = match &self.bucket_weights {
            None => "none".to_string(),
            Some(w) => {
                // Normalise -0.0 to 0.0 so equal weights give equal ids.
                let normalised: Vec<f32> =
                    w.iter().map(|x| if *x == 0.0 { 0.0 } else { *x }).collect();
                let digest = Sha256::digest(encode_weights(&normalised));
                digest[..16].iter().map(|b| format!("{b:02x}")).collect()
            }
        };
        format!(
            "hashing-v1:dim={}:ngram={}-{}:w={:08x},{:08x},{:08x}:bw={}",
            c.dim,
            c.ngram_min,
            c.ngram_max,
            c.word_weight.to_bits(),
            c.bigram_weight.to_bits(),
            c.ngram_weight.to_bits(),
            bw
        )
    }

    /// Add one feature class: sort the hashes, group equal ones, and add
    /// `class_weight * (1 + ln(count))` to each signed bucket.
    fn accumulate(&self, acc: &mut [f64], hashes: &mut [u64], class_weight: f32) {
        if class_weight == 0.0 {
            return;
        }
        hashes.sort_unstable();
        let class_weight = f64::from(class_weight);
        let dim = self.config.dim as u64;
        let mut i = 0;
        while i < hashes.len() {
            let h = hashes[i];
            let mut j = i + 1;
            while j < hashes.len() && hashes[j] == h {
                j += 1;
            }
            let tf = (j - i) as f64;
            let value = class_weight * (1.0 + ln_det(tf));
            let idx = (h % dim) as usize;
            if h >> 63 == 1 {
                acc[idx] -= value;
            } else {
                acc[idx] += value;
            }
            i = j;
        }
    }
}

impl Embedder for HashingEmbedder {
    fn dim(&self) -> usize {
        self.config.dim
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let dim = self.config.dim;
        let text = truncate_on_char_boundary(text, MAX_EMBED_BYTES);
        let words = tokenize(text);

        let mut word_hashes: Vec<u64> = Vec::with_capacity(words.len());
        let mut bigram_hashes: Vec<u64> = Vec::with_capacity(words.len());
        let mut ngram_hashes: Vec<u64> = Vec::new();
        let mut padded: Vec<char> = Vec::new();

        for (i, word) in words.iter().enumerate() {
            word_hashes.push(finish(fnv1a(
                fnv1a(FNV_OFFSET, &[CLASS_WORD]),
                word.as_bytes(),
            )));
            if let Some(next) = words.get(i + 1) {
                let mut h = fnv1a(FNV_OFFSET, &[CLASS_BIGRAM]);
                h = fnv1a(h, word.as_bytes());
                h = fnv1a(h, &[0]);
                h = fnv1a(h, next.as_bytes());
                bigram_hashes.push(finish(h));
            }
            padded.clear();
            padded.push('^');
            padded.extend(word.chars());
            padded.push('$');
            for n in self.config.ngram_min..=self.config.ngram_max {
                if padded.len() < n {
                    break;
                }
                for window in padded.windows(n) {
                    let mut h = fnv1a(FNV_OFFSET, &[CLASS_NGRAM, n as u8]);
                    let mut buf = [0u8; 4];
                    for ch in window {
                        h = fnv1a(h, ch.encode_utf8(&mut buf).as_bytes());
                    }
                    ngram_hashes.push(finish(h));
                }
            }
        }

        let mut acc = vec![0.0f64; dim];
        self.accumulate(&mut acc, &mut word_hashes, self.config.word_weight);
        self.accumulate(&mut acc, &mut bigram_hashes, self.config.bigram_weight);
        self.accumulate(&mut acc, &mut ngram_hashes, self.config.ngram_weight);

        if let Some(weights) = &self.bucket_weights {
            for (a, w) in acc.iter_mut().zip(weights) {
                *a *= f64::from(*w);
            }
        }
        let norm_sq: f64 = acc.iter().map(|v| v * v).sum();
        let norm = norm_sq.sqrt();
        if norm <= 0.0 || !norm.is_finite() {
            return vec![0.0; dim];
        }
        acc.iter().map(|v| (v / norm) as f32).collect()
    }

    fn space_id(&self) -> String {
        self.space_id.clone()
    }
}

/// Cosine similarity of two vectors. `0.0` on length mismatch or when
/// either vector has zero norm.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())) as f32
}

/// Little-endian f32 bytes, for storing pluggable bucket weights.
pub fn encode_weights(weights: &[f32]) -> Vec<u8> {
    weights.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// Inverse of [`encode_weights`]. Rejects a length not divisible by 4.
pub fn decode_weights(bytes: &[u8]) -> Result<Vec<f32>, EmbedError> {
    if !bytes.len().is_multiple_of(4) {
        return Err(EmbedError::InvalidWeights);
    }
    let (chunks, _) = bytes.as_chunks::<4>();
    Ok(chunks.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
/// Distinct prefix bytes per feature class, so a word and an n-gram with
/// the same text hash differently by construction.
const CLASS_WORD: u8 = 1;
const CLASS_BIGRAM: u8 = 2;
const CLASS_NGRAM: u8 = 3;

/// FNV-1a over `bytes`, continuing from state `h`.
fn fnv1a(mut h: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// 64-bit finaliser (murmur3 fmix64) so the low bits (bucket) and the top
/// bit (sign) are well mixed.
fn finish(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}

/// Natural log for `x >= 1` using only IEEE `+ - * /`, so the result is
/// identical on every platform (the platform `ln` is not guaranteed to be).
fn ln_det(x: f64) -> f64 {
    let mut m = x;
    let mut e = 0.0f64;
    while m >= 2.0 {
        m *= 0.5;
        e += 1.0;
    }
    // ln(m) = 2 * atanh((m - 1) / (m + 1)), m in [1, 2) so |t| <= 1/3.
    let t = (m - 1.0) / (m + 1.0);
    let t2 = t * t;
    let mut term = t;
    let mut sum = 0.0f64;
    for k in 0..24 {
        sum += term / f64::from(2 * k + 1);
        term *= t2;
    }
    e * std::f64::consts::LN_2 + 2.0 * sum
}

fn truncate_on_char_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Words: alphanumeric runs (Unicode-aware, so non-English text keeps its
/// features), lowercased, split at lower->Upper camelCase boundaries before
/// lowercasing. Every other character (including `_`, `-`, `.`) separates.
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase() && prev_lower && !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            cur.extend(ch.to_lowercase());
            prev_lower = ch.is_lowercase();
        } else {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(v: &[f32]) -> f64 {
        v.iter()
            .map(|x| f64::from(*x) * f64::from(*x))
            .sum::<f64>()
            .sqrt()
    }

    fn sim(e: &HashingEmbedder, a: &str, b: &str) -> f32 {
        cosine(&e.embed(a), &e.embed(b))
    }

    #[test]
    fn deterministic_and_golden() {
        let e = HashingEmbedder::default();
        let a = e.embed("hello world");
        assert_eq!(a, e.embed("hello world"));
        assert_eq!(a.len(), 256);
        let first4: Vec<(usize, f32)> = a
            .iter()
            .enumerate()
            .filter(|(_, v)| **v != 0.0)
            .take(4)
            .map(|(i, v)| (i, *v))
            .collect();
        assert_eq!(first4, GOLDEN_HELLO_WORLD.to_vec());
    }

    /// Regression anchor: first four non-zero entries of
    /// `embed("hello world")` under the default config, captured from this
    /// implementation's own output.
    const GOLDEN_HELLO_WORLD: [(usize, f32); 4] = [
        (3, 0.157_392_81),
        (5, 0.157_392_81),
        (8, 0.157_392_81),
        (13, -0.157_392_81),
    ];

    /// Independent known answers: published FNV-1a 64-bit vectors and
    /// murmur3 fmix64 values.
    #[test]
    fn primitives_match_published_vectors() {
        assert_eq!(fnv1a(FNV_OFFSET, b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(FNV_OFFSET, b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(FNV_OFFSET, b"foobar"), 0x8594_4171_f739_67e8);
        assert_eq!(finish(0), 0);
        assert_eq!(finish(1), 0xb456_bcfc_34c2_cb2c);
    }

    #[test]
    fn unit_length_and_zero_vectors() {
        let e = HashingEmbedder::default();
        for text in [
            "hello",
            "a",
            "The quick brown fox, jumps over 12 lazy dogs.",
        ] {
            let v = e.embed(text);
            assert!((norm(&v) - 1.0).abs() < 1e-5, "{text}");
            assert!(v.iter().all(|x| x.is_finite()));
        }
        for text in ["", "   ", "!!! ... --- ___ ???"] {
            let v = e.embed(text);
            assert_eq!(v.len(), 256);
            assert!(v.iter().all(|x| *x == 0.0), "{text:?}");
        }
    }

    #[test]
    fn ranking_sanity() {
        let e = HashingEmbedder::default();
        let q = "tokenizer split";
        assert!(
            sim(&e, q, "the tokenizer splits text") > sim(&e, q, "database migration verifier")
        );
        let unrelated = sim(&e, "migration", "database verifier");
        assert!(sim(&e, "migration", "migrations") > unrelated);
        assert!(sim(&e, "chekpoint", "checkpoint") > unrelated);
        assert!(sim(&e, "chekpoint", "checkpoint") > sim(&e, "chekpoint", "migration"));
        assert!(
            sim(&e, "search_state_entries", "searchStateEntries")
                > sim(&e, "search_state_entries", "unrelated words")
        );
        assert!(sim(&e, "search_state_entries", "searchStateEntries") > 0.99);
    }

    #[test]
    fn tokenizer_rules() {
        assert_eq!(
            tokenize("searchStateEntries"),
            ["search", "state", "entries"]
        );
        assert_eq!(tokenize("a_b-c.d"), ["a", "b", "c", "d"]);
        assert_eq!(tokenize("HTTPServer v2"), ["httpserver", "v2"]);
        assert_eq!(tokenize("Größe"), ["größe"]);
    }

    #[test]
    fn non_ascii_text_has_features() {
        let e = HashingEmbedder::default();
        for text in ["données été", "日本語のテキスト", "Привет мир", "größe"]
        {
            let v = e.embed(text);
            assert!(v.iter().any(|x| *x != 0.0), "{text}");
            assert!((norm(&v) - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn config_validation() {
        let bad = |f: fn(&mut EmbedConfig)| {
            let mut c = EmbedConfig::default();
            f(&mut c);
            HashingEmbedder::new(c).unwrap_err()
        };
        assert_eq!(bad(|c| c.dim = 15), EmbedError::InvalidDim);
        assert_eq!(bad(|c| c.dim = 4097), EmbedError::InvalidDim);
        assert_eq!(bad(|c| c.ngram_min = 0), EmbedError::InvalidNgramRange);
        assert_eq!(bad(|c| c.ngram_min = 6), EmbedError::InvalidNgramRange);
        assert_eq!(bad(|c| c.ngram_max = 9), EmbedError::InvalidNgramRange);
        assert_eq!(
            bad(|c| c.word_weight = f32::NAN),
            EmbedError::InvalidWeights
        );
        assert_eq!(bad(|c| c.bigram_weight = -1.0), EmbedError::InvalidWeights);
        assert_eq!(
            bad(|c| c.ngram_weight = f32::INFINITY),
            EmbedError::InvalidWeights
        );
        for dim in [16, 4096] {
            let c = EmbedConfig {
                dim,
                ..EmbedConfig::default()
            };
            assert_eq!(HashingEmbedder::new(c).unwrap().dim(), dim);
        }
    }

    #[test]
    fn bucket_weights() {
        let base = HashingEmbedder::default();
        let rejects = |w: Vec<f32>| {
            HashingEmbedder::default()
                .with_bucket_weights(w)
                .unwrap_err()
        };
        assert_eq!(rejects(vec![1.0; 255]), EmbedError::InvalidWeights);
        let mut nan = vec![1.0; 256];
        nan[3] = f32::NAN;
        assert_eq!(rejects(nan), EmbedError::InvalidWeights);
        let mut neg = vec![1.0; 256];
        neg[3] = -0.5;
        assert_eq!(rejects(neg), EmbedError::InvalidWeights);

        let ones = HashingEmbedder::default()
            .with_bucket_weights(vec![1.0; 256])
            .unwrap();
        assert_eq!(ones.embed("hello world"), base.embed("hello world"));
        assert_ne!(ones.space_id(), base.space_id());

        let mut w = vec![1.0f32; 256];
        for x in w.iter_mut().step_by(2) {
            *x = 0.0;
        }
        let masked = HashingEmbedder::default().with_bucket_weights(w).unwrap();
        let v = masked.embed("hello world and more words");
        assert_ne!(v, base.embed("hello world and more words"));
        assert!(v.iter().step_by(2).all(|x| *x == 0.0));
        assert!((norm(&v) - 1.0).abs() < 1e-5);
        assert_ne!(masked.space_id(), base.space_id());
        assert_ne!(masked.space_id(), ones.space_id());

        // Different weights never share an id; -0.0 and 0.0 do.
        let with_w = |w: Vec<f32>| {
            HashingEmbedder::default()
                .with_bucket_weights(w)
                .unwrap()
                .space_id()
        };
        let mut a = vec![1.0f32; 256];
        let mut b = a.clone();
        b[255] = 1.000_000_1;
        assert_ne!(with_w(a.clone()), with_w(b));
        a[7] = 0.0;
        let mut neg = a.clone();
        neg[7] = -0.0;
        assert_eq!(with_w(a), with_w(neg));

        let zero = HashingEmbedder::default()
            .with_bucket_weights(vec![0.0; 256])
            .unwrap();
        assert!(zero.embed("hello").iter().all(|x| *x == 0.0));
    }

    #[test]
    fn weights_codec() {
        let w = vec![0.0f32, 1.5, 0.25, 3.0e-3];
        let bytes = encode_weights(&w);
        assert_eq!(bytes.len(), 16);
        assert_eq!(decode_weights(&bytes).unwrap(), w);
        assert_eq!(decode_weights(&[]).unwrap(), Vec::<f32>::new());
        assert_eq!(
            decode_weights(&bytes[..15]),
            Err(EmbedError::InvalidWeights)
        );
    }

    #[test]
    fn cosine_edge_cases() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn truncates_huge_input() {
        let e = HashingEmbedder::default();
        let unit = "tokenizer splits text into words. ";
        let big = unit.repeat((1 << 20) / unit.len() + 1);
        assert!(big.len() > 1 << 20);
        let v = e.embed(&big);
        assert_eq!(v, e.embed(&big[..MAX_EMBED_BYTES]));

        // Multi-byte char straddling the cap: truncation backs up to a boundary.
        let mut s = "a".repeat(MAX_EMBED_BYTES - 1);
        s.push('é');
        s.push_str("zzz");
        assert_eq!(
            truncate_on_char_boundary(&s, MAX_EMBED_BYTES).len(),
            MAX_EMBED_BYTES - 1
        );
        let _ = e.embed(&s);
    }

    #[test]
    fn space_id_tracks_parameters() {
        let id = |c: EmbedConfig| HashingEmbedder::new(c).unwrap().space_id();
        let base = id(EmbedConfig::default());
        assert_eq!(base, id(EmbedConfig::default()));
        assert_eq!(base, HashingEmbedder::default().space_id());
        let with = |f: fn(&mut EmbedConfig)| {
            let mut c = EmbedConfig::default();
            f(&mut c);
            id(c)
        };
        assert_ne!(base, with(|c| c.dim = 512));
        assert_ne!(base, with(|c| c.ngram_min = 2));
        assert_ne!(base, with(|c| c.ngram_max = 4));
        assert_ne!(base, with(|c| c.word_weight = 2.0));
        assert_ne!(base, with(|c| c.bigram_weight = 0.0));
        assert_ne!(base, with(|c| c.ngram_weight = 0.5));
    }

    #[test]
    fn word_and_ngram_features_do_not_collide_by_construction() {
        let word = finish(fnv1a(fnv1a(FNV_OFFSET, &[CLASS_WORD]), b"abc"));
        let ngram = finish(fnv1a(fnv1a(FNV_OFFSET, &[CLASS_NGRAM, 3]), b"abc"));
        assert_ne!(word, ngram);
    }

    #[test]
    fn ln_det_is_accurate() {
        for tf in 1..=10_000u32 {
            let x = f64::from(tf);
            assert!((ln_det(x) - x.ln()).abs() < 1e-12, "{x}");
        }
    }
}
