/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Wire envelopes: values crossing process / host boundaries.

pub mod arrow;
pub mod json;

pub use json::{
    JsonData, JsonValue, datavalue_to_json, json_from_serde, json_to_datavalue, serde_from_json,
};
