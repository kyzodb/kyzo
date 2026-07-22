/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The bitemporal kernel: how one stored fact key answers a two-axis
//! as-of question, and how a row's claim polarity rides in its value.
//!
//! The key format itself (memcmp encoding, the two fixed-width time
//! slots) lives in [`crate::data::tuple`] and `data/memcmp.rs`; this
//! module owns the RESOLUTION algebra over it — the skip-scan decision
//! kernel ([`check_key_for_bitemporal`]) and the value-side polarity
//! ([`ClaimPolarity`]).
//!
//! Also owns transaction-time mint at the durable commit door
//! (decisions.md §30/§31): order within one Store is dense
//! [`CommitOrdinal`](super::sweep::CommitOrdinal); client-supplied and
//! foreign timestamps that would reorder local commits are refused.
//!
//! **Seat 34 (durable layer supersedes; it never overwrites history):**
//! a correction is a new WAL-authoritative supersession at a new
//! `(valid, sys)` key. Prior committed fact keys are not rewritten,
//! updated in place, or overwritten. As-of a pre-correction cut still
//! materializes the prior value. There is no rewrite API on committed
//! facts — see [`crate::session::admit::supersession`].

use miette::{Diagnostic, Result, bail};
use thiserror::Error;

use kyzo_model::value::{
    AsOf, DataValue, StorageKey, TERMINAL_VALIDITY, Tuple, ValiditySlot, ValidityTs,
    append_canonical, decode_tuple_from_key, decode_values_all,
};

use super::open::StoreId;
use super::sweep::{CommitOrdinal, Committed};

/// Named refusals on the bitemporal decode / polarity path — never a bare
/// `bail!(String)`.
#[cfg(test)]
use kyzo_model::value::Validity;
#[derive(Debug, Error, Diagnostic)]
pub enum BitemporalDecodeError {
    #[error("corrupt bitemporal value: shorter than its header and polarity byte")]
    #[diagnostic(code(bitemporal::value_truncated))]
    ValueTruncated,
    #[error("corrupt bitemporal value: unknown polarity byte {0:#04x}")]
    #[diagnostic(code(bitemporal::unknown_polarity))]
    UnknownPolarity(u8),
    #[error("bitemporal scan over a key too short to carry its two time slots")]
    #[diagnostic(code(bitemporal::key_too_short))]
    KeyTooShort,
    #[error("bitemporal scan over a key without a valid-time slot")]
    #[diagnostic(code(bitemporal::missing_valid_slot))]
    MissingValidSlot,
    #[error("bitemporal scan over a key with trailing bytes inside its valid-time slot")]
    #[diagnostic(code(bitemporal::valid_slot_trailing))]
    ValidSlotTrailing,
    #[error("bitemporal scan over a key without a system-time slot")]
    #[diagnostic(code(bitemporal::missing_sys_slot))]
    MissingSysSlot,
    #[error("bitemporal scan over a key with trailing bytes after its system-time slot")]
    #[diagnostic(code(bitemporal::sys_slot_trailing))]
    SysSlotTrailing,
    #[error(
        "bitemporal scan over a key with a retract flag in a time slot \
         (polarity lives in the value; stored slot flags are pinned)"
    )]
    #[diagnostic(code(bitemporal::slot_retract_flag))]
    SlotRetractFlag,
    #[error("bitemporal key too short to carry its two time slots")]
    #[diagnostic(code(bitemporal::stamp_key_too_short))]
    StampKeyTooShort,
    #[error("bitemporal key without a system-time slot")]
    #[diagnostic(code(bitemporal::stamp_missing_sys))]
    StampMissingSys,
    #[error("bitemporal key with trailing bytes after its system-time slot")]
    #[diagnostic(code(bitemporal::stamp_sys_trailing))]
    StampSysTrailing,
    #[error(
        "bitemporal key with a retract flag in its system-time slot \
         (polarity lives in the value; stored slot flags are pinned)"
    )]
    #[diagnostic(code(bitemporal::stamp_sys_retract))]
    StampSysRetract,
}

