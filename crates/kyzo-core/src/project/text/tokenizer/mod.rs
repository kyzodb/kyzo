/*
 * Code under this module is adapted from the Tantivy project
 * https://github.com/quickwit-oss/tantivy/tree/0.19.2/src/tokenizer
 * All code here are licensed under the MIT license, as in the original project.
 */
/*
 * The block above is CozoDB's original attribution, preserved verbatim.
 * This module ships inside KyzoDB, whose own code is under the
 * Mozilla Public License, v. 2.0 (https://mozilla.org/MPL/2.0/); the vendored
 * Tantivy code here remains MIT. Per-file provenance headers and the
 * `KYZO DEVIATION` marks are KyzoDB additions.
 */

//! Tokenizer are in charge of chopping text into a stream of tokens
//! ready for indexing.
//!
//! You must define in your schema which tokenizer should be used for
//! each of your fields :
//!
//! ```text
//! use tantivy::schema::*;
//!
//! let mut schema_builder = Schema::builder();
//!
//! let text_options = TextOptions::default()
//!     .set_indexing_options(
//!         TextFieldIndexing::default()
//!             .set_tokenizer("en_stem")
//!             .set_index_option(IndexRecordOption::Basic)
//!     )
//!     .set_stored();
//!
//! let id_options = TextOptions::default()
//!     .set_indexing_options(
//!         TextFieldIndexing::default()
//!             .set_tokenizer("raw_ids")
//!             .set_index_option(IndexRecordOption::WithFreqsAndPositions)
//!     )
//!     .set_stored();
//!
//! schema_builder.add_text_field("title", text_options.clone());
//! schema_builder.add_text_field("text", text_options);
//! schema_builder.add_text_field("uuid", id_options);
//!
//! let schema = schema_builder.build();
//! ```
//!
//! By default, `tantivy` offers the following tokenizers:
//!
//! ## `default`
//!
//! `default` is the tokenizer that will be used if you do not
//! assign a specific tokenizer to your text field.
//! It will chop your text on punctuation and whitespaces,
//! removes tokens that are longer than 40 chars, and lowercase your text.
//!
//! ## `raw`
//! Does not actual tokenizer your text. It keeps it entirely unprocessed.
//! It can be useful to index uuids, or urls for instance.
//!
//! ## `en_stem`
//!
//! In addition to what `default` does, the `en_stem` tokenizer also
//! apply stemming to your tokens. Stemming consists in trimming words to
//! remove their inflection. This tokenizer is slower than the default one,
//! but is recommended to improve recall.
//!
//!
//! # Custom tokenizers
//!
//! You can write your own tokenizer by implementing the [`Tokenizer`] trait
//! or you can extend an existing [`Tokenizer`] by chaining it with several
//! [`TokenFilter`]s.
//!
//! For instance, the `en_stem` is defined as follows.
//!
//! ```text
//! use tantivy::tokenizer::*;
//!
//! let en_stem = TextAnalyzer::from(SimpleTokenizer)
//!     .filter(RemoveLongFilter::limit(40))
//!     .filter(LowerCaser)
//!     .filter(Stemmer::new(Language::English));
//! ```
//!
//! Once your tokenizer is defined, you need to
//! register it with a name in your index's [`TokenizerManager`].
//!
//! ```text
//! # use tantivy::schema::Schema;
//! # use tantivy::tokenizer::*;
//! # use tantivy::Index;
//! #
//! let custom_en_tokenizer = SimpleTokenizer;
//! # let schema = Schema::builder().build();
//! let index = Index::create_in_ram(schema);
//! index.tokenizers()
//!      .register("custom_en", custom_en_tokenizer);
//! ```
//!
//! If you built your schema programmatically, a complete example
//! could like this for instance.
//!
//! Note that tokens with a len greater or equal to
//! [`MAX_TOKEN_LEN`].
//!
//! # Example
//!
//! ```text
//! use tantivy::schema::{Schema, IndexRecordOption, TextOptions, TextFieldIndexing};
//! use tantivy::tokenizer::*;
//! use tantivy::Index;
//!
//! let mut schema_builder = Schema::builder();
//! let text_field_indexing = TextFieldIndexing::default()
//!     .set_tokenizer("custom_en")
//!     .set_index_option(IndexRecordOption::WithFreqsAndPositions);
//! let text_options = TextOptions::default()
//!     .set_indexing_options(text_field_indexing)
//!     .set_stored();
//! schema_builder.add_text_field("title", text_options);
//! let schema = schema_builder.build();
//! let index = Index::create_in_ram(schema);
//!
//! // We need to register our tokenizer :
//! let custom_en_tokenizer = TextAnalyzer::from(SimpleTokenizer)
//!     .filter(RemoveLongFilter::limit(40))
//!     .filter(LowerCaser);
//! index
//!     .tokenizers()
//!     .register("custom_en", custom_en_tokenizer);
//! ```
mod alphanum_only;
mod ascii_folding_filter;
mod empty_tokenizer;
mod lower_caser;
mod ngram_tokenizer;
mod raw_tokenizer;
mod remove_long;
mod simple_tokenizer;
mod split_compound_words;
mod stemmer;
mod stop_word_filter;
#[cfg(test)]
mod tokenized_string;
mod tokenizer_impl;
mod whitespace_tokenizer;

