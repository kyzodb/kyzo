/*
 * Adapted from the Tantivy project, MIT-licensed:
 * https://github.com/quickwit-oss/tantivy/tree/0.19.2/src/tokenizer
 * Vendored by way of CozoDB (Copyright 2023, The Cozo Project Authors).
 * This file remains under the MIT license; it ships inside KyzoDB, whose
 * own code is under the Mozilla Public License, v. 2.0
 * (https://mozilla.org/MPL/2.0/). The Tantivy original carries no per-file
 * header; nothing was removed. Deviations are marked `KYZO DEVIATION`.
 */

use super::{Token, TokenStream, Tokenizer};
use crate::project::text::tokenizer::BoxTokenStream;

/// Unlawful n-gram bounds — named refuse, never `assert!` / panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NgramConfigError {
    /// `min_gram` must be ≥ 1.
    MinGramZero,
    /// `min_gram` must not exceed `max_gram`.
    MinGreaterThanMax,
}

/// Tokenize the text by splitting words into n-grams of the given size(s)
///
/// With this tokenizer, the `position` is always 0.
/// Beware however, in presence of multiple value for the same field,
/// the position will be `POSITION_GAP * index of value`.
///
/// Example 1: `hello` would be tokenized as (min_gram: 2, max_gram: 3, prefix_only: false)
///
/// | Term     | he  | hel | el  | ell | ll  | llo | lo |
/// |----------|-----|-----|-----|-----|-----|-----|----|
/// | Position | 0   | 0   | 0   | 0   | 0   | 0   | 0  |
/// | Offsets  | 0,2 | 0,3 | 1,3 | 1,4 | 2,4 | 2,5 | 3,5|
///
/// Example 2: `hello` would be tokenized as (min_gram: 2, max_gram: 5, prefix_only: **true**)
///
/// | Term     | he  | hel | hell  | hello |
/// |----------|-----|-----|-------|-------|
/// | Position | 0   | 0   | 0     | 0     |
/// | Offsets  | 0,2 | 0,3 | 0,4   | 0,5   |
///
/// Example 3: `hεllo` (non-ascii) would be tokenized as (min_gram: 2, max_gram: 5, prefix_only:
/// **true**)
///
/// | Term     | hε  | hεl | hεll  | hεllo |
/// |----------|-----|-----|-------|-------|
/// | Position | 0   | 0   | 0     | 0     |
/// | Offsets  | 0,3 | 0,4 | 0,5   | 0,6   |
///
/// # Example
///
/// ```text
/// use tantivy::tokenizer::*;
///
/// let tokenizer = NgramTokenizer::new(2, 3, false);
/// let mut stream = tokenizer.token_stream("hello");
/// {
///     let token = stream.next().unwrap();
///     assert_eq!(token.text, "he");
///     assert_eq!(token.offset_from, 0);
///     assert_eq!(token.offset_to, 2);
/// }
/// {
///   let token = stream.next().unwrap();
///     assert_eq!(token.text, "hel");
///     assert_eq!(token.offset_from, 0);
///     assert_eq!(token.offset_to, 3);
/// }
/// {
///   let token = stream.next().unwrap();
///     assert_eq!(token.text, "el");
///     assert_eq!(token.offset_from, 1);
///     assert_eq!(token.offset_to, 3);
/// }
/// {
///   let token = stream.next().unwrap();
///     assert_eq!(token.text, "ell");
///     assert_eq!(token.offset_from, 1);
///     assert_eq!(token.offset_to, 4);
/// }
/// {
///   let token = stream.next().unwrap();
///     assert_eq!(token.text, "ll");
///     assert_eq!(token.offset_from, 2);
///     assert_eq!(token.offset_to, 4);
/// }
/// {
///   let token = stream.next().unwrap();
///     assert_eq!(token.text, "llo");
///     assert_eq!(token.offset_from, 2);
///     assert_eq!(token.offset_to, 5);
/// }
/// {
///   let token = stream.next().unwrap();
///   assert_eq!(token.text, "lo");
///   assert_eq!(token.offset_from, 3);
///   assert_eq!(token.offset_to, 5);
/// }
/// assert!(stream.next().is_none());
/// ```
#[derive(Clone)]
pub(crate) struct NgramTokenizer {
    /// min size of the n-gram
    min_gram: usize,
    /// max size of the n-gram
    max_gram: usize,
    /// if true, will only parse the leading edge of the input
    prefix_only: bool,
}

