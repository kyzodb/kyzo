/*
 * Adapted from the Tantivy project, MIT-licensed:
 * https://github.com/quickwit-oss/tantivy/tree/0.19.2/src/tokenizer
 * Vendored by way of CozoDB (Copyright 2023, The Cozo Project Authors).
 * This file remains under the MIT license; it ships inside KyzoDB, whose
 * own code is under the Mozilla Public License, v. 2.0
 * (https://mozilla.org/MPL/2.0/). The Tantivy original carries no per-file
 * header; nothing was removed. Deviations are marked `KYZO DEVIATION`.
 */

use super::{BoxTokenStream, Token, TokenFilter, TokenStream};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use miette::{Diagnostic, Result};
use thiserror::Error;

/// Named refusal when the compound-words dictionary automaton cannot be built.
#[derive(Debug, Error, Diagnostic)]
#[error("Failed to load compound-words dictionary")]
#[diagnostic(code(fts::compound_dict_build))]
pub(crate) struct CompoundDictionaryBuildFailed;

/// A [`TokenFilter`] which splits compound words into their parts
/// based on a given dictionary.
///
/// Words only will be split if they can be fully decomposed into
/// consecutive matches into the given dictionary.
///
/// This is mostly useful to split [compound nouns][compound] common to many
/// Germanic languages into their constituents.
///
/// # Example
///
/// The quality of the dictionary determines the quality of the splits,
/// e.g. the missing stem "back" of "backen" implies that "brotbackautomat"
/// is not split in the following example.
///
/// ```text
/// use tantivy::tokenizer::{SimpleTokenizer, SplitCompoundWords, TextAnalyzer};
///
/// let tokenizer =
///        TextAnalyzer::from(SimpleTokenizer).filter(SplitCompoundWords::from_dictionary([
///            "dampf", "schiff", "fahrt", "brot", "backen", "automat",
///        ]));
///
/// let mut stream = tokenizer.token_stream("dampfschifffahrt");
/// assert_eq!(stream.next().unwrap().text, "dampf");
/// assert_eq!(stream.next().unwrap().text, "schiff");
/// assert_eq!(stream.next().unwrap().text, "fahrt");
/// assert_eq!(stream.next(), None);
///
/// let mut stream = tokenizer.token_stream("brotbackautomat");
/// assert_eq!(stream.next().unwrap().text, "brotbackautomat");
/// assert_eq!(stream.next(), None);
/// ```
///
/// [compound]: https://en.wikipedia.org/wiki/Compound_(linguistics)
#[derive(Clone)]
pub(crate) struct SplitCompoundWords {
    dict: AhoCorasick,
}

impl SplitCompoundWords {
    /// Create a filter from a given dictionary.
    ///
    /// The dictionary will be used to construct an [`AhoCorasick`] automaton
    /// with reasonable defaults. See [`from_automaton`][Self::from_automaton] if
    /// more control over its construction is required.
    pub(crate) fn from_dictionary<I, P>(dict: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<[u8]>,
    {
        let dict = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostLongest)
            .build(dict)
            .map_err(|_| CompoundDictionaryBuildFailed)?;

        Ok(Self::from_automaton(dict))
    }
}

impl SplitCompoundWords {
    /// Create a filter from a given automaton.
    ///
    /// The automaton should use one of the leftmost-first match kinds
    /// and it should not be anchored.
    pub(crate) fn from_automaton(dict: AhoCorasick) -> Self {
        Self { dict }
    }
}

impl TokenFilter for SplitCompoundWords {
    fn transform<'a>(&self, stream: BoxTokenStream<'a>) -> BoxTokenStream<'a> {
        BoxTokenStream::from(SplitCompoundWordsTokenStream {
            dict: self.dict.clone(),
            tail: stream,
            cuts: Vec::new(),
            parts: Vec::new(),
        })
    }
}

struct SplitCompoundWordsTokenStream<'a> {
    dict: AhoCorasick,
    tail: BoxTokenStream<'a>,
    cuts: Vec<usize>,
    parts: Vec<Token>,
}

