/*
 * Adapted from the Tantivy project, MIT-licensed:
 * https://github.com/quickwit-oss/tantivy/tree/0.19.2/src/tokenizer
 * Vendored by way of CozoDB (Copyright 2023, The Cozo Project Authors).
 * This file remains under the MIT license; it ships inside KyzoDB, whose
 * own code is under the Mozilla Public License, v. 2.0
 * (https://mozilla.org/MPL/2.0/). The Tantivy original carries no per-file
 * header; nothing was removed. Deviations are marked `KYZO DEVIATION`.
 */

use std::cmp::Ordering;

use crate::project::text::tokenizer::{Token, TokenStream};

/// Pre-tokenized text: original string plus token list proven to cohere.
///
/// Fields are private; [`Self::admit`] is the only mint besides serde
/// (which routes through admit).
#[derive(Debug, Clone, serde_derive::Serialize, Eq, PartialEq)]
pub(crate) struct PreTokenizedString {
    text: String,
    tokens: Vec<Token>,
}

impl PreTokenizedString {
    /// Mint when every token's offsets lie in `text` and each token's
    /// `text` equals the corresponding substring.
    pub(crate) fn admit(text: String, tokens: Vec<Token>) -> Option<Self> {
        let len = text.len();
        for token in &tokens {
            let from = token.offset_from();
            let to = token.offset_to();
            if to > len || text.get(from..to) != Some(token.text.as_str()) {
                return None;
            }
        }
        Some(PreTokenizedString { text, tokens })
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn tokens(&self) -> &[Token] {
        &self.tokens
    }
}

impl Ord for PreTokenizedString {
    fn cmp(&self, other: &Self) -> Ordering {
        self.text.cmp(&other.text)
    }
}

impl PartialOrd for PreTokenizedString {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'de> serde::Deserialize<'de> for PreTokenizedString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde_derive::Deserialize)]
        struct Raw {
            text: String,
            tokens: Vec<Token>,
        }
        let raw = Raw::deserialize(deserializer)?;
        PreTokenizedString::admit(raw.text, raw.tokens).ok_or_else(|| {
            serde::de::Error::custom(
                "pre-tokenized text: token offsets or text incoherent with parent text",
            )
        })
    }
}

/// [`TokenStream`] implementation which wraps [`PreTokenizedString`]
pub(crate) struct PreTokenizedStream {
    tokenized_string: PreTokenizedString,
    current_token: i64,
}

impl From<PreTokenizedString> for PreTokenizedStream {
    fn from(s: PreTokenizedString) -> PreTokenizedStream {
        PreTokenizedStream {
            tokenized_string: s,
            current_token: -1,
        }
    }
}

impl TokenStream for PreTokenizedStream {
    fn advance(&mut self) -> bool {
        self.current_token += 1;
        let len = match i64::try_from(self.tokenized_string.tokens.len()) {
            Ok(n) => n,
            Err(_gt_i64) => i64::MAX,
        };
        self.current_token < len
    }

    fn token(&self) -> &Token {
        assert!(
            self.current_token >= 0,
            "TokenStream not initialized. You should call advance() at least once."
        );
        let idx = match usize::try_from(self.current_token) {
            Ok(i) => i,
            Err(_negative) => 0,
        };
        &self.tokenized_string.tokens[idx]
    }

    fn token_mut(&mut self) -> &mut Token {
        assert!(
            self.current_token >= 0,
            "TokenStream not initialized. You should call advance() at least once."
        );
        let idx = match usize::try_from(self.current_token) {
            Ok(i) => i,
            Err(_negative) => 0,
        };
        &mut self.tokenized_string.tokens[idx]
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::project::text::tokenizer::Token;
    use miette::{Result, miette};

    #[test]
    fn test_tokenized_stream() -> Result<()> {
        let tok_text = PreTokenizedString::admit(
            String::from("A a"),
            vec![
                Token::new(0, 1, 0, String::from("A"), 1)
                    .ok_or_else(|| miette!("lawful token A"))?,
                Token::new(2, 3, 1, String::from("a"), 1)
                    .ok_or_else(|| miette!("lawful token a"))?,
            ],
        )
        .ok_or_else(|| miette!("lawful pretokenized string"))?;

        let mut token_stream = PreTokenizedStream::from(tok_text.clone());

        for expected_token in tok_text.tokens() {
            assert!(token_stream.advance());
            assert_eq!(token_stream.token(), expected_token);
        }
        assert!(!token_stream.advance());
        Ok(())
    }

    #[test]
    fn admit_refuses_incoherent_token_text() -> Result<()> {
        assert!(
            PreTokenizedString::admit(
                String::from("hello"),
                vec![
                    Token::new(0, 5, 0, String::from("world"), 1)
                        .ok_or_else(|| miette!("lawful token world"))?,
                ],
            )
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn admit_refuses_offsets_past_text() -> Result<()> {
        assert!(
            PreTokenizedString::admit(
                String::from("hi"),
                vec![
                    Token::new(0, 10, 0, String::from("hi"), 1)
                        .ok_or_else(|| miette!("lawful token hi"))?,
                ],
            )
            .is_none()
        );
        Ok(())
    }
}
