/*
 * Copyright 2025 the Kyzo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The one allocation-admission seam.
//!
//! A caller- or user-declared size (a search `k`, an enumeration budget, any
//! count that arrives from outside and is not itself bounded by data already
//! in memory) must never reach `Vec::with_capacity`/`reserve` directly:
//! reserving straight from an absurd declared size aborts the allocator
//! before a single result is produced. The historical fix was a per-site
//! `declared.min(available)` written at each reservation — findings-driven
//! patches, not a law.
//!
//! [`admit`] is that law made one function: EVERY input-declared reservation
//! is admitted here, bounded by a `available` count that is itself proven
//! finite (typically the number of candidate items that actually exist). The
//! `allocation_admission` resonance check enforces the seam mechanically —
//! no `with_capacity`/`reserve` argument may carry its own `.min(...)` cap;
//! capacity capping happens in exactly this one place.

/// Admit a caller/user-declared reservation size, bounded by a proven-finite
/// `available` count. The result — never larger than `available` — is safe to
/// hand to `Vec::with_capacity`/`reserve`: an absurd `declared` can reserve at
/// most what actually exists, never enough to abort the allocator.
///
/// When no finite `available` bound exists (the final count is unknown, as in
/// a path-enumeration loop), do not reserve at all — grow amortized; the final
/// length is the same. This seam is for the bounded case; unbounded growth
/// needs no reservation and so no admission.
pub(crate) fn admit(declared: usize, available: usize) -> usize {
    declared.min(available)
}
