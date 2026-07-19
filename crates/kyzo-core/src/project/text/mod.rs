/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): `TokenizerConfig::validate` is new (a config is *provable* at
 * index-definition time, not only at first use); `TokenizerCache` recovers
 * from lock poisoning instead of unwrapping; the `indexing` submodule is a
 * seam until the operator tier lands.
 */

//! Full-text search: tokenizer configuration and the analyzer cache.
//!
//! A [`TokenizerConfig`] is pure data — a proven stage name plus
//! [`DataValue`] arguments — stored in an FTS index manifest. Unknown names
//! are refused at [`TokenizerConfig::admit`] (and on serde decode), so they
//! are unstorable. It becomes a runnable [`TextAnalyzer`] only through
//! [`TokenizerConfig::build`], which still refuses malformed arguments.
//!
//! Two moments of truth, by design:
//!
//! - **Name / store time**: [`TokenizerConfig::admit`] (parse, builder, serde)
//!   refuses unknown stage names before a config can exist.
//! - **Definition time**: the operator tier (`::fts create`) calls
//!   [`TokenizerConfig::validate`] so unlawful *arguments* are refused before
//!   the manifest is written.
//! - **Use time**: [`TokenizerConfig::build`] stays lazily fallible for
//!   argument shape — a manifest written by an older or foreign build is
//!   data, and data is never trusted to be well-formed just because it was
//!   once stored.

use crate::DataValue;
use crate::project::text::cangjie::tokenizer::CangJieTokenizer;
use crate::project::text::tokenizer::{
    AlphaNumOnlyFilter, AsciiFoldingFilter, BoxTokenFilter, Language, LowerCaser, NgramTokenizer,
    RawTokenizer, RemoveLongFilter, SimpleTokenizer, SplitCompoundWords, Stemmer, StopWordFilter,
    TextAnalyzer, Tokenizer, WhitespaceTokenizer,
};
use jieba_rs::Jieba;
use miette::{Diagnostic, Result, ensure};
use serde::Deserialize;
use sha2::digest::FixedOutput;
use sha2::{Digest, Sha256};
use smartstring::{LazyCompact, SmartString};
use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use thiserror::Error;
use kyzo_model::data_value_any;

pub(crate) mod ast;
pub(crate) mod cangjie;
pub(crate) mod fts;
pub(crate) mod tokenizer;

/// The stored description of one FTS index: where it hangs, how documents
/// are extracted, and how text is tokenized. Persisted in the catalog; the
/// `tokenizer`/`filters` configs are re-`build()`-able at any later time.
#[derive(Debug, Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct FtsIndexManifest {
    pub(crate) base_relation: SmartString<LazyCompact>,
    pub(crate) index_name: SmartString<LazyCompact>,
    /// The row-extraction expression as a PARSED typed substance. Serde
    /// round-trips it through the value plane's `Expr` codec (op arity is
    /// re-proven on decode), so the catalog never holds an un-parseable
    /// extractor and there is no build-time re-parse of source text.
    pub(crate) extractor: crate::data::expr::Expr,
    pub(crate) tokenizer: TokenizerConfig,
    pub(crate) filters: Vec<TokenizerConfig>,
}

/// A tokenizer or token-filter *as configuration*: a name and its arguments,
/// exactly as written in the index definition. Pure data — see the module
/// docs for when it is proven runnable.
///
/// Name proof: private fields; the only mints are [`TokenizerConfig::admit`]
/// (and serde deserialize through that door). Unknown stage names are
/// unstorable. Argument shape is still proven at [`validate`] / [`build`].
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde_derive::Serialize)]
pub struct TokenizerConfig {
    name: SmartString<LazyCompact>,
    args: Vec<DataValue>,
}

/// Unknown tokenizer / token-filter name refused at the admit door.
#[derive(Debug, Error, Diagnostic)]
#[error("unknown tokenizer or token-filter name: {0}")]
#[diagnostic(code(fts::unknown_stage_name))]
pub struct UnknownTokenizerStageName(pub SmartString<LazyCompact>);