pub(crate) use self::alphanum_only::AlphaNumOnlyFilter;
pub(crate) use self::ascii_folding_filter::AsciiFoldingFilter;
pub(crate) use self::lower_caser::LowerCaser;
pub(crate) use self::ngram_tokenizer::{NgramConfigError, NgramTokenizer};
pub(crate) use self::raw_tokenizer::RawTokenizer;
pub(crate) use self::remove_long::RemoveLongFilter;
pub(crate) use self::simple_tokenizer::SimpleTokenizer;
pub(crate) use self::split_compound_words::SplitCompoundWords;
pub(crate) use self::stemmer::{Language, Stemmer};
pub(crate) use self::stop_word_filter::StopWordFilter;
// pub(crate) use self::tokenized_string::{PreTokenizedStream, PreTokenizedString};
pub(crate) use self::tokenizer_impl::{
    BoxTokenFilter, BoxTokenStream, TextAnalyzer, Token, TokenFilter, TokenStream, Tokenizer,
    advance_char_indices_token,
};
pub(crate) use self::whitespace_tokenizer::WhitespaceTokenizer;

#[cfg(test)]
pub(crate) mod tests {
    // use super::{
    //     Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, Token,
    // };
    // use crate::project::text::tokenizer::TextAnalyzer;

    use crate::project::text::tokenizer::Token;

    /// This is a function that can be used in tests and doc tests
    /// to assert a token's correctness.
    pub(crate) fn assert_token(token: &Token, position: usize, text: &str, from: usize, to: usize) {
        assert_eq!(
            token.position, position,
            "expected position {} but {:?}",
            position, token
        );
        assert_eq!(token.text, text, "expected text {} but {:?}", text, token);
        assert_eq!(
            token.offset_from(),
            from,
            "expected offset_from {} but {:?}",
            from,
            token
        );
        assert_eq!(
            token.offset_to(),
            to,
            "expected offset_to {} but {:?}",
            to,
            token
        );
    }

    /// Law 5 sweep: no tokenizer pipeline may panic on hostile user text.
    /// Vendored code is not exempt — a panic is a panic wherever it was
    /// written. Every tokenizer crossed with a maximal filter stack is fed
    /// adversarial inputs and must run to exhaustion, and stay exhausted
    /// (the post-`None` advances are the regression test for the
    /// `StutteringIterator` underflow deviation in `ngram_tokenizer.rs`).
    mod hostile_input {
        use crate::DataValue;
        use crate::project::text::TokenizerConfig;
        use miette::{Result, miette};

        fn cfg(name: &str, args: Vec<DataValue>) -> Result<TokenizerConfig> {
            TokenizerConfig::admit(name, args).map_err(|e| miette!("{e}"))
        }

        fn tokenizer_configs() -> Result<Vec<TokenizerConfig>> {
            Ok(vec![
                cfg("Raw", vec![])?,
                cfg("Simple", vec![])?,
                cfg("Whitespace", vec![])?,
                cfg("NGram", vec![DataValue::from(1)])?,
                cfg(
                    "NGram",
                    vec![
                        DataValue::from(2),
                        DataValue::from(4),
                        DataValue::Bool(true),
                    ],
                )?,
                cfg("Cangjie", vec![])?,
                cfg("Cangjie", vec![DataValue::from("all")])?,
                cfg(
                    "Cangjie",
                    vec![DataValue::from("search"), DataValue::Bool(true)],
                )?,
                cfg("Cangjie", vec![DataValue::from("unicode")])?,
            ])
        }