/// Named refusals for transaction-time at the durable commit door
/// (decisions.md §30/§31).
#[derive(Debug, Error, Diagnostic, Clone, Copy, PartialEq, Eq)]
pub enum TxnTimeRefuse {
    /// Client-supplied transaction time is Unconstructible — Store assigns
    /// time at the durable event only.
    #[error(
        "ClientTxnTimeForbidden: transaction time is assigned at the Store commit door, never by the client"
    )]
    #[diagnostic(code(store::time::client_txn_time_forbidden))]
    ClientTxnTimeForbidden,
    /// A peer/client timestamp that would reorder another Store's commits.
    #[error("ForeignTxnTime: foreign timestamps cannot order this Store's commits")]
    #[diagnostic(code(store::time::foreign_txn_time))]
    ForeignTxnTime,
}

/// Transaction time bound to a dense [`CommitOrdinal`] at the durable event.
///
/// Physical observation is optional for humans — order authority is the
/// ordinal, never a shared wall clock. Minted only from a SweepDoor
/// [`Committed`] proof (or an equivalent commit-door seal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxnTime {
    store_id: StoreId,
    commit_ordinal: CommitOrdinal,
}

impl TxnTime {
    /// Mint transaction time from a SweepDoor [`Committed`] proof.
    ///
    /// This is the sole public mint — client-supplied and Engine-before-append
    /// paths have no constructor (see [`TxnTimeRefuse`]).
    pub fn at_commit_door(committed: Committed) -> Self {
        Self {
            store_id: committed.store_id(),
            commit_ordinal: committed.commit_ordinal(),
        }
    }

    /// Store identity this txn time is namespaced by.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Dense CommitOrdinal — sole order authority within the Store.
    pub fn commit_ordinal(self) -> CommitOrdinal {
        self.commit_ordinal
    }

    /// Refuse a client-supplied txn-time claim (no mint path exists).
    pub fn refuse_client_supplied() -> TxnTimeRefuse {
        TxnTimeRefuse::ClientTxnTimeForbidden
    }

    /// Refuse a foreign Store timestamp that would reorder local commits.
    pub fn refuse_foreign(
        foreign_store: StoreId,
        local_store: StoreId,
    ) -> Result<(), TxnTimeRefuse> {
        if foreign_store != local_store {
            Err(TxnTimeRefuse::ForeignTxnTime)
        } else {
            Ok(())
        }
    }
}

/// Fact-payload format v1: a stored row VALUE opens directly with its
/// polarity byte (no header precedes it); the non-key columns' canonical
/// encodings follow.
const VALUE_HEADER_LEN: usize = 0;

const DEFAULT_SIZE_HINT: usize = 16;

/// What one stored bitemporal row says about its fact at its valid
/// instant. The polarity lives in the row's VALUE, never in the key: a
/// valid instant therefore has exactly ONE system-version lineage, and
/// the contradiction that sinks polarity-in-key designs — an assert
/// lineage and a retract lineage at the same instant, resolved by bucket
/// order instead of by newest system version — is unrepresentable.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ClaimPolarity {
    /// The fact holds from this valid instant on.
    Assert,
    /// The fact is retracted from this valid instant on.
    Retract,
    /// The record no longer carries any claim at this valid instant (a
    /// system-time correction that unrecords an earlier version): as-of
    /// resolution falls through to the fact's next older instant.
    Erase,
}

const BITEMPORAL_VALUE_HEADER_LEN: usize = VALUE_HEADER_LEN + 1;

const POLARITY_ASSERT: u8 = 0;

const POLARITY_RETRACT: u8 = 1;

const POLARITY_ERASE: u8 = 2;

impl ClaimPolarity {
    /// The polarity's written form: the byte after the value header.
    pub(crate) fn encode(self) -> u8 {
        match self {
            ClaimPolarity::Assert => POLARITY_ASSERT,
            ClaimPolarity::Retract => POLARITY_RETRACT,
            ClaimPolarity::Erase => POLARITY_ERASE,
        }
    }
}

