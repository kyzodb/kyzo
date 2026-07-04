/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A minimal façade over the crate-internal parse tier for `kyzo-lsp`
//! (story #92): validate a KyzoScript source string — parse it and, for a
//! query script, fully resolve params/aggregations/fixed rules — without
//! executing it or needing a live [`crate::Db`]. `Ok(())` means the script
//! would run; `Err` carries the exact designed diagnostic (span + the
//! `#[help]` text story #73 built) that `Db::run_script` would raise, so a
//! caller can render it live, on every keystroke, instead of only after a
//! real run against a real store.
//!
//! Same posture as [`crate::fuzz_api`] and [`crate::bench_api`] — a thin
//! boundary that hands back only what the caller needs, never the parsed
//! AST — but *not* feature-gated: live diagnostics are a first-class
//! product surface (the delivery of #73's redesign, not a fuzz/bench-only
//! concern), so this is always compiled.

use std::collections::BTreeMap;

use miette::Result;

use crate::current_validity;
use crate::data::value::DataValue;
use crate::fixed_rule::DEFAULT_FIXED_RULES;
use crate::parse::parse_script;

/// Validate one KyzoScript source string against the real parser and the
/// real default fixed-rule registry, discarding the parsed AST — the LSP's
/// diagnostics-on-type needs only "does this fail, and if so, with what
/// designed error", never the AST itself (which stays crate-internal).
///
/// `params` lets a caller that already knows the script's intended
/// parameter bindings (from an editor's "run with these params" panel, say)
/// validate `$name` references against them; an empty map still validates
/// everything else and reports every `$name` as unbound (a real, useful
/// diagnostic on its own, since the LSP's default view of a script has no
/// parameter values to offer).
pub fn check_script(src: &str, params: &BTreeMap<String, DataValue>) -> Result<()> {
    let cur_vld = current_validity()?;
    parse_script(src, params, &DEFAULT_FIXED_RULES, cur_vld)?;
    Ok(())
}