fn is_known_stage_name(name: &str) -> bool {
    matches!(
        name,
        "Raw"
            | "Simple"
            | "Whitespace"
            | "NGram"
            | "Cangjie"
            | "AlphaNumOnly"
            | "AsciiFolding"
            | "LowerCase"
            | "Lowercase"
            | "RemoveLong"
            | "SplitCompoundWords"
            | "Stemmer"
            | "Stopwords"
    )
}

/// `RemoveLong` configured with a non-positive length. Typed because the
/// failure it forecloses was *silent*: the CozoDB original cast the argument
/// straight through `as usize`, so a negative length wrapped to (near)
/// `usize::MAX` — a filter that never removes anything while the manifest
/// claims one that does. Raised on both moments of truth, since `validate`
/// delegates to `build`, which calls `construct_token_filter`.
#[derive(Debug, Error, Diagnostic)]
#[error("RemoveLong length must be a positive integer, got {0}")]
#[diagnostic(code(fts::remove_long_non_positive))]
#[diagnostic(help(
    "RemoveLong drops every token longer than the given length; a \
     non-positive length would either drop everything or (as a wrapped \
     negative) nothing at all."
))]
struct NonPositiveRemoveLong(i64);

/// Named refusal when tokenizer / token-filter arguments are unlawful.
/// String/`miette!`/`bail!` identity is unrepresentable — every build path
/// picks a variant.
#[derive(Debug, Error, Diagnostic)]
pub(crate) enum TokenizerBuildRefusal {
    #[error("First argument `min_gram` must be an integer")]
    #[diagnostic(code(fts::ngram_min_not_int))]
    NgramMinNotInt,
    #[error("Second argument `max_gram` must be an integer")]
    #[diagnostic(code(fts::ngram_max_not_int))]
    NgramMaxNotInt,
    #[error("Third argument `prefix_only` must be a boolean")]
    #[diagnostic(code(fts::ngram_prefix_not_bool))]
    NgramPrefixNotBool,
    #[error("min_gram must be >= 1")]
    #[diagnostic(code(fts::ngram_min_too_small))]
    NgramMinTooSmall,
    #[error("max_gram must be >= min_gram")]
    #[diagnostic(code(fts::ngram_max_below_min))]
    NgramMaxBelowMin,
    #[error("Second argument `use_hmm` to Cangjie must be a boolean")]
    #[diagnostic(code(fts::cangjie_hmm_not_bool))]
    CangjieHmmNotBool,
    #[error("First argument `kind` to Cangjie must be a string")]
    #[diagnostic(code(fts::cangjie_kind_not_str))]
    CangjieKindNotStr,
    #[error("Unknown Cangjie kind: {0}")]
    #[diagnostic(code(fts::cangjie_unknown_kind))]
    CangjieUnknownKind(SmartString<LazyCompact>),
    #[error("Unknown tokenizer: {0}")]
    #[diagnostic(code(fts::unknown_tokenizer))]
    UnknownTokenizer(SmartString<LazyCompact>),
    #[error("Missing first argument `min_length`")]
    #[diagnostic(code(fts::remove_long_missing_arg))]
    RemoveLongMissingArg,
    #[error("First argument `min_length` must be an integer")]
    #[diagnostic(code(fts::remove_long_not_int))]
    RemoveLongNotInt,
    #[error("Missing first argument `compound_words_list`")]
    #[diagnostic(code(fts::compound_missing_arg))]
    CompoundMissingArg,
    #[error("First argument `compound_words_list` must be a list of strings")]
    #[diagnostic(code(fts::compound_not_str_list))]
    CompoundNotStrList,
    #[error("Missing first argument `language` to Stemmer")]
    #[diagnostic(code(fts::stemmer_missing_lang))]
    StemmerMissingLang,
    #[error("First argument `language` to Stemmer must be a string")]
    #[diagnostic(code(fts::stemmer_lang_not_str))]
    StemmerLangNotStr,
    #[error("Unsupported language: {0}")]
    #[diagnostic(code(fts::stemmer_unsupported_lang))]
    StemmerUnsupportedLang(SmartString<LazyCompact>),
    #[error("Filter Stopwords requires language name or a list of stopwords")]
    #[diagnostic(code(fts::stopwords_bad_arg))]
    StopwordsBadArg,
    #[error("First argument `stopwords` must be a list of strings")]
    #[diagnostic(code(fts::stopwords_not_str_list))]
    StopwordsNotStrList,
    #[error("Unknown token filter: {0}")]
    #[diagnostic(code(fts::unknown_token_filter))]
    UnknownTokenFilter(SmartString<LazyCompact>),
}

