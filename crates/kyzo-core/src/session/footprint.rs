/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! AskShape + footprint algebra + Frontier (decisions.md §36).
//!
//! Owns: [`AskShape`], [`Footprint`], [`Frontier`], [`EdgePredicate`],
//! accelerator contract, `(FenceEpoch, IncarnationId)` footprint index,
//! fence-pressure operator feed.
//!
//! Bans: Underivable inside Fenced; under-approximating footprints;
//! accelerator-as-admission-authority; durable lock organs, expiry timers,
//! abandonment ceremonies.
//!
//! Live footprints exist only while the owning incarnation's write session
//! is live (session-memory). Prior-epoch / prior-incarnation footprints are
//! not live locks.

use crate::store::authority::IncarnationId;
use crate::store::epoch::FenceEpoch;

/// Sealed ask shape at admission (§36).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskShape {
    /// Full SSI; conflict is a terminal typed refuse carrying ranges.
    Optimistic,
    /// Sound-superset footprint sealed with Catalog generation.
    ///
    /// [`Footprint::Underivable`] is Unconstructible here — sealed only via
    /// [`FencedFootprint::seal`].
    Fenced(FencedFootprint),
}

/// Footprint sealed sum (§36) — Exact | Envelope | IndexDomain | WholeRelation | Frontier | Underivable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Footprint {
    /// Exact key ranges.
    Exact(Vec<ByteRange>),
    /// Sound-superset envelope over ranges.
    Envelope(Vec<ByteRange>),
    /// Index-domain projection.
    IndexDomain {
        /// Projection identity (relation / index name digest).
        projection: [u8; 32],
    },
    /// Whole relation (sugar for full Envelope).
    WholeRelation {
        /// Relation identity digest.
        relation: [u8; 32],
    },
    /// Reachability frontier under an edge predicate.
    Frontier(Frontier),
    /// Cannot derive a sound footprint — legal only under Optimistic planning.
    Underivable,
}

/// Inclusive byte range for Exact / Envelope footprints.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ByteRange {
    /// Inclusive start.
    pub start: Vec<u8>,
    /// Inclusive end.
    pub end: Vec<u8>,
}

/// Frontier direction — only Forward is representable (reverse fails to compile).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Forward reachability from the anchor.
    Forward,
}

/// Edge predicate sealed at Catalog / plan time — total over closed relations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EdgePredicate {
    /// Catalog-sealed predicate identity.
    pub id: [u8; 32],
}

/// Reachability frontier: Direction is in the type; self-extension is a plan error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontier {
    /// Anchor key / node digest.
    pub anchor: Vec<u8>,
    /// Catalog-sealed edge predicate.
    pub edge: EdgePredicate,
    /// Only [`Direction::Forward`] exists — reverse Frontier is Unconstructible.
    pub direction: Direction,
}

/// Footprint arms legal under [`AskShape::Fenced`] — Underivable excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FencedFootprint {
    inner: Footprint,
    /// Catalog generation (Store commit position) sealed with the footprint.
    catalog_generation: u64,
}

impl FencedFootprint {
    /// Seal a sound-superset footprint for Fenced admission.
    ///
    /// [`Footprint::Underivable`] → refuse (Unconstructible at plan construction).
    pub fn seal(footprint: Footprint, catalog_generation: u64) -> Result<Self, FootprintRefuse> {
        match footprint {
            Footprint::Underivable => Err(FootprintRefuse::UnderivableInFenced),
            other => Ok(Self {
                inner: other,
                catalog_generation,
            }),
        }
    }

    /// Borrow the sealed footprint.
    pub fn footprint(&self) -> &Footprint {
        &self.inner
    }

    /// Catalog generation sealed with this footprint.
    pub fn catalog_generation(&self) -> u64 {
        self.catalog_generation
    }
}

/// Accelerator verdict (§36) — accelerates; never admission authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AcceleratorVerdict {
    /// Negative conclusive (absent).
    NegativeConclusive,
    /// Positive conclusive (present).
    PositiveConclusive,
    /// Neither conclusive nor confirmable against the reachability projection.
    Neither,
}

