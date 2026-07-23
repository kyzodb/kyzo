/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #77's Arrow interop cross-check — see Cargo.toml's description.
//! No production code lives here; every consumer is a test proving a real
//! `arrow` reader parses kyzo-core's own encoder output byte-identically.

#![forbid(unsafe_code)]
