/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The wide value faces: each kind's identity law, defined before its
//! bytes. The canonical payload encodings live in [`super::super::canonical`]
//! (one grammar, one authority); residency (inline vs arena) is the
//! cell's threshold law, not a per-kind decision.

pub mod collection;
pub mod interval;
pub mod json;
pub mod regex;
pub mod uuid;
pub mod validity;
pub mod vector;

pub use vector::{Vector, VectorComponent, VectorDimension};
