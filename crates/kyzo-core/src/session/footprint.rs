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
//! fence-pressure operator feed, overlap adjudication, Envelope soundness.
//!
//! Bans: Underivable inside Fenced; under-approximating footprints;
//! accelerator-as-admission-authority; caller-asserted accelerator
//! confirmation; durable lock organs, expiry timers, abandonment ceremonies.
//!
//! Live footprints exist only while the owning incarnation's write session
//! is live (session-memory). Prior-epoch / prior-incarnation footprints are
//! not live locks. Overlapping Fenced footprints refuse at the door.

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
    /// Sound-superset envelope — only via [`SoundEnvelope::seal`].
    Envelope(SoundEnvelope),
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

/// Envelope ranges proven to be a sound superset of declared access (§36).
///
/// Under-approximating an Envelope is Unconstructible — the only constructor
/// is [`SoundEnvelope::seal`], which refuses when `ranges` fail to cover
/// every declared-access range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundEnvelope {
    ranges: Vec<ByteRange>,
}

impl SoundEnvelope {
    /// Seal envelope ranges as a sound superset of `declared_access`.
    ///
    /// Every declared range must be covered by at least one envelope range.
    /// Under-approximation → [`FootprintRefuse::UnderApproximatingEnvelope`].
    pub fn seal(
        ranges: Vec<ByteRange>,
        declared_access: &[ByteRange],
    ) -> Result<Self, FootprintRefuse> {
        if !ranges_cover(&ranges, declared_access) {
            return Err(FootprintRefuse::UnderApproximatingEnvelope);
        }
        Ok(Self { ranges })
    }

    /// Borrow the sealed envelope ranges.
    pub fn ranges(&self) -> &[ByteRange] {
        &self.ranges
    }
}

/// True when every `declared` range is covered by some range in `superset`.
pub fn ranges_cover(superset: &[ByteRange], declared: &[ByteRange]) -> bool {
    declared
        .iter()
        .all(|d| superset.iter().any(|s| range_covers(s, d)))
}

fn range_covers(outer: &ByteRange, inner: &ByteRange) -> bool {
    outer.start.as_slice() <= inner.start.as_slice()
        && outer.end.as_slice() >= inner.end.as_slice()
}

/// Inclusive byte-range overlap.
pub fn ranges_overlap(a: &[ByteRange], b: &[ByteRange]) -> bool {
    a.iter().any(|ra| {
        b.iter().any(|rb| {
            ra.start.as_slice() <= rb.end.as_slice() && rb.start.as_slice() <= ra.end.as_slice()
        })
    })
}

/// Overlap adjudication between two footprints (§36).
///
/// Same-kind range / identity checks when decisive; cross-kind pairs that
/// cannot prove disjointness are treated as overlapping (conservative refuse).
pub fn footprints_overlap(a: &Footprint, b: &Footprint) -> bool {
    match (a, b) {
        (Footprint::Exact(ra), Footprint::Exact(rb))
        | (Footprint::Envelope(SoundEnvelope { ranges: ra }), Footprint::Envelope(SoundEnvelope { ranges: rb }))
        | (Footprint::Exact(ra), Footprint::Envelope(SoundEnvelope { ranges: rb }))
        | (Footprint::Envelope(SoundEnvelope { ranges: ra }), Footprint::Exact(rb)) => {
            ranges_overlap(ra, rb)
        }
        (Footprint::WholeRelation { relation: r1 }, Footprint::WholeRelation { relation: r2 }) => {
            r1 == r2
        }
        (Footprint::IndexDomain { projection: p1 }, Footprint::IndexDomain { projection: p2 }) => {
            p1 == p2
        }
        (Footprint::Frontier(f1), Footprint::Frontier(f2)) => f1.edge == f2.edge,
        (Footprint::Underivable, _) | (_, Footprint::Underivable) => true,
        // Cross-kind: cannot prove disjoint without a shared coordinate → overlap.
        _ => true,
    }
}

