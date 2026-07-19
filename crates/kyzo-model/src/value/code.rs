/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Code(u32)`: the dense interned value handle — the hot-path identity the recursive relations and residency layers consume.
//!
//! ## Lifetime brands vs coexisting arenas (ceiling measurement)
//!
//! An invariant-lifetime brand
//! ([`NestId`](super::arena::NestId) / [`NestedDomainCtx`](super::arena::NestedDomainCtx))
//! mints a compiler-unique identity per nest at zero cost and is applied
//! where a single live observer nest is provable
//! ([`Frame::with_nested_ctx`](super::arena::Frame::with_nested_ctx) /
//! [`Snapshot::with_nested_ctx`](super::arena::Snapshot::with_nested_ctx)).
//!
//! **Rejection of full branding:** a lifetime-branded *spend* witness
//! cannot prove frame identity across coexisting arenas — two frames over
//! different arenas can share a borrow lifetime, so a brand that unified
//! them would claim a safety it cannot deliver. KyzoDB's executor holds
//! multiple arenas live simultaneously, and epoch-stamped containers
//! outlive any one frame borrow. That measurement bounds full
//! lifetime-branding to nesting scopes; where instances coexist, the
//! ceiling is the mint-checked [`Admission`](super::admission::Admission)
//! token ([`Admission::prove_shared`](super::admission::Admission::prove_shared)),
//! which still deletes every domain-mixup panic and every unproven
//! comparison. Admission and [`Denial`](super::admission::Denial) speak
//! one vocabulary ([`super::admission`]).

/// The raw dense handle for an interned value: **identity only, no read
/// authority**. By design, no read API anywhere accepts a bare `Code` —
/// codes are only valid as observed through an arena [`Frame`](super::arena::Frame)
/// or [`Snapshot`](super::arena::Snapshot). Handle equality and
/// identity-order are [`Admission::same_handle`](super::admission::Admission::same_handle)
/// / [`Admission::cmp_identity`](super::admission::Admission::cmp_identity)
/// (or the nest-branded [`NestedDomainCtx`](super::admission::NestedDomainCtx)
/// under one live observer) — packed storage uses [`Code::raw`] under a
/// container that already holds the domain proof. To spend one you need
/// its epoch and arena:
///
/// - [`StampedCode`] — code + epoch, minted by `Arena::intern` and by
///   [`EpochRemap::apply`](super::arena::EpochRemap::apply) (the morphism
///   between frames);
/// - live [`Frame`](super::arena::Frame)s and pinned
///   [`Snapshot`](super::arena::Snapshot)s both spend a `StampedCode`
///   directly, proving arena identity and epoch via
///   [`Admission::prove_shared`](super::admission::Admission::prove_shared)
///   on every spend (typed [`Denial`](super::admission::Denial) — see
///   module-level coexisting-arena measurement; no lifetime-branded spend
///   witness is offered).
///
/// There is deliberately no inherent `PartialEq`/`Eq`/`Hash`/`Ord`: a
/// derived equality would let `code_a == code_b` compile with no context;
/// value order is the arena's to answer inside a frame. Structural
/// identity and identity-order under a proven context are
/// [`Admission::same_handle`](super::admission::Admission::same_handle) /
/// [`Admission::cmp_identity`](super::admission::Admission::cmp_identity).
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct Code(pub(super) u32);

impl Code {
    /// The raw handle, for packed storage under a proven domain. Reading
    /// is free; minting stays with the arena and the epoch-stamped
    /// containers. Comparing handles for identity or order requires a
    /// [`Admission`](super::admission::Admission).
    #[inline]
    pub fn raw(self) -> u32 {
        self.0
    }
}

const _: () = assert!(std::mem::size_of::<Code>() == std::mem::size_of::<u32>());
const _: () = assert!(std::mem::align_of::<Code>() == std::mem::align_of::<u32>());

/// A code together with the arena identity and epoch that give it meaning:
/// the loose-scalar currency for holding a value's identity across
/// statements.
///
/// Minted by `Arena::intern` (stamping the current epoch) and by
/// [`EpochRemap::apply`](super::arena::EpochRemap::apply) (restamping into
/// the next epoch). Spending requires an observer frame: admit it into a
/// live [`Frame`](super::arena::Frame) or hand it to a
/// [`Snapshot`](super::arena::Snapshot), both of which prove the stamp via
/// typed refusal. Persistent containers of many codes carry one stamp for
/// all of them and cross epochs only through the gather door.
#[derive(Clone, Copy, Debug)]
pub struct StampedCode {
    code: Code,
    epoch: super::arena::Epoch,
    arena: super::arena::ArenaId,
}

/// Stamp equality is (arena, epoch, raw handle) — the stamp carries its own
/// context fields. Bare [`Code`] still has no `PartialEq`.
impl PartialEq for StampedCode {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.arena == other.arena
            && self.epoch == other.epoch
            && self.code.raw() == other.code.raw()
    }
}
impl Eq for StampedCode {}
impl std::hash::Hash for StampedCode {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.arena.hash(state);
        self.epoch.hash(state);
        self.code.raw().hash(state);
    }
}

impl StampedCode {
    /// Minting requires the arena's authority token
    /// ([`StampMintAuthority`](super::arena::StampMintAuthority)), whose
    /// only constructor is private to `arena.rs` — neighboring plane
    /// modules can *name* this mint but cannot *call* it. Authority is a
    /// per-concept compile fact, not a module-prefix convention.
    #[inline]
    pub(super) fn mint(
        code: Code,
        epoch: super::arena::Epoch,
        arena: super::arena::ArenaId,
        _authority: super::arena::StampMintAuthority,
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