impl NgramTokenizer {
    /// Configures a new Ngram tokenizer. Refuses zero/`min > max` bounds.
    pub(crate) fn try_new(
        min_gram: usize,
        max_gram: usize,
        prefix_only: bool,
    ) -> Result<NgramTokenizer, NgramConfigError> {
        if min_gram == 0 {
            return Err(NgramConfigError::MinGramZero);
        }
        if min_gram > max_gram {
            return Err(NgramConfigError::MinGreaterThanMax);
        }
        Ok(NgramTokenizer {
            min_gram,
            max_gram,
            prefix_only,
        })
    }

    /// Create a `NGramTokenizer` which generates tokens for all inner ngrams.
    ///
    /// This is as opposed to only prefix ngrams.
    #[cfg(test)]
    pub(crate) fn all_ngrams(
        min_gram: usize,
        max_gram: usize,
    ) -> Result<NgramTokenizer, NgramConfigError> {
        Self::try_new(min_gram, max_gram, false)
    }

    /// Create a `NGramTokenizer` which only generates tokens for the
    /// prefix ngrams.
    #[cfg(test)]
    pub(crate) fn prefix_only(
        min_gram: usize,
        max_gram: usize,
    ) -> Result<NgramTokenizer, NgramConfigError> {
        Self::try_new(min_gram, max_gram, true)
    }
}

/// TokenStream associate to the `NgramTokenizer`
pub(crate) struct NgramTokenStream<'a> {
    /// parameters
    ngram_charidx_iterator: StutteringIterator<CodepointFrontiers<'a>>,
    /// true if the NgramTokenStream is in prefix mode.
    prefix_only: bool,
    /// input
    text: &'a str,
    /// output
    token: Token,
}

impl Tokenizer for NgramTokenizer {
    fn token_stream<'a>(&self, text: &'a str) -> BoxTokenStream<'a> {
        From::from(NgramTokenStream {
            ngram_charidx_iterator: StutteringIterator::new(
                CodepointFrontiers::for_str(text),
                self.min_gram,
                self.max_gram,
            ),
            prefix_only: self.prefix_only,
            text,
            token: Token::empty(),
        })
    }
}

impl<'a> TokenStream for NgramTokenStream<'a> {
    fn advance(&mut self) -> bool {
        if let Some((offset_from, offset_to)) = self.ngram_charidx_iterator.next() {
            if self.prefix_only && offset_from > 0 {
                return false;
            }
            self.token.position = 0;
            match self.token.set_offsets(offset_from, offset_to) {
                token_ref => core::mem::drop(token_ref),
            }
            self.token.text.clear();
            self.token.text.push_str(&self.text[offset_from..offset_to]);
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        &self.token
    }
    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

/// This iterator takes an underlying Iterator
/// and emits all of the pairs `(a,b)` such that
/// a and b are items emitted by the iterator at
/// an interval between `min_gram` and `max_gram`.
///
/// The elements are emitted in the order of appearance
/// of `a` first, `b` then.
///
/// See `test_stutterring_iterator` for an example of its
/// output.
struct StutteringIterator<T> {
    underlying: T,
    min_gram: usize,
    max_gram: usize,

    memory: Vec<usize>,
    cursor: usize,
    gram_len: usize,
}

impl<T> StutteringIterator<T>
where
    T: Iterator<Item = usize>,
{
    pub(crate) fn new(
        mut underlying: T,
        min_gram: usize,
        max_gram: usize,
    ) -> StutteringIterator<T> {
        assert!(min_gram > 0);
        let memory: Vec<usize> = (&mut underlying).take(max_gram + 1).collect();
        if memory.len() <= min_gram {
            // returns an empty iterator
            StutteringIterator {
                underlying,
                min_gram: 1,
                max_gram: 0,
                memory,
                cursor: 0,
                gram_len: 0,
            }
        } else {
            StutteringIterator {
                underlying,
                min_gram,
                max_gram: memory.len() - 1,
                memory,
                cursor: 0,
                gram_len: min_gram,
            }
        }
    }
}

impl<T> Iterator for StutteringIterator<T>
where
    T: Iterator<Item = usize>,
{
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        if self.gram_len > self.max_gram {
            // we have exhausted all options
            // starting at `self.memory[self.cursor]`.
            //
            // Time to advance.
            self.gram_len = self.min_gram;
            if let Some(next_val) = self.underlying.next() {
                self.memory[self.cursor] = next_val;
            } else {
                // KYZO DEVIATION: the vendored source had `self.max_gram -= 1`,
                // which underflows if `next()` is called again after exhaustion
                // with `min_gram == 1`: a panic in a debug build, and in a
                // release build a silent wrap to `usize::MAX` that resurrects
                // the exhausted stream to emit garbage pairs (worse). No
                // in-tree caller advances a finished stream, but neither
                // failure mode is acceptable in the engine under adversarial
                // call order; saturate instead, which covers both
                // (`max_gram < min_gram` already means "done", and stays so).
                self.max_gram = match self.max_gram.checked_sub(1) {
                    Some(v) => v,
                    None => 0,
                };
            }
            self.cursor += 1;
            if self.cursor >= self.memory.len() {
                self.cursor = 0;
            }
        }
        if self.max_gram < self.min_gram {
            return None;
        }
        let start = self.memory[self.cursor % self.memory.len()];
        let stop = self.memory[(self.cursor + self.gram_len) % self.memory.len()];
        self.gram_len += 1;
        Some((start, stop))
    }
}

/// Emits all of the offsets where a codepoint starts
/// or a codepoint ends.
///
/// By convention, we emit `[0]` for the empty string.
struct CodepointFrontiers<'a> {
    s: &'a str,
    next_el: Option<usize>,
}

