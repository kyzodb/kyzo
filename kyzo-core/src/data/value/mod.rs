/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The value plane: a value is a 16-byte tagged cell, either fully inline
//! or a dense `Code` into a shared, order-preserving interning arena.

pub mod arena;
pub mod canonical;
pub mod cell;
pub mod code;
pub mod column;
pub mod number;
pub mod prefix;
pub mod row;
pub mod string;
pub mod tag;
pub mod wide;
