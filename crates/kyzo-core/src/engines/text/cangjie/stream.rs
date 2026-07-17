/*
 * Adapted from the Cang-jie project, MIT-licensed:
 * https://github.com/DCjanus/cang-jie
 * Vendored by way of CozoDB (Copyright 2023, The Cozo Project Authors).
 * This file remains under the MIT license; it ships inside KyzoDB, whose
 * own code is under the Mozilla Public License, v. 2.0
 * (https://mozilla.org/MPL/2.0/). The Cang-jie original carries no per-file
 * header; nothing was removed. Deviations are marked `KYZO DEVIATION`.
 */

use crate::engines::text::tokenizer::Token;

#[derive(Debug)]
pub(crate) struct CangjieTokenStream<'a> {
    result: Vec<&'a str>,
    // Begin with 1
    index: usize,
    offset_from: usize,
    token: Token,
}

impl<'a> CangjieTokenStream<'a> {
    pub(crate) fn new(result: Vec<&'a str>) -> Self {
        CangjieTokenStream {
            result,
            index: 0,
            offset_from: 0,
            token: Token::default(),
        }
    }
}

impl<'a> crate::engines::text::tokenizer::TokenStream for CangjieTokenStream<'a> {
    fn advance(&mut self) -> bool {
        if self.index < self.result.len() {
            let current_word = self.result[self.index];
            let offset_to = self.offset_from + current_word.len();

            self.token = Token::new(
                self.offset_from,
                offset_to,
                self.index,
                current_word.to_string(),
                self.result.len(),
            );

            self.index += 1;
            self.offset_from = offset_to;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &crate::engines::text::tokenizer::Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut crate::engines::text::tokenizer::Token {
        &mut self.token
    }
}
