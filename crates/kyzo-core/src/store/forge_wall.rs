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
//! Seat 8 is proven by module/field privacy (`pub(crate) mod session` →
//! `pub(crate) mod admit`, private `KyzoRecord.core` — sibling `store` cannot
//! construct; `cargo check -p kyzo`) plus the grep-proof harness in
//! `kyzo-trials` (`forge_wall_grep_*`: no put path embeds a forged record, no
//! blob-form type on the store admission surface). External trybuild cannot
//! exercise that `pub(crate)` internal wall without exposing admission at the
//! crate door.

/// Witness that this module is the named seat-8 wall — not a Record mint door.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreCurrencyOnly;

impl StoreCurrencyOnly {
    /// Bytes / ObjectRef only — never a Record constructor.
    pub const fn seal() -> Self {
        Self
    }
}
