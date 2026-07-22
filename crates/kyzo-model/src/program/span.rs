/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): SourceSpan relocated from parse into the data layer;
 * Serialize/Deserialize dropped as never persisted; Display and merge
 * return with the parse tier.
 */

//! Source location: where in the query text a thing came from.
//!
//! A span is data-layer vocabulary, not a parser detail: every substance
//! that originates in user text (symbols, atoms, rules, expressions) carries
//! one, and every diagnostic points back through it. Errors that cannot say
//! *where* are not finished errors.

/// A byte range into the query text: offset and length.
///
/// Spans survive every pipeline stage — a compiled plan can still point at
/// the exact characters responsible for a runtime error.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSpan(pub usize, pub usize);

impl SourceSpan {
    /// The empty span at offset 0 — used when no source location is known.
    pub const fn empty() -> Self {
        SourceSpan(0, 0)
    }
}

impl std::fmt::Debug for SourceSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}..{}", self.0, self.0 + self.1)
    }
}

impl From<SourceSpan> for miette::SourceSpan {
    fn from(s: SourceSpan) -> Self {
        (s.0, s.1).into()
    }
}

impl From<&SourceSpan> for miette::SourceSpan {
    fn from(s: &SourceSpan) -> Self {
        (s.0, s.1).into()
    }
}

impl std::fmt::Display for SourceSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}..{}", self.0, self.0 + self.1)
    }
}

impl SourceSpan {
    /// The smallest span covering both `self` and `other`: diagnostics that
    /// implicate two places label the whole stretch between them.
    pub fn merge(self, other: Self) -> Self {
        let s1 = self.0;
        let e1 = self.0 + self.1;
        let s2 = other.0;
        let e2 = other.0 + other.1;
        let s = s1.min(s2);
        let e = e1.max(e2);
        Self(s, e - s)
    }
}