        fn full_filter_stack() -> Result<Vec<TokenizerConfig>> {
            Ok(vec![
                cfg("RemoveLong", vec![DataValue::from(64)])?,
                cfg("AsciiFolding", vec![])?,
                cfg("Lowercase", vec![])?,
                cfg(
                    "SplitCompoundWords",
                    vec![DataValue::List(vec![
                        DataValue::from("foo"),
                        DataValue::from("bar"),
                    ])],
                )?,
                cfg("Stemmer", vec![DataValue::from("english")])?,
                cfg("Stopwords", vec![DataValue::from("en")])?,
                cfg("AlphaNumOnly", vec![])?,
            ])
        }

        fn hostile_inputs() -> Vec<(&'static str, String)> {
            let zalgo: String = "zalgo text"
                .chars()
                .flat_map(|c| {
                    std::iter::once(c).chain(
                        ['\u{0300}', '\u{0316}', '\u{0334}', '\u{0359}', '\u{036f}']
                            .into_iter()
                            .cycle()
                            .take(24),
                    )
                })
                .collect();
            let rtl = "\u{202e}مرحبا بالعالم\u{202c} שלום עולם \u{200f}mixed العرب end".to_string();
            let nulls = "abc\0def\0\0ghi\u{1}\u{7f}".to_string();
            let megabyte_token = "a".repeat(1 << 20);
            // Invalid UTF-8 byte sequences arrive as text only via lossy
            // decoding; what the tokenizers must survive is the resulting
            // replacement-character soup at odd codepoint boundaries.
            let lossy = String::from_utf8_lossy(&[
                0x66, 0x6f, 0x6f, 0xff, 0xfe, 0x62, 0x61, 0x72, 0x80, 0xc3, 0x28, 0xe2, 0x82, 0xf0,
                0x9f, 0x98,
            ])
            .into_owned();
            let combining_flood = format!("a{}", "\u{0301}".repeat(10_000));
            let emoji = "👨\u{200d}👩\u{200d}👧\u{200d}👦 🇺🇳 🏳️\u{200d}🌈 ﷽".to_string();
            let cjk = "中文测试abc漢字カタカナ한국어123".to_string();
            vec![
                ("zalgo", zalgo),
                ("rtl_with_overrides", rtl),
                ("null_bytes", nulls),
                ("one_megabyte_token", megabyte_token),
                ("utf8_lossy_replacement_soup", lossy),
                ("combining_flood", combining_flood),
                ("emoji_zwj", emoji),
                ("cjk_mixed", cjk),
                ("empty", String::new()),
                ("whitespace_only", " \t\r\n \u{3000}".to_string()),
            ]
        }

        /// Run one analyzer over one input to exhaustion; return how many
        /// tokens it produced. Panics (the thing under test) propagate.
        fn exhaust(analyzer: &crate::project::text::tokenizer::TextAnalyzer, text: &str) -> usize {
            let mut stream = analyzer.token_stream(text);
            let mut n = 0usize;
            while let Some(tok) = stream.next() {
                // Touch the token: its text is `String`, so being here at
                // all proves valid UTF-8. Offsets are not asserted against
                // the input here: AsciiFolding may legitimately grow text
                // beyond the input (single codepoints fold to multi-char
                // ASCII). Cangjie offsets are pinned in cangjie::tokenizer
                // tests (slice-address round-trip, including All overlaps).
                let _walked = tok.text.chars().count();
                n += 1;
            }
            // A finished stream must stay finished — advancing past the end
            // is where the vendored n-gram iterator underflowed.
            for _ in 0..4 {
                assert!(!stream.advance(), "stream resurrected after exhaustion");
            }
            n
        }

        #[test]
        fn no_pipeline_panics_on_hostile_text() -> Result<()> {
            let inputs = hostile_inputs();
            for tk in tokenizer_configs()? {
                let bare = tk.build(&[])?;
                let stacked = tk.build(&full_filter_stack()?)?;
                for (name, text) in &inputs {
                    exhaust(&bare, text);
                    exhaust(&stacked, text);
                    // Reused analyzer, second pass: streams are independent.
                    exhaust(&bare, text);
                    let label = format!("{}/{}", tk.name, name);
                    assert!(!label.is_empty());
                }
            }
            Ok(())
        }

        /// The stemmer must also survive hostile text *unfiltered* (no
        /// RemoveLong ahead of it), including the megabyte token.
        #[test]
        fn bare_stemmer_survives_hostile_text() -> Result<()> {
            let an = cfg("Simple", vec![])?.build(&[
                cfg("Lowercase", vec![])?,
                cfg("Stemmer", vec![DataValue::from("english")])?,
            ])?;
            for (_, text) in hostile_inputs() {
                exhaust(&an, &text);
            }
            Ok(())
        }
    }
}