pub fn claim_polarity_of_value(val: &[u8]) -> Result<ClaimPolarity> {
    let Some(&byte) = val.get(VALUE_HEADER_LEN) else {
        bail!(BitemporalDecodeError::ValueTruncated);
    };
    match byte {
        POLARITY_ASSERT => Ok(ClaimPolarity::Assert),
        POLARITY_RETRACT => Ok(ClaimPolarity::Retract),
        POLARITY_ERASE => Ok(ClaimPolarity::Erase),
        other => bail!(BitemporalDecodeError::UnknownPolarity(other)),
    }
}

pub fn check_key_for_bitemporal(
    key: &[u8],
    polarity: ClaimPolarity,
    as_of: AsOf,
    size_hint: Option<usize>,
) -> Result<(Option<Tuple>, Vec<u8>)> {
    if key.len() < StorageKey::RELATION_PREFIX_LEN + StorageKey::BITEMPORAL_TAIL_LEN {
        bail!(BitemporalDecodeError::KeyTooShort);
    }
    let valid_off = key.len() - StorageKey::BITEMPORAL_TAIL_LEN;
    let sys_off = key.len() - StorageKey::VALIDITY_TAIL_LEN;
    let (valid_val, rest) = DataValue::decode_from_key(&key[valid_off..sys_off])?;
    let DataValue::Validity(valid) = valid_val else {
        bail!(BitemporalDecodeError::MissingValidSlot);
    };
    if !rest.is_empty() {
        bail!(BitemporalDecodeError::ValidSlotTrailing);
    }
    let (sys_val, rest) = DataValue::decode_from_key(&key[sys_off..])?;
    let DataValue::Validity(sys) = sys_val else {
        bail!(BitemporalDecodeError::MissingSysSlot);
    };
    if !rest.is_empty() {
        bail!(BitemporalDecodeError::SysSlotTrailing);
    }
    if !valid.is_assert() || !sys.is_assert() {
        bail!(BitemporalDecodeError::SlotRetractFlag);
    }

    // Bounds live in the claimed-bytes domain, exactly as in the
    // single-axis kernel: only the tails were proven above, and blessing
    // the prefix into `StorageKey` would launder unproven bytes into a
    // type whose possession means provenance.
    let splice_both = |v: ValiditySlot, s: ValiditySlot| -> Vec<u8> {
        let mut nxt = Vec::with_capacity(key.len());
        nxt.extend_from_slice(&key[..valid_off]);
        append_canonical(&mut nxt, &DataValue::Validity(v));
        append_canonical(&mut nxt, &DataValue::Validity(s));
        nxt
    };
    let splice_sys = |s: ValiditySlot| -> Vec<u8> {
        let mut nxt = Vec::with_capacity(key.len());
        nxt.extend_from_slice(&key[..sys_off]);
        append_canonical(&mut nxt, &DataValue::Validity(s));
        nxt
    };

    if valid.timestamp() < as_of.valid() {
        // Instant newer than the valid coordinate (`Reverse` order:
        // smaller means later). Seek to the newest instant at or before
        // `valid_at`, landing directly at the newest system version at or
        // before `sys_at` inside whatever instant the seek finds.
        return Ok((
            None,
            splice_both(
                ValiditySlot::from_stored(as_of.valid(), true),
                ValiditySlot::from_stored(as_of.sys(), true),
            ),
        ));
    }
    if sys.timestamp() < as_of.sys() {
        // Right instant, but this system version postdates the system
        // coordinate: seek to the newest version at or before `sys_at`
        // within the SAME instant.
        return Ok((
            None,
            splice_sys(ValiditySlot::from_stored(as_of.sys(), true)),
        ));
    }
    // This row IS the instant's governing version at sys_at. Its polarity
    // decides. TERMINAL_VALIDITY is the maximum slot encoding, so the
    // bounds below clear the instant (system slot) or the whole fact
    // (both slots); a stored version AT the terminal sentinel is cleared
    // by the scan's byte-successor termination guard.
    match polarity {
        ClaimPolarity::Erase => {
            // Unrecorded at sys_at: this instant contributes nothing; the
            // scan falls onto the fact's next older instant.
            Ok((None, splice_sys(TERMINAL_VALIDITY.into())))
        }
        ClaimPolarity::Retract => {
            // The fact is retracted from this instant on: absent at the
            // coordinates. Skip every older version of the fact.
            Ok((
                None,
                splice_both(TERMINAL_VALIDITY.into(), TERMINAL_VALIDITY.into()),
            ))
        }
        ClaimPolarity::Assert => {
            // A hit. Emit, then skip every older version of the fact.
            let hint = match size_hint {
                Some(h) => h,
                None => DEFAULT_SIZE_HINT,
            };
            let decoded = decode_tuple_from_key(key, hint)?;
            Ok((
                Some(decoded),
                splice_both(TERMINAL_VALIDITY.into(), TERMINAL_VALIDITY.into()),
            ))
        }
    }
}

