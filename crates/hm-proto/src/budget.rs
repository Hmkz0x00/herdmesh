//! Token budgets and a cheap, dependency-free token estimator.
//!
//! Every agent-facing surface has a cap here; builders must check their
//! output with [`estimate_tokens`] in a test.
//!
//! # Calibration
//! Measured against OpenAI `o200k_base` (tiktoken-rs, dev-dependency only)
//! on the fixture corpus in `tests/fixtures/budget/` (prose, Rust, JSON,
//! markdown task lists, git refs/paths, mixed logs/handoffs). See
//! `tests/budget_calibration.rs`; it fails if any fixture or the aggregate
//! drifts outside ±15%.
//!
//! A plain `chars / k` divisor cannot meet that bar: English prose is
//! ~4.5 chars/token while hashes, paths and JSON are ~2.2, so one divisor
//! was +38% on prose and −38% on refs at the same time. The default
//! estimator is therefore [`PretokenHeuristic`], which mimics o200k's
//! pre-tokenizer split and charges per piece. [`CharHeuristic`] is kept as
//! the simple fallback, calibrated on the aggregate only.
//!
//! Known limitation: non-Latin scripts (CJK etc.) are not in the corpus and
//! are likely under-estimated.

/// Room card (L0) cap.
pub const ROOM_CARD_MAX: usize = 150;
/// Default brief (L1) cap.
pub const BRIEF_DEFAULT_MAX: usize = 400;
/// Push notification cap.
pub const PING_MAX: usize = 30;
/// Decision rationale cap.
pub const RATIONALE_MAX: usize = 60;
/// Whole MCP tool list (names + descriptions + schemas) cap.
pub const TOOLS_TOTAL_MAX: usize = 600;

/// Estimates how many LLM tokens a string costs.
pub trait TokenEstimator {
    /// Estimated token count of `s`.
    fn estimate(&self, s: &str) -> usize;
}

/// `ceil(chars / chars_per_token)`. Accurate on aggregate, not per text type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharHeuristic {
    /// Characters per token.
    pub chars_per_token: f64,
}

impl CharHeuristic {
    /// Aggregate chars/token on the calibration corpus was 3.34 (the 3.6
    /// first guess under-counted by 7%); 3.3 rounds toward over-estimating.
    pub const CALIBRATED_CHARS_PER_TOKEN: f64 = 3.3;
}

impl Default for CharHeuristic {
    fn default() -> Self {
        Self {
            chars_per_token: Self::CALIBRATED_CHARS_PER_TOKEN,
        }
    }
}

impl TokenEstimator for CharHeuristic {
    fn estimate(&self, s: &str) -> usize {
        // Precision loss is irrelevant at budget sizes; result is >= 0.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let n = (s.chars().count() as f64 / self.chars_per_token).ceil() as usize;
        n
    }
}

/// Class-aware estimator that mirrors the o200k pre-tokenizer.
///
/// Text is split the way o200k's regex splits it, then each piece is charged:
/// - **word**: optional one-char prefix (space or punctuation) + `Upper* lower*`
///   letters, so `camelCase` is two pieces: 1 token, plus 1 per
///   [`WORD_CHARS_PER_EXTRA`](Self::WORD_CHARS_PER_EXTRA) letters beyond
///   the first [`WORD_FREE_CHARS`](Self::WORD_FREE_CHARS).
/// - **digits**: groups of up to 3, 1 token each.
/// - **punctuation run** (optional leading space): 1 token per
///   [`PUNCT_CHARS_PER_TOKEN`](Self::PUNCT_CHARS_PER_TOKEN) chars, rounded up.
/// - **newline run** / **indentation run**: 1 token each.
/// - **glued word**: a word whose prefix is punctuation (`/crates`,
///   `-proto`) with at least
///   [`GLUED_SPLIT_MIN_LETTERS`](Self::GLUED_SPLIT_MIN_LETTERS) letters costs
///   +1; o200k rarely has such pieces as a single token.
///
/// Constants were picked by sweeping on the corpus; aggregate error +0.6%,
/// biased slightly toward over-estimating so caps stay safe.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PretokenHeuristic;

impl PretokenHeuristic {
    /// Letters in a word piece covered by its first token.
    pub const WORD_FREE_CHARS: usize = 8;
    /// Additional letters per extra token in a long word piece.
    pub const WORD_CHARS_PER_EXTRA: usize = 3;
    /// Punctuation chars per token in a punctuation run.
    pub const PUNCT_CHARS_PER_TOKEN: usize = 3;
    /// A word with a punctuation prefix (not a space) and at least this
    /// many letters costs one extra token.
    pub const GLUED_SPLIT_MIN_LETTERS: usize = 5;

