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
//! A [`TokenizerConfig`] is pure data — a name plus [`DataValue`] arguments —
//! stored verbatim in an FTS index manifest. It becomes a runnable
//! [`TextAnalyzer`] only through [`TokenizerConfig::build`], which is where
//! unknown names and malformed arguments are refused.
//!
//! Two moments of truth, by design:
//!
//! - **Definition time**: the operator tier (`::fts create`) must call
//!   [`TokenizerConfig::validate`] so a bad config is refused before the
//!   manifest is ever written. (New over the CozoDB original, where a
//!   manifest with an unknown tokenizer name was storable and only failed
//!   at first use.)
//! - **Use time**: [`TokenizerConfig::build`] stays lazily fallible anyway —
//!   a manifest written by an older or foreign build is data, and data is
//!   never trusted to be well-formed just because it was once stored.

use crate::DataValue;
use crate::engines::text::cangjie::tokenizer::CangJieTokenizer;
use crate::engines::text::tokenizer::{
    AlphaNumOnlyFilter, AsciiFoldingFilter, BoxTokenFilter, Language, LowerCaser, NgramTokenizer,
    RawTokenizer, RemoveLongFilter, SimpleTokenizer, SplitCompoundWords, Stemmer, StopWordFilter,
    TextAnalyzer, Tokenizer, WhitespaceTokenizer,
};
use jieba_rs::Jieba;
use miette::{Diagnostic, Result, bail, ensure, miette};
use sha2::digest::FixedOutput;
use sha2::{Digest, Sha256};
use smartstring::{LazyCompact, SmartString};
use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use thiserror::Error;

pub(crate) mod ast;
pub(crate) mod cangjie;
// ─────────────────────────────────────────────────────────────────────────
// SEAM: operator tier (not yet ported).
//
// The CozoDB original declares `pub(crate) mod indexing;` here. That module
// (FTS index build + search) needs the catalog's `RelationHandle` and lands
// with the operator tier.
// ─────────────────────────────────────────────────────────────────────────
pub(crate) mod tokenizer;

/// The stored description of one FTS index: where it hangs, how documents
/// are extracted, and how text is tokenized. Persisted in the catalog; the
/// `tokenizer`/`filters` configs are re-`build()`-able at any later time.
#[derive(Debug, Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct FtsIndexManifest {
    pub(crate) base_relation: SmartString<LazyCompact>,
    pub(crate) index_name: SmartString<LazyCompact>,
    pub(crate) extractor: String,
    pub(crate) tokenizer: TokenizerConfig,
    pub(crate) filters: Vec<TokenizerConfig>,
}

/// A tokenizer or token-filter *as configuration*: a name and its arguments,
/// exactly as written in the index definition. Pure data — see the module
/// docs for when it is proven runnable.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct TokenizerConfig {
    pub name: SmartString<LazyCompact>,
    pub args: Vec<DataValue>,
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