impl TokenizerConfig {
    /// Typed name-proof door: unknown stage names are unrepresentable as a
    /// stored config. Argument legality remains at [`validate`] / [`build`].
    pub fn admit(
        name: impl Into<SmartString<LazyCompact>>,
        args: Vec<DataValue>,
    ) -> std::result::Result<TokenizerConfig, UnknownTokenizerStageName> {
        let name = name.into();
        if !is_known_stage_name(&name) {
            return Err(UnknownTokenizerStageName(name));
        }
        Ok(TokenizerConfig { name, args })
    }

    /// Default stage used by staged FTS/LSH builders before an override —
    /// `Simple` is always an admitted name; constructed directly so the
    /// known-stage proof is unrepresentable as failure.
    pub fn simple() -> TokenizerConfig {
        TokenizerConfig {
            name: SmartString::from("Simple"),
            args: vec![],
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn args(&self) -> &[DataValue] {
        &self.args
    }

    /// The cache key for one full analyzer pipeline (this tokenizer plus
    /// `filters`): sha256 over each stage's name and memcmp-encoded args.
    ///
    /// Stability matters: equal configs must hash equal across processes and
    /// releases, because the hash deduplicates live analyzers and may key
    /// persistent state in later tiers. Pinned by `config_hash_is_stable`
    /// below. (The value differs from CozoDB's for configs with `Num`/`Str`
    /// args: KyzoDB's memcmp encoding renumbered those type tags. Argless
    /// and all-`Bool`-arg configs hash identically across the forks — the
    /// `Bool` tags are unchanged. CozoDB never persisted this hash, so
    /// nothing written by the base depends on it.)
    pub(crate) fn config_hash(&self, filters: &[Self]) -> impl AsRef<[u8]> {
        let mut hasher = Sha256::new();
        hasher.update(self.name.as_bytes());
        let mut args_vec = vec![];
        for arg in &self.args {
            kyzo_model::value::append_canonical(&mut args_vec, arg);
        }
        hasher.update(&args_vec);
        for filter in filters {
            hasher.update(filter.name.as_bytes());
            args_vec.clear();
            for arg in &filter.args {
                kyzo_model::value::append_canonical(&mut args_vec, arg);
            }
            hasher.update(&args_vec);
        }
        hasher.finalize_fixed()
    }

    /// Prove at definition time that this config (as a tokenizer, with
    /// `filters` as its filter chain) is constructible, without keeping the
    /// analyzer. The operator tier calls this from `::fts create` /
    /// `::lsh create` so an unknown name or malformed argument is refused
    /// *before* the manifest is written. `build` stays fallible regardless —
    /// see the module docs.
    pub(crate) fn validate(&self, filters: &[Self]) -> Result<()> {
        self.build(filters).map(|_| ())
    }

    pub(crate) fn build(&self, filters: &[Self]) -> Result<TextAnalyzer> {
        let tokenizer = self.construct_tokenizer()?;
        let token_filters = filters
            .iter()
            .map(|filter| filter.construct_token_filter())
            .collect::<Result<Vec<_>>>()?;
        Ok(TextAnalyzer {
            tokenizer,
            token_filters,
        })
    }
    pub(crate) fn construct_tokenizer(&self) -> Result<Box<dyn Tokenizer>> {
        Ok(match &self.name as &str {
            "Raw" => Box::new(RawTokenizer),
            "Simple" => Box::new(SimpleTokenizer),
            "Whitespace" => Box::new(WhitespaceTokenizer),
            "NGram" => {
                let min_gram = self
                    .args
                    .first()
                    .unwrap_or(&DataValue::from(1))
                    .get_int()
                    .ok_or(TokenizerBuildRefusal::NgramMinNotInt)?;
                let max_gram = self
                    .args
                    .get(1)
                    .unwrap_or(&DataValue::from(min_gram))
                    .get_int()
                    .ok_or(TokenizerBuildRefusal::NgramMaxNotInt)?;
                let prefix_only = self
                    .args
                    .get(2)
                    .unwrap_or(&DataValue::Bool(false))
                    .get_bool()
                    .ok_or(TokenizerBuildRefusal::NgramPrefixNotBool)?;
                ensure!(min_gram >= 1, TokenizerBuildRefusal::NgramMinTooSmall);
                ensure!(max_gram >= min_gram, TokenizerBuildRefusal::NgramMaxBelowMin);
                Box::new(NgramTokenizer::new(
                    min_gram as usize,
                    max_gram as usize,
                    prefix_only,
                ))
            }
            "Cangjie" => {
                let hmm = match self.args.get(1) {
                    None => false,
                    Some(d) => d
                        .get_bool()
                        .ok_or(TokenizerBuildRefusal::CangjieHmmNotBool)?,
                };
                let option = match self.args.first() {
                    None => cangjie::options::TokenizerOption::Default { hmm },
                    Some(d) => {
                        let s = d
                            .get_str()
                            .ok_or(TokenizerBuildRefusal::CangjieKindNotStr)?;
                        match s {
                            "default" => cangjie::options::TokenizerOption::Default { hmm },
                            "all" => cangjie::options::TokenizerOption::All,
                            "search" => cangjie::options::TokenizerOption::ForSearch { hmm },
                            "unicode" => cangjie::options::TokenizerOption::Unicode,
                            _ => {
                                return Err(TokenizerBuildRefusal::CangjieUnknownKind(s.into()).into());
                            }
                        }
                    }
                };
                Box::new(CangJieTokenizer {
                    worker: std::sync::Arc::new(Jieba::new()),
                    option,
                })
            }
            _ => {
                return Err(TokenizerBuildRefusal::UnknownTokenizer(self.name.clone()).into());
            }
        })
    }
    pub(crate) fn construct_token_filter(&self) -> Result<BoxTokenFilter> {
        Ok(match &self.name as &str {
            "AlphaNumOnly" => AlphaNumOnlyFilter.into(),
            "AsciiFolding" => AsciiFoldingFilter.into(),
            "LowerCase" | "Lowercase" => LowerCaser.into(),
            "RemoveLong" => {
                let limit = self
                    .args
                    .first()
                    .ok_or(TokenizerBuildRefusal::RemoveLongMissingArg)?
                    .get_int()
                    .ok_or(TokenizerBuildRefusal::RemoveLongNotInt)?;
                ensure!(limit > 0, NonPositiveRemoveLong(limit));
                RemoveLongFilter::limit(limit as usize).into()
            }
            "SplitCompoundWords" => {
                let mut list_values = Vec::new();
                match self
                    .args
                    .first()
                    .ok_or(TokenizerBuildRefusal::CompoundMissingArg)?
                {
                    DataValue::List(l) => {
                        for v in l {
                            list_values.push(
                                v.get_str()
                                    .ok_or(TokenizerBuildRefusal::CompoundNotStrList)?,
                            );
                        }
                    }
                    data_value_any!() => {
                        return Err(TokenizerBuildRefusal::CompoundNotStrList.into());
                    }
                }
                SplitCompoundWords::from_dictionary(list_values)?.into()
            }
            "Stemmer" => {
                let language = match self
                    .args
                    .first()
                    .ok_or(TokenizerBuildRefusal::StemmerMissingLang)?
                    .get_str()
                    .ok_or(TokenizerBuildRefusal::StemmerLangNotStr)?
                    .to_lowercase()
                    .as_str()
                {
                    "arabic" => Language::Arabic,
                    "danish" => Language::Danish,
                    "dutch" => Language::Dutch,
                    "english" => Language::English,
                    "finnish" => Language::Finnish,
                    "french" => Language::French,
                    "german" => Language::German,
                    "greek" => Language::Greek,
                    "hungarian" => Language::Hungarian,
                    "italian" => Language::Italian,
                    "norwegian" => Language::Norwegian,
                    "portuguese" => Language::Portuguese,
                    "romanian" => Language::Romanian,
                    "russian" => Language::Russian,
                    "spanish" => Language::Spanish,
                    "swedish" => Language::Swedish,
                    "tamil" => Language::Tamil,
                    "turkish" => Language::Turkish,
                    lang => {
                        return Err(
                            TokenizerBuildRefusal::StemmerUnsupportedLang(lang.into()).into(),
                        );
                    }
                };
                Stemmer::new(language).into()
            }
            "Stopwords" => {
                match self
                    .args
                    .first()
                    .ok_or(TokenizerBuildRefusal::StopwordsBadArg)?
                {
                    DataValue::Str(name) => StopWordFilter::for_lang(name)?.into(),
                    DataValue::List(l) => {
                        let mut stopwords = Vec::new();
                        for v in l {
                            stopwords.push(
                                v.get_str()
                                    .ok_or(TokenizerBuildRefusal::StopwordsNotStrList)?
                                    .to_string(),
                            );
                        }
                        StopWordFilter::new(stopwords).into()
                    }
                    data_value_any!() => {
                        return Err(TokenizerBuildRefusal::StopwordsBadArg.into());
                    }
                }
            }
            _ => {
                return Err(
                    TokenizerBuildRefusal::UnknownTokenFilter(self.name.clone()).into(),
                );
            }
        })
    }
}

impl<'de> Deserialize<'de> for TokenizerConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};
        use std::fmt;
        struct RawVisitor;
        impl<'de> Visitor<'de> for RawVisitor {
            type Value = (SmartString<LazyCompact>, Vec<DataValue>);
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("TokenizerConfig { name, args }")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut name = None;
                let mut args = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => name = Some(map.next_value()?),
                        "args" => args = Some(map.next_value()?),
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok((
                    name.ok_or_else(|| A::Error::missing_field("name"))?,
                    args.ok_or_else(|| A::Error::missing_field("args"))?,
                ))
            }
        }
        let (name, args) =
            deserializer.deserialize_struct("TokenizerConfig", &["name", "args"], RawVisitor)?;
        TokenizerConfig::admit(name, args).map_err(serde::de::Error::custom)
    }
}

