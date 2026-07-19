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

/// For each value of the field, emit a single unprocessed token.
#[derive(Clone)]
pub(crate) struct RawTokenizer;

pub(crate) struct RawTokenStream {
    token: Token,
    has_token: bool,
}

impl Tokenizer for RawTokenizer {
    fn token_stream<'a>(&self, text: &'a str) -> BoxTokenStream<'a> {
        let token = Token::new(0, text.len(), 0, text.to_string(), 1).expect("len offsets");
        RawTokenStream {
            token,
            has_token: true,
        }
        .into()
    }
}

impl TokenStream for RawTokenStream {
    fn advance(&mut self) -> bool {
        let result = self.has_token;
        self.has_token = false;
        result
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
    use crate::project::text::tokenizer::{RawTokenizer, TextAnalyzer, Token};

    #[test]
    fn test_raw_tokenizer() {
        let tokens = token_stream_helper("Hello, happy tax payer!");
        assert_eq!(tokens.len(), 1);
        assert_token(&tokens[0], 0, "Hello, happy tax payer!", 0, 23);
    }

    fn token_stream_helper(text: &str) -> Vec<Token> {
        let a = TextAnalyzer::from(RawTokenizer);
        let mut token_stream = a.token_stream(text);
        let mut tokens: Vec<Token> = vec![];
        let mut add_token = |token: &Token| {
            tokens.push(token.clone());
        };
        token_stream.process(&mut add_token);
        tokens
    }
}
