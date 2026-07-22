/*
 * Adapted from the Cang-jie project, MIT-licensed:
 * https://github.com/DCjanus/cang-jie
 * Vendored by way of CozoDB (Copyright 2023, The Cozo Project Authors).
 * This file remains under the MIT license; it ships inside KyzoDB, whose
 * own code is under the Mozilla Public License, v. 2.0
 * (https://mozilla.org/MPL/2.0/). The Cang-jie original carries no per-file
 * header; nothing was removed. Deviations are marked `KYZO DEVIATION`.
 */

use super::{options::TokenizerOption, stream::CangjieTokenStream};
use crate::project::text::tokenizer::BoxTokenStream;
use jieba_rs::Jieba;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub(crate) struct CangJieTokenizer {
    /// Separation algorithm provider
    pub(crate) worker: Arc<Jieba>,
    /// Separation config
    pub(crate) option: TokenizerOption,
}

impl CangJieTokenizer {
    /// Empty Jieba worker with the default cut option (no HMM).
    pub(crate) fn empty() -> Self {
        CangJieTokenizer {
            worker: Arc::new(Jieba::empty()),
            option: TokenizerOption::Default { hmm: false },
        }
    }
}

impl crate::project::text::tokenizer::Tokenizer for CangJieTokenizer {
    /// Cut text into tokens
    fn token_stream<'a>(&self, text: &'a str) -> BoxTokenStream<'a> {
        let result = match self.option {
            TokenizerOption::All => self.worker.cut_all(text),
            TokenizerOption::Default { hmm: use_hmm } => self.worker.cut(text, use_hmm),
            TokenizerOption::ForSearch { hmm: use_hmm } => {
                self.worker.cut_for_search(text, use_hmm)
            }
            TokenizerOption::Unicode => {
                text.chars()
                    .fold((0usize, vec![]), |(offset, mut result), the_char| {
                        result.push(&text[offset..offset + the_char.len_utf8()]);
                        (offset + the_char.len_utf8(), result)
                    })
                    .1
            }
        };
        // KYZO DEVIATION: the vendored source `log::trace!`d every token of
        // the input here; kyzo-core carries no `log` dependency and does not
        // echo user text to logs.
        // KYZO DEVIATION: pass `text` so the stream derives byte offsets from
        // slice addresses (overlapping All/ForSearch cuts). See stream.rs.
        BoxTokenStream::from(CangjieTokenStream::new(text, result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::text::tokenizer::Tokenizer;
    use jieba_rs::Jieba;
    use std::sync::Arc;

    fn collect(option: TokenizerOption, text: &str) -> Vec<(String, usize, usize)> {
        let tok = CangJieTokenizer {
            worker: Arc::new(Jieba::new()),
            option,
        };
        let mut stream = tok.token_stream(text);
        let mut out = Vec::new();
        while let Some(t) = stream.next() {
            out.push((t.text.clone(), t.offset_from(), t.offset_to()));
        }
        out
    }

    /// Offset law: every token must reconstruct as `text[from..to]`.
    fn assert_offset_round_trip(option: TokenizerOption, text: &str) {
        for (word, from, to) in collect(option, text) {
            assert!(
                from <= to && to <= text.len(),
                "offsets out of range: {from}..{to} in len {}",
                text.len()
            );
            assert!(
                text.is_char_boundary(from) && text.is_char_boundary(to),
                "offsets not on UTF-8 boundaries: {from}..{to}"
            );
            assert_eq!(
                &text[from..to],
                word.as_str(),
                "offset round-trip failed for {word:?} at {from}..{to}"
            );
        }
    }

    #[test]
    fn offset_round_trip_all_modes_including_overlapping_cuts() {
        let samples = ["南京长江大桥", "中文测试abc漢字", "hello世界", "a", ""];
        let modes = [
            TokenizerOption::Default { hmm: false },
            TokenizerOption::Default { hmm: true },
            TokenizerOption::ForSearch { hmm: false },
            TokenizerOption::ForSearch { hmm: true },
            TokenizerOption::All,
            TokenizerOption::Unicode,
        ];
        for text in samples {
            for mode in &modes {
                assert_offset_round_trip(mode.clone(), text);
            }
        }
    }

    /// CJK oracle: Default (no HMM) split must equal jieba.cut, content-pinned.
    /// Uses the textbook phrase (dictionary-backed multi-token cut). The bridge
    /// phrase is a single dict entry under current jieba-rs — pinned via
    /// ForSearch / All below, not Default.
    #[test]
    fn cjk_oracle_default_split_matches_jieba() {
        let text = "我来到北京清华大学";
        let jieba = Jieba::new();
        let expect: Vec<&str> = jieba.cut(text, false);
        let got: Vec<String> = collect(TokenizerOption::Default { hmm: false }, text)
            .into_iter()
            .map(|(w, _, _)| w)
            .collect();
        assert_eq!(got, expect, "Cangjie Default must equal jieba.cut");
        assert_eq!(
            got.as_slice(),
            &["我", "来到", "北京", "清华大学"],
            "textbook CJK cut content"
        );
        for (word, from, to) in collect(TokenizerOption::Default { hmm: false }, text) {
            assert_eq!(&text[from..to], word.as_str());
        }
    }

    /// ForSearch on the bridge phrase: multi-token cut including 南京/长江/大桥.
    #[test]
    fn cjk_oracle_for_search_bridge_phrase() {
        let text = "南京长江大桥";
        let got: Vec<String> = collect(TokenizerOption::ForSearch { hmm: false }, text)
            .into_iter()
            .map(|(w, _, _)| w)
            .collect();
        assert!(got.contains(&"南京".into()), "got {got:?}");
        assert!(got.contains(&"长江".into()), "got {got:?}");
        assert!(got.contains(&"大桥".into()), "got {got:?}");
        assert_offset_round_trip(TokenizerOption::ForSearch { hmm: false }, text);
    }

    /// All-mode overlapping cuts: more tokens than Unicode chars, every
    /// offset still round-trips (the bug the sequential accumulator lied about).
    #[test]
    fn all_mode_overlapping_offsets_round_trip() {
        let text = "南京长江大桥";
        let tokens = collect(TokenizerOption::All, text);
        assert!(
            tokens.len() > text.chars().count(),
            "All must emit overlapping cuts, got {} tokens for {} chars",
            tokens.len(),
            text.chars().count()
        );
        assert_offset_round_trip(TokenizerOption::All, text);
    }
}
