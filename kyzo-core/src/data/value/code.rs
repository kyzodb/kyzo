/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Code(u32)`: the dense interned value handle — the hot-path identity the recursive relations and residency layers consume.

/// The dense handle for an interned value, scoped to an arena epoch.
///
/// Two disjoint ranges share the one `u32` space (the type-C contract; see
/// [`Arena`](super::arena::Arena)):
///
/// - **Sealed codes** `[0, sealed_len)`: the value's rank in the epoch's
///   sealed dictionary — among sealed codes of one epoch, rank order *is*
///   lexicographic byte order, and columnar kernels exploit that under a
///   container-level epoch stamp.
/// - **Tail codes** `[sealed_len, sealed_len + delta_len)`: arrival-stable
///   handles for values interned since the last seal. Equality (and hash)
///   are exact — the currency fixpoints need — but numeric order among
///   tail codes carries no byte-order meaning; order-sensitive operations
///   on tail values go through the prefix-first byte comparison instead.
///
/// A code is a point-in-epoch identity: stable until the arena seals, and
/// carried across a seal only through
/// [`EpochRemap::apply`](super::arena::EpochRemap::apply). Containers that
/// persist codes across seals own that boundary (they are epoch-stamped and
/// cross only through the typed gather door).
///
/// `Code` deliberately implements no `Ord`: a semantic comparison of two
/// codes is only sound with the arena (or a container's epoch stamp) in
/// hand, so it is spelled [`Arena::cmp_codes`](super::arena::Arena::cmp_codes)
/// — and structural ordering (deterministic iteration, dedup by identity)
/// is spelled explicitly over [`Code::raw`], which claims identity order,
/// never value order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Code(pub(crate) u32);

impl Code {
    /// The raw handle, for packed storage (#120's u32 runs, bitmaps,
    /// quantization). Reading is free; minting stays with the arena and
    /// the epoch-stamped containers.
    #[inline]
    pub fn raw(self) -> u32 {
        self.0
    }
}
