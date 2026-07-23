/*
 * Adapted from the Tantivy project, MIT-licensed:
 * https://github.com/quickwit-oss/tantivy/tree/0.19.2/src/tokenizer
 * Vendored by way of CozoDB (Copyright 2023, The Cozo Project Authors).
 * This file remains under the MIT license; it ships inside KyzoDB, whose
 * own code is under the Mozilla Public License, v. 2.0
 * (https://mozilla.org/MPL/2.0/). The Tantivy original carries no per-file
 * header; nothing was removed. Deviations are marked `KYZO DEVIATION`.
 */

//! # Example
//! ```text
//! use tantivy::tokenizer::*;
//!
//! let tokenizer = TextAnalyzer::from(SimpleTokenizer)
//!   .filter(RemoveLongFilter::limit(5));
//!
//! let mut stream = tokenizer.token_stream("toolong nice");
//! // because `toolong` is more than 5 characters, it is filtered
//! // out of the token stream.
//! assert_eq!(stream.next().unwrap().text, "nice");
//! assert!(stream.next().is_none());
//! ```
use super::{Token, TokenFilter, TokenStream};
use crate::project::text::tokenizer::BoxTokenStream;

/// `RemoveLongFilter` removes tokens that are longer
/// than a given number of bytes (in UTF-8 representation).
///
/// It is especially useful when indexing unconstrained content.
/// e.g. Mail containing base-64 encoded pictures etc.
#[derive(Clone)]
pub(crate) struct RemoveLongFilter {
    length_limit: usize,
}

impl RemoveLongFilter {
    /// Creates a `RemoveLongFilter` given a limit in bytes of the UTF-8 representation.
    pub(crate) fn limit(length_limit: usize) -> RemoveLongFilter {
        RemoveLongFilter { length_limit }
    }
}

impl<'a> RemoveLongFilterStream<'a> {
    /// Keep tokens whose UTF-8 byte length is at most the limit ("longer than"
    /// is removed; exact-boundary length == limit is kept).
    fn predicate(&self, token: &Token) -> bool {
        token.text.len() <= self.token_length_limit
    }
}

impl TokenFilter for RemoveLongFilter {
    fn transform<'a>(&self, token_stream: BoxTokenStream<'a>) -> BoxTokenStream<'a> {
        BoxTokenStream::from(RemoveLongFilterStream {
            token_length_limit: self.length_limit,
            tail: token_stream,
        })
    }
}

pub(crate) struct RemoveLongFilterStream<'a> {
    token_length_limit: usize,
    tail: BoxTokenStream<'a>,
}

impl<'a> TokenStream for RemoveLongFilterStream<'a> {
    fn advance(&mut self) -> bool {
        while self.tail.advance() {
            if self.predicate(self.tail.token()) {
                return true;
            }
        }
        false
    }

    fn token(&self) -> &Token {
        self.tail.token()
    }

    fn token_mut(&mut self) -> &mut Token {
        self.tail.token_mut()
    }
}

#[cfg(test)]
mod tests {
    use crate::project::text::tokenizer::tests::assert_token;
    use crate::project::text::tokenizer::{RemoveLongFilter, SimpleTokenizer, TextAnalyzer, Token};

    #[test]
    fn test_remove_long() {
        let tokens = token_stream_helper("hello tantivy, happy searching!", 6);
        assert_eq!(tokens.len(), 2);
        assert_token(&tokens[0], 0, "hello", 0, 5);
        assert_token(&tokens[1], 2, "happy", 15, 20);
    }

    /// Exact-boundary pin: length == limit is kept ("longer than", not
    /// "longer than or equal"). Six ASCII letters survive limit 6; seven die.
    #[test]
    fn remove_long_keeps_exact_boundary_token() {
        let tokens = token_stream_helper("abcdef abcdefg", 6);
        assert_eq!(tokens.len(), 1, "exact-6 kept, 7 dropped: {tokens:?}");
        assert_eq!(tokens[0].text, "abcdef");
        assert_eq!(tokens[0].text.len(), 6);
    }

        fn token_stream_helper(text: &str, limit: usize) -> Vec<Token> {
        let a = TextAnalyzer::from(SimpleTokenizer).filter(RemoveLongFilter::limit(limit));
        crate::project::text::tokenizer::tests::collect_tokens(a.token_stream(text))
    }
}
