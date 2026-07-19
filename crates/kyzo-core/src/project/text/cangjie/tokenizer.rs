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

impl Default for CangJieTokenizer {
    fn default() -> Self {
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
        BoxTokenStream::from(CangjieTokenStream::new(result))
    }
}
