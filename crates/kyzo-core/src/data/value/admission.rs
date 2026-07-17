/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Admission and Denial — one discipline, opposite directions.
//!
//! A **token** proves why an operation was allowed; a **witness** proves
//! why a row (or mint, or spend) was refused. Never a bare boolean.
//!
//! | Direction | Vocabulary | What it is |
//! | --- | --- | --- |
//! | Allowed | [`Admission`] | Durable `(arena, epoch)` context fact (`Copy`) |
//! | Allowed | [`NestedDomainCtx`] | Same fact under an invariant-lifetime nest brand |
//! | Allowed | [`BulkSpendAuthority`] | Consumable permission after domain admission |
//! | Allowed | [`BulkPass`] | Amortized capability after spending the authority |
//! | Refused | [`Denial`] | Typed witness for arena/epoch/cut/arity/extent refusal |
//!
//! Both directions are re-exported here so the value plane has one
//! vocabulary door. Call sites may keep the thin aliases [`DomainCtx`] /
//! [`DomainCtxRefusal`]; those names are the same types.

pub use super::arena::{
    Admission, BulkSpendAuthority, Denial, DomainCtx, DomainCtxRefusal, NestId, NestedDomainCtx,
};

pub(super) use super::arena::BulkPass;

/// Nest-branded admission — thin alias naming the shared vocabulary.
pub type NestedAdmission<'id> = NestedDomainCtx<'id>;

/// Consumable admission after a domain check — thin alias naming the
/// shared vocabulary (`BulkSpendAuthority` remains the type's own name).
pub type SpendAdmission = BulkSpendAuthority;