/// The per-database analyzer cache: index name → analyzer, and config hash →
/// analyzer, so N indices sharing one pipeline share one live instance.
#[derive(Default)]
pub(crate) struct TokenizerCache {
    pub(crate) named_cache: RwLock<HashMap<SmartString<LazyCompact>, Arc<TextAnalyzer>>>,
    pub(crate) hashed_cache: RwLock<HashMap<Vec<u8>, Arc<TextAnalyzer>>>,
}

// KYZO DEVIATION from the CozoDB original: lock acquisition recovers from
// poisoning instead of `unwrap()`ing. The cache holds no invariant a
// panicked writer could have half-applied (entries are inserted whole), so
// continuing with the underlying data is sound, and a panicking thread
// elsewhere can no longer cascade into every later FTS query.
fn read_lock<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}
fn write_lock<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl TokenizerCache {
    pub(crate) fn get(
        &self,
        tokenizer_name: &str,
        tokenizer: &TokenizerConfig,
        filters: &[TokenizerConfig],
    ) -> Result<Arc<TextAnalyzer>> {
        {
            let idx_cache = read_lock(&self.named_cache);
            if let Some(analyzer) = idx_cache.get(tokenizer_name) {
                return Ok(analyzer.clone());
            }
        }
        let hash = tokenizer.config_hash(filters);
        {
            let hashed_cache = read_lock(&self.hashed_cache);
            if let Some(analyzer) = hashed_cache.get(hash.as_ref()) {
                let mut idx_cache = write_lock(&self.named_cache);
                idx_cache.insert(tokenizer_name.into(), analyzer.clone());
                return Ok(analyzer.clone());
            }
        }
        {
            let analyzer = Arc::new(tokenizer.build(filters)?);
            let mut hashed_cache = write_lock(&self.hashed_cache);
            hashed_cache.insert(hash.as_ref().to_vec(), analyzer.clone());
            let mut idx_cache = write_lock(&self.named_cache);
            idx_cache.insert(tokenizer_name.into(), analyzer.clone());
            Ok(analyzer)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str, args: Vec<DataValue>) -> TokenizerConfig {
        TokenizerConfig::admit(name, args).expect("test stage name must be admitted")
    }

    fn hex(h: impl AsRef<[u8]>) -> String {
        h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The config hash is a stability contract (it keys the analyzer cache
    /// and may key persistent state in later tiers). These vectors pin it.
    ///
    /// The zero-argument vector is independently checkable:
    /// `printf 'Simple' | sha256sum` (the hash of a config with no args and
    /// no filters is sha256(name) — an empty args list encodes to nothing).
    /// The arg-bearing vector additionally pins the memcmp encoding of
    /// `Int`/`Bool`/`Str` args as KyzoDB writes them; it was captured from
    /// this build and must never change silently — a failure here means the
    /// memcmp value encoding drifted, which is a data-format event, not a
    /// test to update.
    #[test]
    fn config_hash_is_stable() {
        let simple = cfg("Simple", vec![]);
        assert_eq!(
            hex(simple.config_hash(&[])),
            // = sha256("Simple"), checkable with `printf 'Simple' | sha256sum`:
            "3fee95da5ab69ebfdc16ec892b754105a11c20203054a6c6bcc0a60176891043"
        );

        let ngram = cfg(
            "NGram",
            vec![
                DataValue::from(1),
                DataValue::from(3),
                DataValue::Bool(false),
            ],
        );
        let filters = vec![
            cfg("Lowercase", vec![]),
            cfg("Stemmer", vec![DataValue::from("english")]),
        ];
        // INDEPENDENT DERIVATION. config_hash = sha256( name ++
        // canonical(args) ++ for each filter: name ++ canonical(args) ).
        // Reconstruct the hashed input entirely from the format law using
        // HAND-DERIVED canonical bytes (value tag + the key pinned by
        // `data::value::number::format_v1_golden_vectors`), then hash with a
        // stock Sha256 -- never the production encoder. If either drifts,
        // this fails independently of the production path.
        //   Int(1)      = 10 03 04 39 80 00..(9)
        //   Int(3)      = 10 03 04 3a c0 00..(9)
        //   Bool(false) = 08 00                (Tag::Bool, 0x00)
        //   Str("english") = 18 65 6e 67 6c 69 73 68 00 00  (Tag::Str, bytes, 00 00 term)
        let mut expected_input: Vec<u8> = Vec::new();
        expected_input.extend_from_slice(b"NGram");
        expected_input
            .extend_from_slice(&[0x10, 0x03, 0x04, 0x39, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected_input
            .extend_from_slice(&[0x10, 0x03, 0x04, 0x3a, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        expected_input.extend_from_slice(&[0x08, 0x00]);
        expected_input.extend_from_slice(b"Lowercase");
        expected_input.extend_from_slice(b"Stemmer");
        expected_input
            .extend_from_slice(&[0x18, 0x65, 0x6e, 0x67, 0x6c, 0x69, 0x73, 0x68, 0x00, 0x00]);
        let expected = {
            use sha2::{Digest, Sha256};
            hex(Sha256::digest(&expected_input))
        };
        assert_eq!(
            hex(ngram.config_hash(&filters)),
            expected,
            "config hash diverged from the hand-derived canonical input"
        );
    }

    /// Same config ⇒ same hash ⇒ same live analyzer instance; a second name
    /// for the same pipeline resolves through the hashed cache to the same
    /// `Arc`.
    #[test]
    fn cache_is_deterministic() {
        let cache = TokenizerCache::default();
        let tk = cfg("Simple", vec![]);
        let filters = vec![cfg("Lowercase", vec![])];

        let h1 = hex(tk.config_hash(&filters));
        let h2 = hex(tk.clone().config_hash(&filters.clone()));
        assert_eq!(h1, h2);

        let a1 = cache.get("idx1", &tk, &filters).unwrap();
        let a2 = cache.get("idx1", &tk, &filters).unwrap();
        assert!(
            Arc::ptr_eq(&a1, &a2),
            "named cache must return the same instance"
        );

        let a3 = cache.get("idx2_same_pipeline", &tk, &filters).unwrap();
        assert!(
            Arc::ptr_eq(&a1, &a3),
            "same config under a different name must share the analyzer via the hashed cache"
        );

        // Different config: different hash, different instance.
        let other = cfg("Whitespace", vec![]);
        assert_ne!(h1, hex(other.config_hash(&filters)));
        let a4 = cache.get("idx3", &other, &filters).unwrap();
        assert!(!Arc::ptr_eq(&a1, &a4));
    }

    /// A non-positive `RemoveLong` length used to wrap through `as usize`
    /// into a filter that never removes anything (silently); it is now a
    /// typed refusal on both moments of truth — definition time
    /// (`validate`) and use time (`build`/`construct_token_filter`).
    #[test]
    fn remove_long_rejects_non_positive_lengths() {
        for bad in [-1i64, 0, i64::MIN] {
            let filter = cfg("RemoveLong", vec![DataValue::from(bad)]);
            let err = match filter.construct_token_filter() {
                Err(e) => e,
                Ok(_) => panic!("RemoveLong({bad}) must not construct"),
            };
            assert!(
                err.downcast_ref::<NonPositiveRemoveLong>().is_some(),
                "RemoveLong({bad}) at use time: expected the typed refusal, got: {err:?}"
            );
            let err = cfg("Simple", vec![]).validate(&[filter]).unwrap_err();
            assert!(
                err.downcast_ref::<NonPositiveRemoveLong>().is_some(),
                "RemoveLong({bad}) at definition time: expected the typed refusal, got: {err:?}"
            );
        }
        // The smallest lawful length still constructs.
        assert!(
            cfg("RemoveLong", vec![DataValue::from(1)])
                .construct_token_filter()
                .is_ok()
        );
        // The tokenizer with integer args refuses non-positives too (NGram
        // guards its ranges before any cast).
        assert!(
            cfg("NGram", vec![DataValue::from(-1)])
                .construct_tokenizer()
                .is_err()
        );
        assert!(
            cfg("NGram", vec![DataValue::from(1), DataValue::from(-3)])
                .construct_tokenizer()
                .is_err()
        );
    }

    /// Unknown names are refused at the admit door — unstorable as config.
    #[test]
    fn unknown_tokenizer_refused_at_admit() {
        assert!(TokenizerConfig::admit("NoSuchTokenizer", vec![]).is_err());
        assert!(TokenizerConfig::admit("NoSuchFilter", vec![]).is_err());
    }

    /// `validate` is the definition-time proof the operator tier calls
    /// before writing a manifest (known name, argument legality).
    #[test]
    fn validate_proves_config_at_definition_time() {
        cfg("Simple", vec![])
            .validate(&[
                cfg("Lowercase", vec![]),
                cfg("Stemmer", vec![DataValue::from("english")]),
                cfg("AsciiFolding", vec![]),
                cfg("AlphaNumOnly", vec![]),
                cfg("RemoveLong", vec![DataValue::from(40)]),
                cfg("Stopwords", vec![DataValue::from("en")]),
            ])
            .unwrap();

        // Unknown tokenizer name — refuse at admit, not at validate.
        assert!(TokenizerConfig::admit("NoSuchTokenizer", vec![]).is_err());
        // Known name, unlawful args.
        assert!(
            cfg("NGram", vec![DataValue::from(0)])
                .validate(&[])
                .is_err()
        );
        assert!(
            cfg("NGram", vec![DataValue::from(3), DataValue::from(2)])
                .validate(&[])
                .is_err()
        );
        assert!(
            cfg("Simple", vec![])
                .validate(&[cfg("Stemmer", vec![DataValue::from("klingon")])])
                .is_err()
        );
        assert!(
            cfg("Simple", vec![])
                .validate(&[cfg("Stopwords", vec![DataValue::from("zz")])])
                .is_err()
        );
        assert!(
            cfg("Cangjie", vec![DataValue::from("bogus-kind")])
                .validate(&[])
                .is_err()
        );
    }
}
