/*
 * Adapted from the Tantivy project, MIT-licensed:
 * https://github.com/quickwit-oss/tantivy/tree/0.19.2/src/tokenizer
 * Vendored by way of CozoDB (Copyright 2023, The Cozo Project Authors).
 * This file remains under the MIT license; it ships inside KyzoDB, whose
 * own code is under the Mozilla Public License, v. 2.0
 * (https://mozilla.org/MPL/2.0/). The Tantivy original carries no per-file
 * header; nothing was removed. Deviations are marked `KYZO DEVIATION`.
 */

use std::str::CharIndices;

use super::{BoxTokenStream, Token, TokenStream, Tokenizer};

/// Tokenize the text by splitting on whitespaces and punctuation.
#[derive(Clone)]
pub(crate) struct SimpleTokenizer;

pub(crate) struct SimpleTokenStream<'a> {
    text: &'a str,
    chars: CharIndices<'a>,
    token: Token,
}

impl Tokenizer for SimpleTokenizer {
    fn token_stream<'a>(&self, text: &'a str) -> BoxTokenStream<'a> {
        BoxTokenStream::from(SimpleTokenStream {
            text,
            chars: text.char_indices(),
            token: Token::empty(),
        })
    }
}

impl<'a> TokenStream for SimpleTokenStream<'a> {
    fn advance(&mut self) -> bool {
        super::advance_char_indices_token(
            self.text,
            &mut self.chars,
            &mut self.token,
            char::is_alphanumeric,
        )
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

#[cfg(test)]
mod tests {
    use crate::project::text::tokenizer::tests::assert_token;
    use crate::project::text::tokenizer::{SimpleTokenizer, TextAnalyzer, Token};

    #[test]
    fn test_simple_tokenizer() {
        let tokens = token_stream_helper("Hello, happy tax payer!");
        assert_eq!(tokens.len(), 4);
        assert_token(&tokens[0], 0, "Hello", 0, 5);
        assert_token(&tokens[1], 1, "happy", 7, 12);
        assert_token(&tokens[2], 2, "tax", 13, 16);
        assert_token(&tokens[3], 3, "payer", 17, 22);
    }

        fn token_stream_helper(text: &str) -> Vec<Token> {
        let a = TextAnalyzer::from(SimpleTokenizer);
        crate::project::text::tokenizer::tests::collect_tokens(a.token_stream(text))
    }
}