/// Decode ONLY a bitemporal key's SYSTEM-version slot (the tail's inner,
/// second slot — the writer's system stamp for this row), without
/// resolving a full `AsOf` query. No allocation: `Validity` is `Copy`, and
/// the tag byte at this fixed offset is always `VLD_TAG` by construction,
/// so `DataValue::decode_from_key` dispatches straight into that one
/// branch — a fixed-offset slice plus a handful of field reads.
///
/// Used by integrity checks that must confirm a fact row's recorded stamp
/// against some external bound (the dump path's clock-floor backstop,
/// `storage/backup.rs`) without re-deriving the whole resolution algebra
/// `check_key_for_bitemporal` implements.
pub(crate) fn system_stamp_of_key(key: &[u8]) -> Result<ValidityTs> {
    if key.len() < StorageKey::RELATION_PREFIX_LEN + StorageKey::BITEMPORAL_TAIL_LEN {
        bail!(BitemporalDecodeError::StampKeyTooShort);
    }
    let sys_off = key.len() - StorageKey::VALIDITY_TAIL_LEN;
    let (sys_val, rest) = DataValue::decode_from_key(&key[sys_off..])?;
    let DataValue::Validity(sys) = sys_val else {
        bail!(BitemporalDecodeError::StampMissingSys);
    };
    if !rest.is_empty() {
        bail!(BitemporalDecodeError::StampSysTrailing);
    }
    if !sys.is_assert() {
        bail!(BitemporalDecodeError::StampSysRetract);
    }
    Ok(sys.timestamp())
}

