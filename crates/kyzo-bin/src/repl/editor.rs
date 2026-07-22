/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0, `cozo-bin/src/repl.rs`'s `Indented` type). This file is based
 * on code contributed by https://github.com/rhn to the CozoDB original;
 * that authorship is preserved here. Split out of the former flat
 * `repl.rs` into its own module: line-editing/continuation behavior is a
 * distinct concern from the read-eval loop (`super::repl_main`) and from
 * command dispatch (`super::commands`).
 *
 * - **`Completer::update` no longer panics.** The original asserted that
 *   this path could never run: rustyline only calls `update` to splice a
 *   candidate `complete` returned, and `complete` is never overridden here
 *   (its default impl always returns no candidates), so `update` is
 *   unreachable *today*. But that invariant lives in a different method
 *   than the one enforcing it — a future change adding real completions
 *   here would have to remember to touch this one too, silently, or
 *   reintroduce a panic reachable from a keypress. A no-op fallback
 *   (leave the line buffer untouched) costs nothing now and can't regress
 *   into a crash later.
 */

//! The REPL's line-editing behavior: a query continues onto more lines
//! while it starts with a space and hasn't yet ended in a blank line. No
//! hinting, no syntax highlighting, no tab completion — `rustyline` needs a
//! `Helper` naming all four regardless, so this type is all four, three of
//! them trivially.

use rustyline::Changeset;

pub(super) struct Indented;

impl rustyline::hint::Hinter for Indented {
    type Hint = String;
}

impl rustyline::highlight::Highlighter for Indented {}
impl rustyline::completion::Completer for Indented {
    type Candidate = String;

    fn update(
        &self,
        _line: &mut rustyline::line_buffer::LineBuffer,
        _start: usize,
        _elected: &str,
        _cl: &mut Changeset,
    ) {
        // No completions are ever offered (see the port note above), so
        // there is never a candidate to splice in; doing nothing is the
        // correct behavior if this is ever reached, not just the safe one.
    }
}

impl rustyline::Helper for Indented {}

impl rustyline::validate::Validator for Indented {
    fn validate(
        &self,
        ctx: &mut rustyline::validate::ValidationContext<'_>,
    ) -> rustyline::Result<rustyline::validate::ValidationResult> {
        Ok(if ctx.input().starts_with(' ') {
            if ctx.input().ends_with('\n') {
                rustyline::validate::ValidationResult::Valid(None)
            } else {
                rustyline::validate::ValidationResult::Incomplete
            }
        } else {
            rustyline::validate::ValidationResult::Valid(None)
        })
    }
}
