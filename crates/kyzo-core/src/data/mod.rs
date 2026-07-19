/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Modules are landed in dependency order, sorted bottom-up: the dead-code
// expectations fire (forcing their removal) as consumers land. Once a
// module's first engine consumer is in place, its dead-code expectation
// becomes triggered and the expectation must be removed or changed to allow.
pub(crate) mod aggr;
pub(crate) mod arrow_ipc;
pub(crate) mod bitemporal;
// expr: bottom-up landing per the module-order note above; dead in the
// lib build until its first engine consumer lands.
#[expect(dead_code)]
pub(crate) mod expr;
pub(crate) mod functions;
pub(crate) mod json;
// program: bottom-up landing per the module-order note above; dead in
// the lib build until its first engine consumer lands.
#[allow(dead_code)]
pub(crate) mod program;
pub(crate) mod relation;
pub(crate) mod sketch;

#[cfg(test)]
mod tests;
