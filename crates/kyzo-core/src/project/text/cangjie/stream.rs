/*
 * Adapted from the Cang-jie project, MIT-licensed:
 * https://github.com/DCjanus/cang-jie
 * Vendored by way of CozoDB (Copyright 2023, The Cozo Project Authors).
 * This file remains under the MIT license; it ships inside KyzoDB, whose
 * own code is under the Mozilla Public License, v. 2.0
 * (https://mozilla.org/MPL/2.0/). The Cang-jie original carries no per-file
 * header; nothing was removed. Deviations are marked `KYZO DEVIATION`.
 */

use crate::project::text::tokenizer::Token;

/// One jieba cut with byte offsets proven against the source string.
#[derive(Debug)]
struct CangjieToken<'a> {
    word: &'a str,
    byte_start: usize,
    byte_end: usize,
}

impl<'a> CangjieToken<'a> {
    fn new(word: &'a str, byte_start: usize, byte_end: usize) -> Self {
        CangjieToken {
            word,
            byte_start,
            byte_end,
        }
    }
}

#[derive(Debug)]
pub(crate) struct CangjieTokenStream<'a> {
    result: Vec<CangjieToken<'a>>,
    index: usize,
    token: Token,
}

impl<'a> CangjieTokenStream<'a> {
    /// Create a token stream from slices borrowed from `src`.
    ///
    /// Every item in `result` must be a subslice of `src` so byte offsets can
    /// be derived from the slice addresses. Sequential length-accumulation
    /// (the vendored All/ForSearch bug) is refused: overlapping cuts would
    /// walk past `src.len()` and make `text[from..to]` reconstruct the wrong
    /// token — silent wrong answers on the one-law surface.
    ///
    /// KYZO DEVIATION: matches upstream cang-jie `new(src, result)` (slice
    /// address offsets). Position length is always 1 (upstream); the old
    /// vendored stream used `result.len()` for every token.
    pub(crate) fn new(src: &'a str, result: Vec<&'a str>) -> Self {
        let base = src.as_ptr().addr();
        let end = base + src.len();
        let result = result
            .into_iter()
            .map(|word| {
                let word_start = word.as_ptr().addr();
                let word_end = word_start + word.len();
                assert!(
                    base <= word_start && word_end <= end,
                    "token slice must be borrowed from src"
                );
                let byte_start = word_start - base;
                CangjieToken::new(word, byte_start, byte_start + word.len())
            })
            .collect();
        CangjieTokenStream {
            result,
            index: 0,
            token: Token::empty(),
        }
    }
}

impl<'a> crate::project::text::tokenizer::TokenStream for CangjieTokenStream<'a> {
    fn advance(&mut self) -> bool {
        if self.index < self.result.len() {
            let current = &self.result[self.index];

            self.token = Token::new(
                current.byte_start,
                current.byte_end,
                self.index,
                current.word.to_string(),
                1,
            )
            .expect("byte_end is byte_start + word len");

            self.index += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &crate::project::text::tokenizer::Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut crate::project::text::tokenizer::Token {
        &mut self.token
    }
}
