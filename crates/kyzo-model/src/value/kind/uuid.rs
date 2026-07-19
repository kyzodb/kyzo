/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Uuid`: sixteen raw bytes, fixed width; identity and storage order
//! are the bytes themselves. The simplest face — no normalization, no
//! variant/version interpretation (a v4 and a v7 UUID with equal bytes
//! would be the same value, and unequal bytes are different values,
//! full stop).