impl TokenizerConfig {
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
            crate::data::value::append_canonical(&mut args_vec, arg);
        }
        hasher.update(&args_vec);
        for filter in filters {
            hasher.update(filter.name.as_bytes());
            args_vec.clear();
            for arg in &filter.args {
                crate::data::value::append_canonical(&mut args_vec, arg);
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
                    .ok_or_else(|| miette!("First argument `min_gram` must be an integer"))?;
                let max_gram = self
                    .args
                    .get(1)
                    .unwrap_or(&DataValue::from(min_gram))
                    .get_int()
                    .ok_or_else(|| miette!("Second argument `max_gram` must be an integer"))?;
                let prefix_only = self
                    .args
                    .get(2)
                    .unwrap_or(&DataValue::Bool(false))
                    .get_bool()
                    .ok_or_else(|| miette!("Third argument `prefix_only` must be a boolean"))?;
                ensure!(min_gram >= 1, "min_gram must be >= 1");
                ensure!(max_gram >= min_gram, "max_gram must be >= min_gram");
                Box::new(NgramTokenizer::new(
                    min_gram as usize,
                    max_gram as usize,
                    prefix_only,
                ))
            }
            "Cangjie" => {
                let hmm = match self.args.get(1) {
                    None => false,
                    Some(d) => d.get_bool().ok_or_else(|| {
                        miette!("Second argument `use_hmm` to Cangjie must be a boolean")
                    })?,
                };
                let option = match self.args.first() {
                    None => cangjie::options::TokenizerOption::Default { hmm },
                    Some(d) => {
                        let s = d.get_str().ok_or_else(|| {
                            miette!("First argument `kind` to Cangjie must be a string")
                        })?;
                        match s {
                            "default" => cangjie::options::TokenizerOption::Default { hmm },
                            "all" => cangjie::options::TokenizerOption::All,
                            "search" => cangjie::options::TokenizerOption::ForSearch { hmm },
                            "unicode" => cangjie::options::TokenizerOption::Unicode,
                            _ => bail!("Unknown Cangjie kind: {}", s),
                        }
                    }
                };
                Box::new(CangJieTokenizer {
                    worker: std::sync::Arc::new(Jieba::new()),
                    option,
                })
            }
            _ => bail!("Unknown tokenizer: {}", self.name),
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
                    .ok_or_else(|| miette!("Missing first argument `min_length`"))?
                    .get_int()
                    .ok_or_else(|| miette!("First argument `min_length` must be an integer"))?;
                ensure!(limit > 0, NonPositiveRemoveLong(limit));
                RemoveLongFilter::limit(limit as usize).into()
            }
            "SplitCompoundWords" => {
                let mut list_values = Vec::new();
                match self
                    .args
                    .first()
                    .ok_or_else(|| miette!("Missing first argument `compound_words_list`"))?
                {
                    DataValue::List(l) => {
                        for v in l {
                            list_values.push(v.get_str().ok_or_else(|| {
                                miette!(
                                    "First argument `compound_words_list` must be a list of strings"
                                )
                            })?);
                        }
                    }
                    _ => bail!("First argument `compound_words_list` must be a list of strings"),
                }
                SplitCompoundWords::from_dictionary(list_values)
                    .map_err(|e| miette!("Failed to load dictionary: {}", e))?
                    .into()
            }
            "Stemmer" => {
                let language = match self
                    .args
                    .first()
                    .ok_or_else(|| miette!("Missing first argument `language` to Stemmer"))?
                    .get_str()
                    .ok_or_else(|| {
                        miette!("First argument `language` to Stemmer must be a string")
                    })?
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
                    lang => bail!("Unsupported language: {}", lang),
                };
                Stemmer::new(language).into()
            }
            "Stopwords" => {
                match self.args.first().ok_or_else(|| {
                    miette!("Filter Stopwords requires language name or a list of stopwords")
                })? {
                    DataValue::Str(name) => StopWordFilter::for_lang(name)?.into(),
                    DataValue::List(l) => {
                        let mut stopwords = Vec::new();
                        for v in l {
                            stopwords.push(
                                v.get_str()
                                    .ok_or_else(|| {
                                        miette!(
                                            "First argument `stopwords` must be a list of strings"
                                        )
                                    })?
                                    .to_string(),
                            );
                        }
                        StopWordFilter::new(stopwords).into()
                    }
                    _ => bail!("Filter Stopwords requires language name or a list of stopwords"),
                }
            }
            _ => bail!("Unknown token filter: {:?}", self.name),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct FtsIndexConfig {
    base_relation: SmartString<LazyCompact>,
    index_name: SmartString<LazyCompact>,
    fts_fields: Vec<SmartString<LazyCompact>>,
    tokenizer: TokenizerConfig,
    filters: Vec<TokenizerConfig>,
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
        TokenizerConfig {
            name: name.into(),
            args,
        }
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
        assert_eq!(
            hex(ngram.config_hash(&filters)),
            "dbd9b3ac64b752ecd00db722871c9bef6452c87c2f7bf2d635480adeba50fdda"
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

    /// The lazy path stays lazy: an unknown name is representable as config
    /// (a stored manifest may carry it) and fails only when constructed...
    #[test]
    fn unknown_tokenizer_fails_at_construction() {
        let bad = cfg("NoSuchTokenizer", vec![]);
        assert!(bad.construct_tokenizer().is_err());
        assert!(bad.build(&[]).is_err());
        let bad_filter = cfg("NoSuchFilter", vec![]);
        assert!(bad_filter.construct_token_filter().is_err());
    }

    /// ...and `validate` is the definition-time proof the operator tier
    /// calls before writing a manifest.
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

        // Unknown tokenizer name.
        assert!(cfg("NoSuchTokenizer", vec![]).validate(&[]).is_err());
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