impl<'a> CodepointFrontiers<'a> {
    fn for_str(s: &'a str) -> Self {
        CodepointFrontiers {
            s,
            next_el: Some(0),
        }
    }
}

impl<'a> Iterator for CodepointFrontiers<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        let offset = self.next_el?;
        if self.s.is_empty() {
            self.next_el = None;
        } else {
            let first_codepoint_width = utf8_codepoint_width(self.s.as_bytes()[0]);
            self.s = &self.s[first_codepoint_width..];
            self.next_el = Some(offset + first_codepoint_width);
        }
        Some(offset)
    }
}

const CODEPOINT_UTF8_WIDTH: [u8; 16] = [1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 3, 4];

// Number of bytes to encode a codepoint in UTF-8 given
// the first byte.
//
// To do that we count the number of higher significant bits set to `1`.
fn utf8_codepoint_width(b: u8) -> usize {
    let higher_4_bits = usize::from(b) >> 4;
    usize::from(CODEPOINT_UTF8_WIDTH[higher_4_bits])
}

#[cfg(test)]
mod tests {

    use super::{CodepointFrontiers, NgramConfigError, NgramTokenizer, StutteringIterator, utf8_codepoint_width};
    use crate::project::text::tokenizer::tests::assert_token;
    use crate::project::text::tokenizer::{BoxTokenStream, Token, Tokenizer};

    fn test_helper(mut tokenizer: BoxTokenStream<'_>) -> Vec<Token> {
        let mut tokens: Vec<Token> = vec![];
        tokenizer.process(&mut |token: &Token| tokens.push(token.clone()));
        tokens
    }

    #[test]
    fn test_utf8_codepoint_width() {
        // 0xxx
        for i in 0..128 {
            assert_eq!(utf8_codepoint_width(i), 1);
        }
        // 110xx
        for i in (128 | 64)..(128 | 64 | 32) {
            assert_eq!(utf8_codepoint_width(i), 2);
        }
        // 1110xx
        for i in (128 | 64 | 32)..(128 | 64 | 32 | 16) {
            assert_eq!(utf8_codepoint_width(i), 3);
        }
        // 1111xx
        for i in (128 | 64 | 32 | 16)..256 {
            let b = match u8::try_from(i) {
                Ok(v) => v,
                Err(_gt_u8) => continue,
            };
            assert_eq!(utf8_codepoint_width(b), 4);
        }
    }

