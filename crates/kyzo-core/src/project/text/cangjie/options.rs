/*
 * Adapted from the Cang-jie project, MIT-licensed:
 * https://github.com/DCjanus/cang-jie
 * Vendored by way of CozoDB (Copyright 2023, The Cozo Project Authors).
 * This file remains under the MIT license; it ships inside KyzoDB, whose
 * own code is under the Mozilla Public License, v. 2.0
 * (https://mozilla.org/MPL/2.0/). The Cang-jie original carries no per-file
 * header; nothing was removed. Deviations are marked `KYZO DEVIATION`.
 */

/// Tokenizer Option
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenizerOption {
    /// Cut the input text, return all possible words
    All,
    /// Cut the input text
    Default {
        /// `hmm`: enable HMM or not
        hmm: bool,
    },

    /// Cut the input text in search mode
    ForSearch {
        /// `hmm`: enable HMM or not
        hmm: bool,
    },
    /// Cut the input text into UTF-8 characters
    Unicode,
}
