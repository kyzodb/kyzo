/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! [`Arity`]: relation-fragment width proven nonzero.
//!
//! A collection illegal when empty (here: a tuple width of zero) is a
//! non-empty type — never a bare `usize` re-asserted at every constructor.
//! Zero is unrepresentable: construction takes [`NonZeroUsize`].

use std::num::NonZeroUsize;

/// Column count of a relation fragment: at least one.
///
/// Private field; the only public door is [`Arity::new`] over a proven
/// [`NonZeroUsize`]. Call sites that still hold a bare `usize` lift through
/// [`Arity::try_new`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Arity(NonZeroUsize);

impl Arity {
    /// One column — the minimum lawful width.
    pub const ONE: Self = Self(NonZeroUsize::MIN);

    /// Infallible: `NonZeroUsize` already proves `≥ 1`.
    pub const fn new(width: NonZeroUsize) -> Self {
        Self(width)
    }

    /// Fallible lift from a bare count at a boundary that has not yet
    /// proven non-zero. The only door from a bare `usize`.
    pub const fn try_new(width: usize) -> Option<Self> {
        match NonZeroUsize::new(width) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The underlying column count.
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl From<NonZeroUsize> for Arity {
    fn from(width: NonZeroUsize) -> Self {
        Self::new(width)
    }
}

impl From<Arity> for usize {
    fn from(arity: Arity) -> Self {
        arity.get()
    }
}

impl From<Arity> for NonZeroUsize {
    fn from(arity: Arity) -> Self {
        arity.0
    }
}