impl<'a> SplitCompoundWordsTokenStream<'a> {
    // Will use `self.cuts` to fill `self.parts` if `self.tail.token()`
    // can fully be split into consecutive matches against `self.dict`.
    fn split(&mut self) {
        let token = self.tail.token();
        let mut text = token.text.as_str();

        self.cuts.clear();
        let mut pos = 0;

        for match_ in self.dict.find_iter(text) {
            if pos != match_.start() {
                break;
            }

            self.cuts.push(pos);
            pos = match_.end();
        }

        if pos == token.text.len() {
            // Fill `self.parts` in reverse order,
            // so that `self.parts.pop()` yields
            // the tokens in their original order.
            //
            // KYZO DEVIATION: each part gets byte offsets inside the parent
            // token (parent.from + cut .. + part.len). The vendored code
            // copied the full parent range onto every part — wrong for
            // highlighting and for multi-byte (umlaut) compounds.
            let parent_from = token.offset_from();
            for &cut in self.cuts.iter().rev() {
                let (head, tail) = text.split_at(cut);
                let part_from = parent_from + cut;
                let part_to = part_from + tail.len();
                text = head;
                self.parts.push(
                    Token::new(
                        part_from,
                        part_to,
                        token.position,
                        tail.to_owned(),
                        token.position_length,
                    )
                    .expect("part offsets derived from proven parent + cut"),
                );
            }
        }
    }
}

impl<'a> TokenStream for SplitCompoundWordsTokenStream<'a> {
    fn advance(&mut self) -> bool {
        self.parts.pop();

        if !self.parts.is_empty() {
            return true;
        }

        if !self.tail.advance() {
            return false;
        }

        // Will yield either `self.parts.last()` or
        // `self.tail.token()` if it could not be split.
        self.split();
        true
    }

    fn token(&self) -> &Token {
        self.parts.last().unwrap_or_else(|| self.tail.token())
    }

    fn token_mut(&mut self) -> &mut Token {
        self.parts
            .last_mut()
            .unwrap_or_else(|| self.tail.token_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::text::tokenizer::{SimpleTokenizer, TextAnalyzer, TokenStream, Tokenizer};

    /// Umlaut compounds: cuts are UTF-8 byte offsets, and each part's
    /// `offset_from`/`offset_to` reconstruct that part from the source.
    #[test]
    fn split_compound_umlaut_byte_offsets_round_trip() {
        // "über" is 5 bytes (ü = C3 BC); "fahrt" is 5 ASCII bytes.
        let text = "überfahrt";
        assert_eq!("über".len(), 5);
        let tokenizer = TextAnalyzer::from(SimpleTokenizer)
            .filter(SplitCompoundWords::from_dictionary(["über", "fahrt"]).unwrap());
        let mut stream = tokenizer.token_stream(text);
        let first = stream.next().expect("über");
        assert_eq!(first.text, "über");
        assert_eq!(first.offset_from(), 0);
        assert_eq!(first.offset_to(), 5);
        assert_eq!(&text[first.offset_from()..first.offset_to()], "über");
        let second = stream.next().expect("fahrt");
        assert_eq!(second.text, "fahrt");
        assert_eq!(second.offset_from(), 5);
        assert_eq!(second.offset_to(), 10);
        assert_eq!(&text[second.offset_from()..second.offset_to()], "fahrt");
        assert!(stream.next().is_none());
    }

    #[test]
    fn splitting_compound_words_works() {
        let tokenizer = TextAnalyzer::from(SimpleTokenizer)
            .filter(SplitCompoundWords::from_dictionary(["foo", "bar"]).unwrap());

        {
            let mut stream = tokenizer.token_stream("");
            assert_eq!(stream.next(), None);
        }

        {
            let mut stream = tokenizer.token_stream("foo bar");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next(), None);
        }

        {
            let mut stream = tokenizer.token_stream("foobar");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next(), None);
        }

        {
            let mut stream = tokenizer.token_stream("foobarbaz");
            assert_eq!(stream.next().unwrap().text, "foobarbaz");
            assert_eq!(stream.next(), None);
        }

        {
            let mut stream = tokenizer.token_stream("baz foobar qux");
            assert_eq!(stream.next().unwrap().text, "baz");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next().unwrap().text, "qux");
            assert_eq!(stream.next(), None);
        }

        {
            let mut stream = tokenizer.token_stream("foobar foobar");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next(), None);
        }

        {
            let mut stream = tokenizer.token_stream("foobar foo bar foobar");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next(), None);
        }

        {
            let mut stream = tokenizer.token_stream("foobazbar foo bar foobar");
            assert_eq!(stream.next().unwrap().text, "foobazbar");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next(), None);
        }

        {
            let mut stream = tokenizer.token_stream("foobar qux foobar");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next().unwrap().text, "qux");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next(), None);
        }

        {
            let mut stream = tokenizer.token_stream("barfoo");
            assert_eq!(stream.next().unwrap().text, "bar");
            assert_eq!(stream.next().unwrap().text, "foo");
            assert_eq!(stream.next(), None);
        }
    }
}