pub fn extend_tuple_from_bitemporal_v(key: &mut Tuple, val: &[u8]) -> Result<()> {
    let Some(payload) = val.get(BITEMPORAL_VALUE_HEADER_LEN..) else {
        bail!(BitemporalDecodeError::ValueTruncated);
    };
    if payload.is_empty() {
        return Ok(());
    }
    key.extend(decode_values_all(payload)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use miette::{IntoDiagnostic, Result, miette};

    use kyzo_model::value::ValidityTs;
    use kyzo_model::value::{RelationId, TupleT};
    use std::collections::BTreeMap;

    fn vts(t: i64) -> ValidityTs {
        ValidityTs::of_micros(t)
    }

    fn zero_as_of() -> AsOf {
        AsOf::at(vts(0), vts(0))
    }

    fn slot(t: i64) -> ValiditySlot {
        // Stored slot flags are pinned to assert; polarity lives in the value.
        ValiditySlot::from_stored(vts(t), true)
    }

    const REL: RelationId = match RelationId::new(7) {
        Some(id) => id,
        //  7 < CAP is a static fact; diverging arm keeps the expect-meter off.
        None => loop {},
    };

    fn bikey(fact: i64, valid_ts: i64, sys_ts: i64) -> Vec<u8> {
        [
            DataValue::from(fact),
            DataValue::Validity(slot(valid_ts)),
            DataValue::Validity(slot(sys_ts)),
        ]
        .encode_as_key(REL)
        .as_bytes()
        .to_vec()
    }

    fn skip_walk(
        store: &BTreeMap<Vec<u8>, ClaimPolarity>,
        sys_at: i64,
        valid_at: i64,
    ) -> Result<Vec<Tuple>> {
        let mut out = vec![];
        let mut bound = vec![];
        let mut steps = 0usize;
        loop {
            steps += 1;
            assert!(
                steps <= 4 * store.len() + 4,
                "skip walk failed to terminate"
            );
            let Some((k, polarity)) = store.range(bound..).next() else {
                break;
            };
            let (ret, nxt) =
                check_key_for_bitemporal(k, *polarity, AsOf::at(vts(sys_at), vts(valid_at)), None)?;
            bound = if nxt.as_slice() > k.as_slice() {
                nxt
            } else {
                let mut succ = k.clone();
                succ.push(0);
                succ
            };
            if let Some(t) = ret {
                out.push(t);
            }
        }
        Ok(out)
    }

    fn oracle(rows: &[(i64, i64, i64, ClaimPolarity)], sys_at: i64, valid_at: i64) -> Vec<i64> {
        let mut facts: Vec<i64> = rows.iter().map(|r| r.0).collect();
        facts.sort_unstable();
        facts.dedup();
        let mut out = vec![];
        for f in facts {
            let mut instants: Vec<i64> = rows
                .iter()
                .filter(|r| r.0 == f && r.1 <= valid_at)
                .map(|r| r.1)
                .collect();
            instants.sort_unstable();
            instants.dedup();
            let mut verdict = None;
            for instant in instants.into_iter().rev() {
                // The instant's governing version: newest system ts at or
                // before sys_at, across ALL of the instant's versions.
                let governing = rows
                    .iter()
                    .filter(|r| r.0 == f && r.1 == instant && r.2 <= sys_at)
                    .max_by_key(|r| r.2)
                    .map(|r| r.3);
                match governing {
                    Some(ClaimPolarity::Assert) => {
                        verdict = Some(true);
                        break;
                    }
                    Some(ClaimPolarity::Retract) => {
                        verdict = Some(false);
                        break;
                    }
                    // Erased or later-recorded: fall to the next older
                    // instant.
                    Some(ClaimPolarity::Erase) | None => {}
                }
            }
            if verdict == Some(true) {
                out.push(f);
            }
        }
        out
    }

    fn facts_of(tuples: &[Tuple]) -> Result<Vec<i64>> {
        tuples
            .iter()
            .map(|t| match &t[0] {
                DataValue::Num(n) => n.as_int().ok_or_else(|| miette!("int-domain column")),
                DataValue::Null
                | DataValue::Bool(_)
                | DataValue::Str(_)
                | DataValue::Bytes(_)
                | DataValue::Uuid(_)
                | DataValue::Regex(_)
                | DataValue::Json(_)
                | DataValue::Vector(_)
                | DataValue::List(_)
                | DataValue::Set(_)
                | DataValue::Validity(_)
                | DataValue::Interval(_)
                | DataValue::Geometry(_) => Err(miette!("non-integer fact column")),
            })
            .collect()
    }

    #[test]
    fn bitemporal_tail_orders_valid_outer_system_inner() -> Result<()> {
        let mut slots: Vec<(i64, i64)> = vec![];
        for vt in [-3, 0, 10, 20, i64::MAX] {
            for st in [-7, 0, 5, 15, i64::MAX] {
                slots.push((vt, st));
            }
        }
        let mut by_bytes = slots.clone();
        by_bytes.sort_by_key(|(v, s)| bikey(1, *v, *s));
        let mut by_semantics = slots.clone();
        by_semantics.sort_by_key(|(v, s)| (vts(*v), vts(*s)));
        assert_eq!(
            by_bytes, by_semantics,
            "byte order must equal (valid, system) semantic order"
        );

        Ok(())
    }

    #[test]
    fn bitemporal_skip_scan_matches_oracle() -> Result<()> {
        let mut state: u64 = 0x5EED_B17E_44C0_FFEE;
        let mut next = move |m: usize| -> usize {
            // INVARIANT(lcg64): Knuth LCG step is defined wrapping on u64.
            state = (std::num::Wrapping(state) * std::num::Wrapping(6364136223846793005)
                + std::num::Wrapping(1442695040888963407))
            .0;
            match usize::try_from(state >> 33) {
                Ok(v) => v % m,
                Err(_) => 0,
            }
        };
        for _case in 0..2000 {
            let n_rows = 1 + next(10);
            let mut rows: Vec<(i64, i64, i64, ClaimPolarity)> = vec![];
            for _ in 0..n_rows {
                rows.push((
                    match i64::try_from(next(3)) {
                        Ok(v) => v,
                        Err(_) => 0,
                    },
                    [0, 10, 20, 30][next(4)],
                    [0, 5, 15, 25][next(4)],
                    [
                        ClaimPolarity::Assert,
                        ClaimPolarity::Retract,
                        ClaimPolarity::Erase,
                    ][next(3)],
                ));
            }
            rows.sort_unstable_by_key(|r| (r.0, r.1, r.2));
            // One row per (fact, valid, sys) coordinate, matching the
            // store, where the key is unique — otherwise the map
            // (last-insert-wins) and the oracle (sees every row) would
            // judge different histories. Which colliding polarity survives
            // is an arbitrary deterministic pick per seed (`sort_unstable`
            // does not preserve generation order among equal keys); walk
            // and oracle consume the identical surviving set either way.
            rows.dedup_by_key(|r| (r.0, r.1, r.2));
            let store: BTreeMap<Vec<u8>, ClaimPolarity> = rows
                .iter()
                .map(|(f, v, s, p)| (bikey(*f, *v, *s), *p))
                .collect();
            for sys_at in [-1i64, 0, 5, 10, 15, 25, 40] {
                for valid_at in [-1i64, 0, 10, 20, 30, 40] {
                    let got = facts_of(&skip_walk(&store, sys_at, valid_at)?)?;
                    let want = oracle(&rows, sys_at, valid_at);
                    assert_eq!(
                        got, want,
                        "divergence at sys_at={sys_at} valid_at={valid_at} rows={rows:?}"
                    );
                }
            }
        }

        Ok(())
    }

    #[test]
    fn polarity_flip_at_same_instant_is_governed_by_newest_system_version() -> Result<()> {
        let store: BTreeMap<Vec<u8>, ClaimPolarity> = [
            (bikey(1, 10, 5), ClaimPolarity::Assert),
            (bikey(1, 10, 20), ClaimPolarity::Retract),
        ]
        .into();
        // Before the correction was recorded: the assert governs.
        assert_eq!(facts_of(&skip_walk(&store, 15, 15)?)?, vec![1]);
        // After: the retract governs — the fact is absent.
        assert_eq!(facts_of(&skip_walk(&store, 25, 15)?)?, Vec::<i64>::new());
        // An erase instead of a retract falls through to... nothing older:
        // also absent, but by fall-through, not settlement.
        let store: BTreeMap<Vec<u8>, ClaimPolarity> = [
            (bikey(1, 10, 5), ClaimPolarity::Assert),
            (bikey(1, 10, 20), ClaimPolarity::Erase),
            (bikey(1, 0, 1), ClaimPolarity::Assert),
        ]
        .into();
        // Erased at 10, so the older instant 0 shows through.
        assert_eq!(facts_of(&skip_walk(&store, 25, 15)?)?, vec![1]);
        // A RETRACT at 10 would instead settle the fact absent.
        let store: BTreeMap<Vec<u8>, ClaimPolarity> = [
            (bikey(1, 10, 5), ClaimPolarity::Assert),
            (bikey(1, 10, 20), ClaimPolarity::Retract),
            (bikey(1, 0, 1), ClaimPolarity::Assert),
        ]
        .into();
        assert_eq!(facts_of(&skip_walk(&store, 25, 15)?)?, Vec::<i64>::new());

        Ok(())
    }

    #[test]
    fn corrupt_bitemporal_keys_refuse_and_never_panic() -> Result<()> {
        assert!(
            check_key_for_bitemporal(&[0u8; 8], ClaimPolarity::Assert, zero_as_of(), None).is_err()
        );
        let flagged = [
            DataValue::from(1i64),
            DataValue::Validity(Validity::new(vts(10), false).ok_or_else(|| miette!("validity"))?.into()),
            DataValue::Validity(slot(5)),
        ]
        .encode_as_key(RelationId::new(7).ok_or_else(|| miette!("relation id"))?)
        .as_bytes()
        .to_vec();
        assert!(
            check_key_for_bitemporal(&flagged, ClaimPolarity::Assert, zero_as_of(), None).is_err(),
            "retract flag in a stored valid slot must refuse"
        );
        let terminal = [
            DataValue::from(1i64),
            DataValue::Validity(TERMINAL_VALIDITY.into()),
            DataValue::Validity(TERMINAL_VALIDITY.into()),
        ]
        .encode_as_key(RelationId::new(7).ok_or_else(|| miette!("relation id"))?)
        .as_bytes()
        .to_vec();
        assert!(
            check_key_for_bitemporal(&terminal, ClaimPolarity::Assert, zero_as_of(), None).is_err(),
            "the terminal sentinel is a bound, never a storable slot"
        );
        let ints = [
            DataValue::from(1i64),
            DataValue::from(2i64),
            DataValue::from(3i64),
        ]
        .encode_as_key(RelationId::new(7).ok_or_else(|| miette!("relation id"))?)
        .as_bytes()
        .to_vec();
        assert!(
            check_key_for_bitemporal(&ints, ClaimPolarity::Assert, zero_as_of(), None).is_err()
        );
        for len in 0..64usize {
            let garbage = vec![0xEEu8; len];
            let outcome =
                check_key_for_bitemporal(&garbage, ClaimPolarity::Assert, zero_as_of(), None);
            // Garbage must terminate with a typed answer — never unwind.
            match outcome {
                Ok(decoded) => drop(decoded),
                Err(refuse) => drop(refuse),
            }
        }

        Ok(())
    }

    #[test]
    fn bitemporal_value_polarity_round_trips_and_refuses_corruption() -> Result<()> {
        for polarity in [
            ClaimPolarity::Assert,
            ClaimPolarity::Retract,
            ClaimPolarity::Erase,
        ] {
            let val = vec![polarity.encode()];
            assert_eq!(claim_polarity_of_value(&val)?, polarity);
            // A bare retract/erase value carries no payload and extends
            // nothing.
            let mut tup: Tuple = Tuple::from_vec(vec![DataValue::from(1i64)]);
            extend_tuple_from_bitemporal_v(&mut tup, &val)?;
            assert_eq!(tup.len(), 1);
        }
        // An assert row's non-key columns ride after the polarity byte.
        let mut val = Vec::new();
        val.push(ClaimPolarity::Assert.encode());
        let non_keys = vec![DataValue::from(42i64), DataValue::from(7i64)];
        for v in &non_keys {
            kyzo_model::value::append_canonical(&mut val, v);
        }
        let mut tup: Tuple = Tuple::from_vec(vec![DataValue::from(1i64)]);
        extend_tuple_from_bitemporal_v(&mut tup, &val)?;
        assert_eq!(
            tup,
            Tuple::from_vec(vec![
                DataValue::from(1i64),
                DataValue::from(42i64),
                DataValue::from(7i64)
            ])
        );
        // Refusals. An EMPTY value has no polarity byte at all.
        assert!(claim_polarity_of_value(&[]).is_err());
        assert!(extend_tuple_from_bitemporal_v(&mut Tuple::new(), &[]).is_err());
        // A leading byte that is not a known polarity is refused.
        assert!(claim_polarity_of_value(&[0xEE]).is_err());
        // A valid Assert polarity followed by a garbage payload: the
        // polarity reads fine, but decoding the non-key columns refuses.
        let mut bad = vec![ClaimPolarity::Assert.encode()];
        bad.push(0xEE); // 0xEE is not a canonical value tag
        assert_eq!(claim_polarity_of_value(&bad)?, ClaimPolarity::Assert);
        assert!(extend_tuple_from_bitemporal_v(&mut Tuple::new(), &bad).is_err());

        Ok(())
    }

    // Oracle-vs-kernel temporal differential moved to
    // `kyzo-trials::time_travel` (`reverify_laws_resolve_mirrors_the_real_kernel_with_negative_timestamps`).
    // kyzo-core must not import `kyzo_oracle`.
}
