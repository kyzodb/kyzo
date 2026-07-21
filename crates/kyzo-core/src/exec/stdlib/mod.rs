/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Builtin stdlib: BoundOp registry + kernels. No catch-all functions module.
pub mod bind;
pub mod bound_op;
pub mod collection;
pub mod compare;
pub mod convert;
pub mod errors;
pub mod geo;
pub mod interval;
pub mod metric;
pub mod nondet;
pub mod numeric;
pub mod temporal_format;
pub mod text;

#[cfg(test)]
mod tests;

#[allow(unused_imports)] // reexport surface; callers bind later or via tests
pub use bind::{bind_op, resolve_op};
#[allow(unused_imports)] // reexport surface; callers bind later or via tests
pub use bound_op::BoundOp;
#[allow(unused_imports)] // reexport surface; callers bind later or via tests
pub use errors::StdlibRefuse;
