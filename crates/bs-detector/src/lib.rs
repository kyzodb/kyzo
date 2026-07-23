/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Library surface of the Bullshit Detector — the bite-proof suite imports
//! the engines through here; the binary (`main.rs`) is a thin door over
//! [`run`].

#![forbid(unsafe_code)]

pub mod boundary;
pub mod engines;
pub mod policy;
pub mod registry;
pub mod run;
pub mod waiver;