/// Apply accelerator against the authority projection.
///
/// `Neither` that is also unconfirmable → [`FootprintRefuse::FrontierUnprovable`].
pub fn admit_accelerator(
    verdict: AcceleratorVerdict,
    confirmable: bool,
) -> Result<(), FootprintRefuse> {
    match verdict {
        AcceleratorVerdict::NegativeConclusive | AcceleratorVerdict::PositiveConclusive => Ok(()),
        AcceleratorVerdict::Neither if confirmable => Ok(()),
        AcceleratorVerdict::Neither => Err(FootprintRefuse::FrontierUnprovable),
    }
}

/// Live footprint index key: `(FenceEpoch, IncarnationId)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FootprintIndexKey {
    /// Fence epoch of the owning session.
    pub fence_epoch: FenceEpoch,
    /// Incarnation of the owning write session.
    pub incarnation_id: IncarnationId,
}

impl PartialOrd for FootprintIndexKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FootprintIndexKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.fence_epoch
            .cmp(&other.fence_epoch)
            .then_with(|| {
                self.incarnation_id
                    .open_ordinal()
                    .cmp(&other.incarnation_id.open_ordinal())
            })
            .then_with(|| {
                self.incarnation_id
                    .entropy()
                    .as_bytes()
                    .cmp(other.incarnation_id.entropy().as_bytes())
            })
    }
}

/// Session-memory live footprint table — no durable lock organ.
#[derive(Debug, Default)]
pub struct LiveFootprintTable {
    live: std::collections::BTreeMap<FootprintIndexKey, AskShape>,
}

impl LiveFootprintTable {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a live footprint for the owning incarnation.
    pub fn insert(&mut self, key: FootprintIndexKey, shape: AskShape) {
        self.live.insert(key, shape);
    }

    /// Drop footprints for a dead incarnation (next open / session end).
    pub fn drop_incarnation(&mut self, incarnation: IncarnationId) {
        self.live.retain(|k, _| k.incarnation_id != incarnation);
    }

    /// Drop all footprints of a prior epoch (not live locks).
    pub fn drop_epoch(&mut self, epoch: FenceEpoch) {
        self.live.retain(|k, _| k.fence_epoch != epoch);
    }

    /// Count of live Fenced footprints (fence-pressure operator feed).
    pub fn fence_pressure(&self) -> u64 {
        self.live
            .values()
            .filter(|s| matches!(s, AskShape::Fenced(_)))
            .count() as u64
    }

    /// Whether any current-epoch Fenced footprint is live (blocks ordinary advance).
    pub fn has_live_fenced_in_epoch(&self, epoch: FenceEpoch) -> bool {
        self.live.iter().any(|(k, s)| {
            k.fence_epoch == epoch && matches!(s, AskShape::Fenced(_))
        })
    }
}

/// Conflicting ranges carried by a terminal SSI refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRanges {
    /// Ranges that conflicted under Optimistic SSI.
    pub ranges: Vec<ByteRange>,
}

/// Footprint / AskShape admission refuses (session door — not StoreRefuse).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum FootprintRefuse {
    /// Underivable inside Fenced.
    #[error("UnderivableInFenced: Underivable is Unconstructible under AskShape::Fenced")]
    #[diagnostic(code(session::footprint::underivable_in_fenced))]
    UnderivableInFenced,
    /// Frontier neither conclusive nor confirmable (§36).
    #[error("FrontierUnprovable: accelerator Neither and unconfirmable — never admit")]
    #[diagnostic(code(session::footprint::frontier_unprovable))]
    FrontierUnprovable,
    /// Terminal Optimistic SSI conflict — caller re-derives; no retry counters.
    #[error("OptimisticConflict: terminal SSI conflict")]
    #[diagnostic(code(session::footprint::optimistic_conflict))]
    OptimisticConflict {
        /// Conflicting ranges.
        ranges: ConflictRanges,
    },
    /// Catalog generation mismatch at the door.
    #[error("CatalogGenerationMismatch: footprint sealed against a stale Catalog generation")]
    #[diagnostic(code(session::footprint::catalog_generation_mismatch))]
    CatalogGenerationMismatch,
}
