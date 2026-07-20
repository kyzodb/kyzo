/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Seat 8 forge wall — Store cannot mint meaning from bytes.
//!
//! Every Store read/decode door (SST/scan, WAL replay, object get, dump/restore)
//! yields ordered storage currency / [`super::objects::ObjectRef`] material only.
//! [`crate::session::admit::KyzoRecord`] constructors are private to the
//! admission seam (`session/admit.rs`); checksum-valid forge-as-record is
//! Unconstructible here.
//!
//! Proofs live in `kyzo-trials` (`forge_wall`): trybuild compile-fail that
//! store/encode/WAL/SST paths cannot construct a KyzoRecord, plus grep that
//! no put path embeds a forged record and no blob-form type sits on the
//! store admission surface.

/// Witness that this module is the named seat-8 wall — not a Record mint door.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreCurrencyOnly;

impl StoreCurrencyOnly {
    /// Bytes / ObjectRef only — never a Record constructor.
    pub const fn seal() -> Self {
        Self
    }
}
