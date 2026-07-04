/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Port in flight (#3): modules land bottom-up in dependency order; the
// dead-code expectations fire — forcing their removal — as consumers land.
// aggr's expectation fired when query/eval.rs landed as its first engine
// consumer.
pub(crate) mod aggr;
pub(crate) mod batch;
pub(crate) mod bitemporal;
#[expect(dead_code)]
pub(crate) mod expr;
pub(crate) mod fact_payload;
pub(crate) mod functions;
pub(crate) mod memcmp;
#[allow(dead_code)]
pub(crate) mod program;
pub(crate) mod relation;
pub(crate) mod sketch;
pub(crate) mod span;
pub(crate) mod symb;
pub(crate) mod tuple;
pub(crate) mod value;

#[cfg(test)]
mod tests;
