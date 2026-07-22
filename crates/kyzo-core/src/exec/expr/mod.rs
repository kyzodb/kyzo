/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Expression evaluation doors (row + columnar).
pub(crate) mod batch;
pub(crate) mod eval;

pub(crate) use eval::{eval_expr, eval_pred, resolve_write_validity};