    #[test]
    fn test_codepoint_frontiers() {
        assert_eq!(CodepointFrontiers::for_str("").collect::<Vec<_>>(), vec![0]);
        assert_eq!(
            CodepointFrontiers::for_str("abcd").collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        assert_eq!(
            CodepointFrontiers::for_str("aあ").collect::<Vec<_>>(),
            vec![0, 1, 4]
        );
    }

    #[test]
    fn test_ngram_tokenizer_1_2_false() -> Result<(), NgramConfigError> {
        let tokens = test_helper(NgramTokenizer::all_ngrams(1, 2)?.token_stream("hello"));
        assert_eq!(tokens.len(), 9);
        assert_token(&tokens[0], 0, "h", 0, 1);
        assert_token(&tokens[1], 0, "he", 0, 2);
        assert_token(&tokens[2], 0, "e", 1, 2);
        assert_token(&tokens[3], 0, "el", 1, 3);
        assert_token(&tokens[4], 0, "l", 2, 3);
        assert_token(&tokens[5], 0, "ll", 2, 4);
        assert_token(&tokens[6], 0, "l", 3, 4);
        assert_token(&tokens[7], 0, "lo", 3, 5);
        assert_token(&tokens[8], 0, "o", 4, 5);
        Ok(())
    }

    #[test]
    fn test_ngram_tokenizer_min_max_equal() -> Result<(), NgramConfigError> {
        let tokens = test_helper(NgramTokenizer::all_ngrams(3, 3)?.token_stream("hello"));
        assert_eq!(tokens.len(), 3);
        assert_token(&tokens[0], 0, "hel", 0, 3);
        assert_token(&tokens[1], 0, "ell", 1, 4);
        assert_token(&tokens[2], 0, "llo", 2, 5);
        Ok(())
    }

    #[test]
    fn test_ngram_tokenizer_2_5_prefix() -> Result<(), NgramConfigError> {
        let tokens = test_helper(NgramTokenizer::prefix_only(2, 5)?.token_stream("frankenstein"));
        assert_eq!(tokens.len(), 4);
        assert_token(&tokens[0], 0, "fr", 0, 2);
        assert_token(&tokens[1], 0, "fra", 0, 3);
        assert_token(&tokens[2], 0, "fran", 0, 4);
        assert_token(&tokens[3], 0, "frank", 0, 5);
        Ok(())
    }

    #[test]
    fn test_ngram_non_ascii_1_2() -> Result<(), NgramConfigError> {
        let tokens = test_helper(NgramTokenizer::all_ngrams(1, 2)?.token_stream("hεllo"));
        assert_eq!(tokens.len(), 9);
        assert_token(&tokens[0], 0, "h", 0, 1);
        assert_token(&tokens[1], 0, "hε", 0, 3);
        assert_token(&tokens[2], 0, "ε", 1, 3);
        assert_token(&tokens[3], 0, "εl", 1, 4);
        assert_token(&tokens[4], 0, "l", 3, 4);
        assert_token(&tokens[5], 0, "ll", 3, 5);
        assert_token(&tokens[6], 0, "l", 4, 5);
        assert_token(&tokens[7], 0, "lo", 4, 6);
        assert_token(&tokens[8], 0, "o", 5, 6);
        Ok(())
    }

    #[test]
    fn test_ngram_non_ascii_2_5_prefix() -> Result<(), NgramConfigError> {
        let tokens = test_helper(NgramTokenizer::prefix_only(2, 5)?.token_stream("hεllo"));
        assert_eq!(tokens.len(), 4);
        assert_token(&tokens[0], 0, "hε", 0, 3);
        assert_token(&tokens[1], 0, "hεl", 0, 4);
        assert_token(&tokens[2], 0, "hεll", 0, 5);
        assert_token(&tokens[3], 0, "hεllo", 0, 6);
        Ok(())
    }

    #[test]
    fn test_ngram_empty() -> Result<(), NgramConfigError> {
        let tokens = test_helper(NgramTokenizer::all_ngrams(1, 5)?.token_stream(""));
        assert!(tokens.is_empty());
        let tokens = test_helper(NgramTokenizer::all_ngrams(2, 5)?.token_stream(""));
        assert!(tokens.is_empty());
        Ok(())
    }

    #[test]
    fn test_ngram_min_gram_zero_refuses() {
        assert!(matches!(
            NgramTokenizer::all_ngrams(0, 2),
            Err(NgramConfigError::MinGramZero)
        ));
    }

    #[test]
    fn test_ngram_min_greater_than_max_refuses() {
        assert!(matches!(
            NgramTokenizer::all_ngrams(2, 1),
            Err(NgramConfigError::MinGreaterThanMax)
        ));
    }

    #[test]
    fn test_stutterring_iterator_empty() {
        let rg: Vec<usize> = vec![0];
        let mut it = StutteringIterator::new(rg.into_iter(), 1, 2);
        assert_eq!(it.next(), None);
    }

    #[test]
    fn test_stutterring_iterator() {
        let mut it = StutteringIterator::new(0..10, 1, 2);
        assert_eq!(it.next(), Some((0, 1)));
        assert_eq!(it.next(), Some((0, 2)));
        assert_eq!(it.next(), Some((1, 2)));
        assert_eq!(it.next(), Some((1, 3)));
        assert_eq!(it.next(), Some((2, 3)));
        assert_eq!(it.next(), Some((2, 4)));
        assert_eq!(it.next(), Some((3, 4)));
        assert_eq!(it.next(), Some((3, 5)));
        assert_eq!(it.next(), Some((4, 5)));
        assert_eq!(it.next(), Some((4, 6)));
        assert_eq!(it.next(), Some((5, 6)));
        assert_eq!(it.next(), Some((5, 7)));
        assert_eq!(it.next(), Some((6, 7)));
        assert_eq!(it.next(), Some((6, 8)));
        assert_eq!(it.next(), Some((7, 8)));
        assert_eq!(it.next(), Some((7, 9)));
        assert_eq!(it.next(), Some((8, 9)));
        assert_eq!(it.next(), None);
    }
}
