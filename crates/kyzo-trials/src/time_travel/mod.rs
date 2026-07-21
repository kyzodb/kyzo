/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Temporal trial batteries — split by kind of proof, not by size.
//!
//! - [`script`]: language-surface as-of via `Db::run_script` + real `@` clauses
//! - [`path`]: Capabilities 3–4 from `query/trials` — temporal generator twin
//!   (`program_grid`, `shuffle_body`) + refusal-lift coverage; also the
//!   prior Fjall full-path seat (deferred — Cap3–4 own the cut destiny)

#[cfg(test)]
mod path;
#[cfg(test)]
mod script;
