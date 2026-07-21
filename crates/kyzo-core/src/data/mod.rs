/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/// Typed digest / region identities for the private record model (#268 purity).
pub(crate) mod digest;
pub(crate) mod json;
/// Statement-body types + ONTOK constructions for the one private KyzoRecord.
///
/// `construct::{event,state,role,concept,rule,derivation,context_record}` are
/// wired through [`crate::session::admit::admit_construct`] — no blanket allow.
pub(crate) mod statement;
