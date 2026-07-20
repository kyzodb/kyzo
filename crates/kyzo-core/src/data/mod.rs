/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

pub(crate) mod json;
/// Typed digest / region identities for the private record model (#268 purity).
#[allow(dead_code)] // mid-wiring seat; callers land with admit surfaces
pub(crate) mod digest;
/// Statement-body types + ONTOK constructions for the one private KyzoRecord.
#[allow(dead_code)] // mid-wiring seat (#268 T1); callers land in later T#s
pub(crate) mod statement;
