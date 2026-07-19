/*
 * Adapted from the Tantivy project, MIT-licensed:
 * https://github.com/quickwit-oss/tantivy/tree/0.19.2/src/tokenizer
 * Vendored by way of CozoDB (Copyright 2023, The Cozo Project Authors).
 * This file remains under the MIT license; it ships inside KyzoDB, whose
 * own code is under the Mozilla Public License, v. 2.0
 * (https://mozilla.org/MPL/2.0/). The Tantivy original carries no per-file
 * header; nothing was removed. Deviations are marked `KYZO DEVIATION`.
 */

use crate::project::text::tokenizer::{BoxTokenStream, Token, TokenStream, Tokenizer};

#[derive(Clone)]
pub(crate) struct EmptyTokenizer;

impl Tokenizer for EmptyTokenizer {
    fn token_stream<'a>(&self, _text: &'a str) -> BoxTokenStream<'a> {
        EmptyTokenStream::default().into()
    }
}

#[derive(Default)]
struct EmptyTokenStream {
    token: Token,
}

impl TokenStream for EmptyTokenStream {
    fn advance(&mut self) -> bool {
        false
    }

    fn token(&self) -> &super::Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut super::Token {
        &mut self.token
    }
}

#[cfg(test)]
mod tests {
    use crate::project::text::tokenizer::Tokenizer;

    #[test]
    fn test_empty_tokenizer() {
        let tokenizer = super::EmptyTokenizer;
        let mut empty = tokenizer.token_stream("whatever string");
        assert!(!empty.advance());
    }
}
