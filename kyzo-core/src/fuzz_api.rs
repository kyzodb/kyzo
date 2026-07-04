/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! An opaque façade over the crate-internal parse tier, for the KyzoScript
//! fuzz target (`fuzz/fuzz_targets/kyzoscript_parser.rs`). Same posture as
//! [`crate::bench_api`]: gated behind its own feature so it never touches
//! the normal public surface, and hands the fuzzer only what it needs —
//! "does this text parse without panicking or hanging" — never the AST
//! type itself, which stays crate-internal.
//!
//! The memcmp-codec fuzz target needs no façade: `encode_tuple_key` /
//! `decode_tuple_from_key` ([`crate::data::tuple`], re-exported at the
//! crate root) are already public and exercise the exact codec under test.

use std::collections::BTreeMap;

use miette::Result;

use crate::current_validity;
use crate::fixed_rule::DEFAULT_FIXED_RULES;
use crate::parse::parse_script;

/// Parse a KyzoScript source string with empty params and the real default
/// fixed-rule registry, discarding the parsed AST. The fuzz target's only
/// invariant is "never panics, never hangs" on arbitrary bytes; `Ok`/`Err`
/// are both acceptable outcomes.
pub fn fuzz_parse_script(src: &str) -> Result<()> {
    let cur_vld = current_validity()?;
    parse_script(src, &BTreeMap::new(), &DEFAULT_FIXED_RULES, cur_vld)?;
    Ok(())
}