/// Adjudicate a candidate Fenced footprint against already-live shapes (§36).
///
/// Overlapping Fenced refuse at the door. Optimistic live shapes do not block.
pub fn adjudicate_fenced_overlap<'a>(
    candidate: &FencedFootprint,
    live: impl Iterator<Item = &'a AskShape>,
) -> Result<(), FootprintRefuse> {
    for shape in live {
        if let AskShape::Fenced(other) = shape {
            if footprints_overlap(candidate.footprint(), other.footprint()) {
                return Err(FootprintRefuse::OverlappingFenced);
            }
        }
    }
    Ok(())
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
    /// Neither conclusive nor confirmable against the reachability projection
    /// without authenticated confirmation evidence.
    Neither,
}

/// Authenticated confirmation against the authority reachability projection (§36).
///
/// Minted only by the projection door — caller-forged confirmation is
/// Unconstructible (no public constructor).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectionConfirmation {
    /// Admission-snapshot digest the projection confirmed against.
    snapshot_digest: [u8; 32],
}

impl ProjectionConfirmation {
    /// Mint confirmation from the authority reachability projection.
    ///
    /// Sole constructor — session/store projection seams only.
    pub(crate) fn mint(snapshot_digest: [u8; 32]) -> Self {
        Self { snapshot_digest }
    }

    /// Snapshot digest this confirmation binds.
    pub fn snapshot_digest(&self) -> &[u8; 32] {
        &self.snapshot_digest
    }
}

/// Apply accelerator against the authority projection.
///
/// Conclusive verdicts admit without confirmation. [`AcceleratorVerdict::Neither`]
/// requires [`ProjectionConfirmation`] minted by the projection door;
/// absent confirmation → [`FootprintRefuse::FrontierUnprovable`].
/// Caller-asserted `confirmable: bool` is Unconstructible.
pub fn admit_accelerator(
    verdict: AcceleratorVerdict,
    confirmation: Option<&ProjectionConfirmation>,
) -> Result<(), FootprintRefuse> {
    match verdict {
        AcceleratorVerdict::NegativeConclusive | AcceleratorVerdict::PositiveConclusive => Ok(()),
        AcceleratorVerdict::Neither => match confirmation {
            Some(_) => Ok(()),
            None => Err(FootprintRefuse::FrontierUnprovable),
        },
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

/// Typed outcome of a live-table insert — never a silent overwrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveInsert {
    /// No prior shape at this key.
    Fresh,
    /// Prior shape at this key replaced; previous returned.
    Replaced {
        /// Shape that occupied the key before this insert.
        previous: AskShape,
    },
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
    ///
    /// Fenced candidates are overlap-adjudicated against every other live
    /// Fenced shape (same key excluded — replacement is explicit via
    /// [`LiveInsert::Replaced`]). Overlapping Fenced → refuse. Silent
    /// overwrite is Unconstructible.
    pub fn insert(
        &mut self,
        key: FootprintIndexKey,
        shape: AskShape,
    ) -> Result<LiveInsert, FootprintRefuse> {
        if let AskShape::Fenced(ref fenced) = shape {
            adjudicate_fenced_overlap(
                fenced,
                self.live
                    .iter()
                    .filter(|(k, _)| **k != key)
                    .map(|(_, s)| s),
            )?;
        }
        match self.live.insert(key, shape) {
            None => Ok(LiveInsert::Fresh),
            Some(previous) => Ok(LiveInsert::Replaced { previous }),
        }
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
    #[error("FrontierUnprovable: accelerator Neither without ProjectionConfirmation — never admit")]
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
    /// Overlapping live Fenced footprints (§36) — refuse at the door.
    #[error("OverlappingFenced: overlapping Fenced footprints refuse at admission")]
    #[diagnostic(code(session::footprint::overlapping_fenced))]
    OverlappingFenced,
    /// Envelope ranges under-approximate declared access (§36).
    #[error("UnderApproximatingEnvelope: Envelope must be a sound superset of declared access")]
    #[diagnostic(code(session::footprint::under_approximating_envelope))]
    UnderApproximatingEnvelope,
}
