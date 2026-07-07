/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Code(u32)`: the dense interned value handle — the hot-path identity the recursive relations and residency layers consume.

/// The raw dense handle for an interned value: **identity only, no read
/// authority**.
///
/// Type-C's law is that a code means something only inside a scoped
/// observer frame, so no read API anywhere accepts a bare `Code`. What a
/// `Code` *can* do is be identity: equality, hashing, and packed storage
/// (`raw()` for #120's u32 runs, bitmaps, quantization) — always under a
/// container-level epoch stamp. To spend one you need its epoch:
///
/// - [`StampedCode`] — code + epoch, minted by `Arena::intern` and by
///   [`EpochRemap::apply`](super::arena::EpochRemap::apply) (the morphism
///   between frames);
/// - [`Frame::admit`](super::arena::Frame::admit) — turns a `StampedCode`
///   into a frame-bound [`FrameCode`](super::arena::FrameCode) witness,
///   spendable for exactly as long as the borrow checker lets the frame
///   live;
/// - snapshots spend `StampedCode` directly under exact runtime asserts.
///
/// There is deliberately no `Ord`: order is the arena's to answer, inside
/// a frame. Structural ordering (deterministic iteration, dedup by
/// identity) is spelled over [`Code::raw`], which claims identity order,
/// never value order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Code(pub(super) u32);

impl Code {
    /// The raw handle, for packed storage. Reading is free; minting stays
    /// with the arena and the epoch-stamped containers.
    #[inline]
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// A code together with the arena identity and epoch that give it meaning:
/// the loose-scalar currency for holding a value's identity across
/// statements.
///
/// Minted by `Arena::intern` (stamping the current epoch) and by
/// [`EpochRemap::apply`](super::arena::EpochRemap::apply) (restamping into
/// the next epoch). Spending requires an observer frame: admit it into a
/// live [`Frame`](super::arena::Frame) or hand it to a
/// [`Snapshot`](super::arena::Snapshot), both of which verify the stamp
/// exactly. Persistent containers of many codes carry one stamp for all of
/// them and cross epochs only through the gather door.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StampedCode {
    code: Code,
    epoch: super::arena::Epoch,
    arena: super::arena::ArenaId,
}

impl StampedCode {
    /// Minting authority is the value plane only: `Arena::intern`,
    /// `EpochRemap::apply`, and the plane's own tests. A stamp that exists
    /// was issued by the arena — the law is a type boundary, not a
    /// crate-wide convention.
    #[inline]
    pub(super) fn mint(
        code: Code,
        epoch: super::arena::Epoch,
        arena: super::arena::ArenaId,
    ) -> Self {
        StampedCode { code, epoch, arena }
    }

    /// The arena this stamp belongs to (plane-internal: observers verify
    /// it on every admit/spend).
    #[inline]
    pub(super) fn arena(self) -> super::arena::ArenaId {
        self.arena
    }

    /// The raw identity, for packing under a container-level stamp.
    #[inline]
    pub fn code(self) -> Code {
        self.code
    }

    /// The epoch this code is stamped for.
    #[inline]
    pub fn epoch(self) -> super::arena::Epoch {
        self.epoch
    }
}
