/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![no_main]

//! Fuzzes the KyzoScript parser entry point used by `Db::run_script`
//! (`crates/kyzo-core/src/parse/mod.rs::parse_script`).
//!
//! `parse_script` lives in `pub(crate) mod parse`, so it isn't reachable
//! from an external crate. Rather than widen the parse module's own
//! visibility, `crates/kyzo-core/src/fuzz_api.rs` adds one `#[cfg(feature =
//! "fuzz-internals")]` function — `fuzz_parse_script` — mirroring the
//! existing `bench_api` façade pattern: it discards the parsed AST and
//! hands back only `Result<()>`, so no crate-internal type crosses the
//! boundary either.
//!
//! Invariant: parsing arbitrary bytes never panics and never hangs on a
//! small input. `Ok` and `Err` are both acceptable outcomes — the parser's
//! own law suite (`parse/fuzz_tests.rs`) already checks the stronger
//! spanned-error property with a grammar-aware generator; this target's
//! job is raw-byte coverage a structured generator won't reach on its own.

use kyzo::fuzz_api::fuzz_parse_script;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // `&str` is the real API surface (`Db::run_script(&self, payload: &str,
    // ...)`); lossy conversion matches how the parse-tier's own generative
    // fuzz harness treats byte-mutated (possibly invalid-UTF-8) input.
    let src = String::from_utf8_lossy(data);
    let _ = fuzz_parse_script(&src);
});