    fn word_cost(letters: usize) -> usize {
        1 + letters
            .saturating_sub(Self::WORD_FREE_CHARS)
            .div_ceil(Self::WORD_CHARS_PER_EXTRA)
    }

    fn punct_cost(len: usize) -> usize {
        len.div_ceil(Self::PUNCT_CHARS_PER_TOKEN)
    }
}

fn is_punct(c: char) -> bool {
    !c.is_whitespace() && !c.is_alphabetic() && !c.is_numeric()
}

fn scan_punct(cs: &[char], mut i: usize) -> usize {
    while i < cs.len() && is_punct(cs[i]) {
        i += 1;
    }
    i
}

/// Consumes `Upper* lower*` letters starting at `i`; returns the end index.
fn scan_letters(cs: &[char], mut i: usize) -> usize {
    while i < cs.len() && cs[i].is_alphabetic() && !cs[i].is_lowercase() {
        i += 1;
    }
    while i < cs.len() && cs[i].is_lowercase() {
        i += 1;
    }
    i
}

impl TokenEstimator for PretokenHeuristic {
    fn estimate(&self, s: &str) -> usize {
        let cs: Vec<char> = s.chars().collect();
        let mut tokens = 0;
        let mut i = 0;
        while i < cs.len() {
            let c = cs[i];
            let next_is_letter = cs.get(i + 1).is_some_and(|n| n.is_alphabetic());
            if c.is_alphabetic() || (c != '\n' && c != '\r' && !c.is_numeric() && next_is_letter) {
                // Word piece, with optional one-char prefix.
                let start = if c.is_alphabetic() { i } else { i + 1 };
                let end = scan_letters(&cs, start);
                tokens += Self::word_cost(end - start);
                // Words glued to a path/punct prefix (`/crates`, `-proto`)
                // rarely exist as one BPE token; o200k splits them.
                if is_punct(c) && end - start >= Self::GLUED_SPLIT_MIN_LETTERS {
                    tokens += 1;
                }
                i = end;
            } else if c.is_numeric() {
                let mut j = i;
                while j < cs.len() && j - i < 3 && cs[j].is_numeric() {
                    j += 1;
                }
                tokens += 1;
                i = j;
            } else if c == '\n' || c == '\r' || c.is_whitespace() {
                // Whitespace run: newline runs and indentation are one piece
                // each; a lone space before a word/punct merges into it.
                let mut j = i;
                while j < cs.len() && cs[j].is_whitespace() {
                    j += 1;
                }
                let run = &cs[i..j];
                let has_newline = run.iter().any(|&w| w == '\n' || w == '\r');
                let followed_by_text = j < cs.len();
                if has_newline || !followed_by_text || run.len() > 1 {
                    tokens += 1;
                    // Leave the last space as the next piece's prefix.
                    i = if !has_newline && followed_by_text && run.len() > 1 {
                        j - 1
                    } else {
                        j
                    };
                } else {
                    // Single space prefix: punct runs absorb it; words handle
                    // it via the prefix rule above.
                    i = j;
                    if cs.get(j).is_some_and(|&n| is_punct(n)) {
                        // ` "word`: o200k makes ` "` a punct piece, not a
                        // word prefix.
                        let end = scan_punct(&cs, j);
                        tokens += Self::punct_cost(end - j);
                        i = end;
                    }
                }
            } else {
                // Punctuation run.
                let end = scan_punct(&cs, i);
                tokens += Self::punct_cost(end - i);
                i = end;
            }
        }
        tokens
    }
}

/// Estimate tokens with the default estimator ([`PretokenHeuristic`]).
pub fn estimate_tokens(s: &str) -> usize {
    PretokenHeuristic.estimate(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_have_spec_values() {
        assert_eq!(ROOM_CARD_MAX, 150);
        assert_eq!(BRIEF_DEFAULT_MAX, 400);
        assert_eq!(PING_MAX, 30);
        assert_eq!(RATIONALE_MAX, 60);
        assert_eq!(TOOLS_TOTAL_MAX, 600);
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(CharHeuristic::default().estimate(""), 0);
    }

    #[test]
    fn char_heuristic_rounds_up() {
        let h = CharHeuristic {
            chars_per_token: 3.6,
        };
        assert_eq!(h.estimate("a"), 1);
        assert_eq!(h.estimate("abcd"), 2);
    }

    #[test]
    fn short_words_cost_one_each() {
        assert_eq!(estimate_tokens("the cat sat"), 3);
    }

    #[test]
    fn digits_group_by_three() {
        assert_eq!(estimate_tokens("1234567"), 3);
    }

    #[test]
    fn camel_case_splits() {
        assert_eq!(estimate_tokens("StoreError"), 2);
    }

    #[test]
    fn single_ping_line_is_tiny() {
        assert!(estimate_tokens("M1-2 done @ 8ca4588") <= PING_MAX);
    }
}
