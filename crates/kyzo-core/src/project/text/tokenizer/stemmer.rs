/*
 * Adapted from the Tantivy project, MIT-licensed:
 * https://github.com/quickwit-oss/tantivy/tree/0.19.2/src/tokenizer
 * Vendored by way of CozoDB (Copyright 2023, The Cozo Project Authors).
 * This file remains under the MIT license; it ships inside KyzoDB, whose
 * own code is under the Mozilla Public License, v. 2.0
 * (https://mozilla.org/MPL/2.0/). The Tantivy original carries no per-file
 * header; nothing was removed. Deviations are marked `KYZO DEVIATION`.
 */

use std::borrow::Cow;
use std::mem;

use rust_stemmers::{self, Algorithm};

use super::{Token, TokenFilter, TokenStream};
use crate::project::text::tokenizer::BoxTokenStream;

/// Available stemmer languages.
#[derive(Debug, serde_derive::Serialize, serde_derive::Deserialize, Eq, PartialEq, Copy, Clone)]
#[allow(missing_docs)]
pub(crate) enum Language {
    Arabic,
    Danish,
    Dutch,
    English,
    Finnish,
    French,
    German,
    Greek,
    Hungarian,
    Italian,
    Norwegian,
    Portuguese,
    Romanian,
    Russian,
    Spanish,
    Swedish,
    Tamil,
    Turkish,
}

impl Language {
    fn algorithm(self) -> Algorithm {
        use self::Language::*;
        match self {
            Arabic => Algorithm::Arabic,
            Danish => Algorithm::Danish,
            Dutch => Algorithm::Dutch,
            English => Algorithm::English,
            Finnish => Algorithm::Finnish,
            French => Algorithm::French,
            German => Algorithm::German,
            Greek => Algorithm::Greek,
            Hungarian => Algorithm::Hungarian,
            Italian => Algorithm::Italian,
            Norwegian => Algorithm::Norwegian,
            Portuguese => Algorithm::Portuguese,
            Romanian => Algorithm::Romanian,
            Russian => Algorithm::Russian,
            Spanish => Algorithm::Spanish,
            Swedish => Algorithm::Swedish,
            Tamil => Algorithm::Tamil,
            Turkish => Algorithm::Turkish,
        }
    }
}

/// `Stemmer` token filter. Several languages are supported, see [`Language`] for the available
/// languages.
/// Tokens are expected to be lowercased beforehand.
#[derive(Clone)]
pub(crate) struct Stemmer {
    stemmer_algorithm: Algorithm,
}

impl Stemmer {
    /// Creates a new `Stemmer` [`TokenFilter`] for a given language algorithm.
    pub(crate) fn new(language: Language) -> Stemmer {
        Stemmer {
            stemmer_algorithm: language.algorithm(),
        }
    }
}

impl Default for Stemmer {
    /// Creates a new `Stemmer` [`TokenFilter`] for [`Language::English`].
    fn default() -> Self {
        Stemmer::new(Language::English)
    }
}

impl TokenFilter for Stemmer {
    fn transform<'a>(&self, token_stream: BoxTokenStream<'a>) -> BoxTokenStream<'a> {
        let inner_stemmer = rust_stemmers::Stemmer::create(self.stemmer_algorithm);
        BoxTokenStream::from(StemmerTokenStream {
            tail: token_stream,
            stemmer: inner_stemmer,
            buffer: String::new(),
        })
    }
}

pub(crate) struct StemmerTokenStream<'a> {
    tail: BoxTokenStream<'a>,
    stemmer: rust_stemmers::Stemmer,
    buffer: String,
}

impl<'a> TokenStream for StemmerTokenStream<'a> {
    fn advance(&mut self) -> bool {
        if !self.tail.advance() {
            return false;
        }
        let token = self.tail.token_mut();
        let stemmed_str = self.stemmer.stem(&token.text);
        match stemmed_str {
            Cow::Owned(stemmed_str) => token.text = stemmed_str,
            Cow::Borrowed(stemmed_str) => {
                self.buffer.clear();
                self.buffer.push_str(stemmed_str);
                mem::swap(&mut token.text, &mut self.buffer);
            }
        }
        true
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
    use super::*;
    use crate::project::text::tokenizer::{SimpleTokenizer, TextAnalyzer};

    fn stem_one(lang: Language, word: &str) -> String {
        let an = TextAnalyzer::from(SimpleTokenizer).filter(Stemmer::new(lang));
        let mut stream = an.token_stream(word);
        let text = stream
            .next()
            .unwrap_or_else(|| panic!("expected a token for {word:?}"))
            .text
            .clone();
        assert!(stream.next().is_none(), "single-word input");
        text
    }

    /// Per-language content pins — snowball stems, not length theater.
    #[test]
    fn stemmer_per_language_pairs() {
        assert_eq!(stem_one(Language::English, "running"), "run");
        assert_eq!(stem_one(Language::English, "dogs"), "dog");
        assert_eq!(stem_one(Language::German, "häuser"), "haus");
        assert_eq!(stem_one(Language::French, "chevaux"), "cheval");
        assert_eq!(stem_one(Language::Spanish, "caminando"), "camin");
        assert_eq!(stem_one(Language::Italian, "parlando"), "parl");
        assert_eq!(stem_one(Language::Portuguese, "falando"), "fal");
        assert_eq!(stem_one(Language::Dutch, "huizen"), "huiz");
        assert_eq!(stem_one(Language::Swedish, "hoppar"), "hopp");
        assert_eq!(stem_one(Language::Russian, "бегает"), "бега");
        assert_eq!(stem_one(Language::Danish, "løber"), "løb");
        assert_eq!(stem_one(Language::Finnish, "talossa"), "talo");
        assert_eq!(stem_one(Language::Norwegian, "hopper"), "hopp");
        assert_eq!(stem_one(Language::Romanian, "vorbind"), "vorb");
        assert_eq!(stem_one(Language::Greek, "τρέχει"), "τρεχ");
        assert_eq!(stem_one(Language::Hungarian, "házak"), "ház");
    }
}
