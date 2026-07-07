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

use std::cmp::Reverse;

use miette::{Result, bail};

use crate::data::value::{
    AsOf, DataValue, EncodedKey, TERMINAL_VALIDITY, Tuple, Validity, ValidityTs, append_canonical,
    decode_tuple_from_key, decode_values_all,
};

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
        bail!("corrupt bitemporal value: shorter than its header and polarity byte");
    };
    match byte {
        POLARITY_ASSERT => Ok(ClaimPolarity::Assert),
        POLARITY_RETRACT => Ok(ClaimPolarity::Retract),
        POLARITY_ERASE => Ok(ClaimPolarity::Erase),
        other => bail!("corrupt bitemporal value: unknown polarity byte {other:#04x}"),
    }
}

pub fn check_key_for_bitemporal(
    key: &[u8],
    polarity: ClaimPolarity,
    as_of: AsOf,
    size_hint: Option<usize>,
) -> Result<(Option<Tuple>, Vec<u8>)> {
    if key.len() < EncodedKey::RELATION_PREFIX_LEN + EncodedKey::BITEMPORAL_TAIL_LEN {
        bail!("bitemporal scan over a key too short to carry its two time slots");
    }
    let valid_off = key.len() - EncodedKey::BITEMPORAL_TAIL_LEN;
    let sys_off = key.len() - EncodedKey::VALIDITY_TAIL_LEN;
    let (valid_val, rest) = DataValue::decode_from_key(&key[valid_off..sys_off])?;
    let DataValue::Validity(valid) = valid_val else {
        bail!("bitemporal scan over a key without a valid-time slot");
    };
    if !rest.is_empty() {
        bail!("bitemporal scan over a key with trailing bytes inside its valid-time slot");
    }
    let (sys_val, rest) = DataValue::decode_from_key(&key[sys_off..])?;
    let DataValue::Validity(sys) = sys_val else {
        bail!("bitemporal scan over a key without a system-time slot");
    };
    if !rest.is_empty() {
        bail!("bitemporal scan over a key with trailing bytes after its system-time slot");
    }
    if !valid.is_assert.0 || !sys.is_assert.0 {
        bail!(
            "bitemporal scan over a key with a retract flag in a time slot \
             (polarity lives in the value; stored slot flags are pinned)"
        );
    }

    // Bounds live in the claimed-bytes domain, exactly as in the
    // single-axis kernel: only the tails were proven above, and blessing
    // the prefix into `EncodedKey` would launder unproven bytes into a
    // type whose possession means provenance.
    let splice_both = |v: Validity, s: Validity| -> Vec<u8> {
        let mut nxt = Vec::with_capacity(key.len());
        nxt.extend_from_slice(&key[..valid_off]);
        append_canonical(&mut nxt, &DataValue::Validity(v));
        append_canonical(&mut nxt, &DataValue::Validity(s));
        nxt
    };
    let splice_sys = |s: Validity| -> Vec<u8> {
        let mut nxt = Vec::with_capacity(key.len());
        nxt.extend_from_slice(&key[..sys_off]);
        append_canonical(&mut nxt, &DataValue::Validity(s));
        nxt
    };

    if valid.timestamp < as_of.valid {
        // Instant newer than the valid coordinate (`Reverse` order:
        // smaller means later). Seek to the newest instant at or before
        // `valid_at`, landing directly at the newest system version at or
        // before `sys_at` inside whatever instant the seek finds.
        return Ok((
            None,
            splice_both(
                Validity {
                    timestamp: as_of.valid,
                    is_assert: Reverse(true),
                },
                Validity {
                    timestamp: as_of.sys,
                    is_assert: Reverse(true),
                },
            ),
        ));
    }
    if sys.timestamp < as_of.sys {
        // Right instant, but this system version postdates the system
        // coordinate: seek to the newest version at or before `sys_at`
        // within the SAME instant.
        return Ok((
            None,
            splice_sys(Validity {
                timestamp: as_of.sys,
                is_assert: Reverse(true),
            }),
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
            Ok((None, splice_sys(TERMINAL_VALIDITY)))
        }
        ClaimPolarity::Retract => {
            // The fact is retracted from this instant on: absent at the
            // coordinates. Skip every older version of the fact.
            Ok((None, splice_both(TERMINAL_VALIDITY, TERMINAL_VALIDITY)))
        }
        ClaimPolarity::Assert => {
            // A hit. Emit, then skip every older version of the fact.
            let decoded = decode_tuple_from_key(key, size_hint.unwrap_or(DEFAULT_SIZE_HINT))?;
            Ok((
                Some(decoded),
                splice_both(TERMINAL_VALIDITY, TERMINAL_VALIDITY),
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
    if key.len() < EncodedKey::RELATION_PREFIX_LEN + EncodedKey::BITEMPORAL_TAIL_LEN {
        bail!("bitemporal key too short to carry its two time slots");
    }
    let sys_off = key.len() - EncodedKey::VALIDITY_TAIL_LEN;
    let (sys_val, rest) = DataValue::decode_from_key(&key[sys_off..])?;
    let DataValue::Validity(sys) = sys_val else {
        bail!("bitemporal key without a system-time slot");
    };
    if !rest.is_empty() {
        bail!("bitemporal key with trailing bytes after its system-time slot");
    }
    if !sys.is_assert.0 {
        bail!(
            "bitemporal key with a retract flag in its system-time slot \
             (polarity lives in the value; stored slot flags are pinned)"
        );
    }
    Ok(sys.timestamp)
}

pub fn extend_tuple_from_bitemporal_v(key: &mut Tuple, val: &[u8]) -> Result<()> {
    let Some(payload) = val.get(BITEMPORAL_VALUE_HEADER_LEN..) else {
        bail!("corrupt bitemporal value: shorter than its header and polarity byte");
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

    use crate::data::value::ValidityTs;
    use crate::data::value::{RelationId, TupleT};
    use std::collections::BTreeMap;

    fn vts(t: i64) -> ValidityTs {
        ValidityTs::from_raw(t)
    }

    fn zero_as_of() -> AsOf {
        AsOf {
            sys: vts(0),
            valid: vts(0),
        }
    }

    fn slot(t: i64) -> Validity {
        // Stored slot flags are pinned to assert; polarity lives in the value.
        Validity {
            timestamp: vts(t),
            is_assert: Reverse(true),
        }
    }

    fn bikey(fact: i64, valid_ts: i64, sys_ts: i64) -> Vec<u8> {
        [
            DataValue::from(fact),
            DataValue::Validity(slot(valid_ts)),
            DataValue::Validity(slot(sys_ts)),
        ]
        .encode_as_key(RelationId::new(7).expect("below cap"))
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
            let (ret, nxt) = check_key_for_bitemporal(
                k,
                *polarity,
                AsOf {
                    sys: vts(sys_at),
                    valid: vts(valid_at),
                },
                None,
            )?;
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

    fn facts_of(tuples: &[Tuple]) -> Vec<i64> {
        tuples
            .iter()
            .map(|t| match &t[0] {
                DataValue::Num(n) => n.as_int().expect("int-domain column"),
                other => panic!("non-integer fact column: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn bitemporal_tail_orders_valid_outer_system_inner() {
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
    }

    #[test]
    fn bitemporal_skip_scan_matches_oracle() {
        let mut state: u64 = 0x5EED_B17E_44C0_FFEE;
        let mut next = move |m: usize| -> usize {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as usize) % m
        };
        for _case in 0..2000 {
            let n_rows = 1 + next(10);
            let mut rows: Vec<(i64, i64, i64, ClaimPolarity)> = vec![];
            for _ in 0..n_rows {
                rows.push((
                    next(3) as i64,
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
                    let got = facts_of(&skip_walk(&store, sys_at, valid_at).unwrap());
                    let want = oracle(&rows, sys_at, valid_at);
                    assert_eq!(
                        got, want,
                        "divergence at sys_at={sys_at} valid_at={valid_at} rows={rows:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn polarity_flip_at_same_instant_is_governed_by_newest_system_version() {
        let store: BTreeMap<Vec<u8>, ClaimPolarity> = [
            (bikey(1, 10, 5), ClaimPolarity::Assert),
            (bikey(1, 10, 20), ClaimPolarity::Retract),
        ]
        .into();
        // Before the correction was recorded: the assert governs.
        assert_eq!(facts_of(&skip_walk(&store, 15, 15).unwrap()), vec![1]);
        // After: the retract governs — the fact is absent.
        assert_eq!(
            facts_of(&skip_walk(&store, 25, 15).unwrap()),
            Vec::<i64>::new()
        );
        // An erase instead of a retract falls through to... nothing older:
        // also absent, but by fall-through, not settlement.
        let store: BTreeMap<Vec<u8>, ClaimPolarity> = [
            (bikey(1, 10, 5), ClaimPolarity::Assert),
            (bikey(1, 10, 20), ClaimPolarity::Erase),
            (bikey(1, 0, 1), ClaimPolarity::Assert),
        ]
        .into();
        // Erased at 10, so the older instant 0 shows through.
        assert_eq!(facts_of(&skip_walk(&store, 25, 15).unwrap()), vec![1]);
        // A RETRACT at 10 would instead settle the fact absent.
        let store: BTreeMap<Vec<u8>, ClaimPolarity> = [
            (bikey(1, 10, 5), ClaimPolarity::Assert),
            (bikey(1, 10, 20), ClaimPolarity::Retract),
            (bikey(1, 0, 1), ClaimPolarity::Assert),
        ]
        .into();
        assert_eq!(
            facts_of(&skip_walk(&store, 25, 15).unwrap()),
            Vec::<i64>::new()
        );
    }

    #[test]
    fn corrupt_bitemporal_keys_refuse_and_never_panic() {
        assert!(
            check_key_for_bitemporal(&[0u8; 8], ClaimPolarity::Assert, zero_as_of(), None).is_err()
        );
        let flagged = [
            DataValue::from(1i64),
            DataValue::Validity(Validity {
                timestamp: vts(10),
                is_assert: Reverse(false),
            }),
            DataValue::Validity(slot(5)),
        ]
        .encode_as_key(RelationId::new(7).expect("below cap"))
        .as_bytes()
        .to_vec();
        assert!(
            check_key_for_bitemporal(&flagged, ClaimPolarity::Assert, zero_as_of(), None).is_err(),
            "retract flag in a stored valid slot must refuse"
        );
        let terminal = [
            DataValue::from(1i64),
            DataValue::Validity(TERMINAL_VALIDITY),
            DataValue::Validity(TERMINAL_VALIDITY),
        ]
        .encode_as_key(RelationId::new(7).expect("below cap"))
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
        .encode_as_key(RelationId::new(7).expect("below cap"))
        .as_bytes()
        .to_vec();
        assert!(
            check_key_for_bitemporal(&ints, ClaimPolarity::Assert, zero_as_of(), None).is_err()
        );
        for len in 0..64usize {
            let garbage = vec![0xEEu8; len];
            let _ = check_key_for_bitemporal(&garbage, ClaimPolarity::Assert, zero_as_of(), None);
        }
    }

    #[test]
    fn bitemporal_value_polarity_round_trips_and_refuses_corruption() {
        for polarity in [
            ClaimPolarity::Assert,
            ClaimPolarity::Retract,
            ClaimPolarity::Erase,
        ] {
            let val = vec![polarity.encode()];
            assert_eq!(claim_polarity_of_value(&val).unwrap(), polarity);
            // A bare retract/erase value carries no payload and extends
            // nothing.
            let mut tup: Tuple = vec![DataValue::from(1i64)];
            extend_tuple_from_bitemporal_v(&mut tup, &val).unwrap();
            assert_eq!(tup.len(), 1);
        }
        // An assert row's non-key columns ride after the polarity byte.
        let mut val = Vec::new();
        val.push(ClaimPolarity::Assert.encode());
        let non_keys = vec![DataValue::from(42i64), DataValue::from(7i64)];
        for v in &non_keys {
            crate::data::value::append_canonical(&mut val, v);
        }
        let mut tup: Tuple = vec![DataValue::from(1i64)];
        extend_tuple_from_bitemporal_v(&mut tup, &val).unwrap();
        assert_eq!(
            tup,
            Tuple::from(vec![
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
        assert_eq!(
            claim_polarity_of_value(&bad).unwrap(),
            ClaimPolarity::Assert
        );
        assert!(extend_tuple_from_bitemporal_v(&mut Tuple::new(), &bad).is_err());
    }

    /// Cross-check against the reference oracle: `query::laws::resolve_relation`
    /// implements the same resolution kernel, mirrored across the sign
    /// boundary on both axes (valid and system coordinates spanning
    /// negative and positive). This test probes coordinates on both sides
    /// of every stored one, using `bikey` and `skip_walk` directly from
    /// this module (the exhaustive case with 2000 generated histories,
    /// vs. the small fixed fixture in `query/laws.rs` used as a fast
    /// sanity check).
    #[test]
    fn reverify_laws_resolve_mirrors_the_real_kernel_with_negative_timestamps() {
        use crate::query::laws;
        let mut state: u64 = 0xDEAD_BEEF_CAFE_F00D;
        let mut next = move |m: usize| -> usize {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as usize) % m
        };
        let valids = [-30i64, -10, -3, 0, 10, 20, 30];
        let syss = [-25i64, -5, 0, 5, 15, 25];
        for _case in 0..2000 {
            let n_rows = 1 + next(10);
            let mut rows: Vec<(i64, i64, i64, ClaimPolarity)> = vec![];
            for _ in 0..n_rows {
                rows.push((
                    next(3) as i64,
                    valids[next(valids.len())],
                    syss[next(syss.len())],
                    [
                        ClaimPolarity::Assert,
                        ClaimPolarity::Retract,
                        ClaimPolarity::Erase,
                    ][next(3)],
                ));
            }
            rows.sort_unstable_by_key(|r| (r.0, r.1, r.2));
            rows.dedup_by_key(|r| (r.0, r.1, r.2));
            let store: BTreeMap<Vec<u8>, ClaimPolarity> = rows
                .iter()
                .map(|(f, v, s, p)| (bikey(*f, *v, *s), *p))
                .collect();
            // `.expect`: every `v` here is drawn from the fixed `valids`
            // list above, never the reserved terminal tick
            // (`laws::Event`'s constructors refuse `valid == i64::MAX`).
            let history: Vec<laws::Event> = rows
                .iter()
                .map(|(f, v, s, p)| {
                    let key: Tuple = vec![DataValue::from(*f)];
                    match p {
                        ClaimPolarity::Assert => laws::Event::assert(key, Tuple::new(), *v, *s),
                        ClaimPolarity::Retract => laws::Event::retract(key, *v, *s),
                        ClaimPolarity::Erase => laws::Event::erase(key, *v, *s),
                    }
                    .expect("valid instant is drawn from a bounded fixture list, never the reserved terminal tick")
                })
                .collect();
            for sys_at in [-40i64, -25, -5, 0, 5, 15, 25, 40] {
                for valid_at in [-40i64, -30, -10, -3, 0, 10, 20, 30, 40] {
                    let got_real = facts_of(&skip_walk(&store, sys_at, valid_at).unwrap());
                    let got_laws: Vec<i64> = laws::resolve_relation(
                        &history,
                        laws::AsOf {
                            valid: valid_at,
                            sys: sys_at,
                        },
                    )
                    .into_iter()
                    .map(|t| match &t[0] {
                        DataValue::Num(n) => n.as_int().expect("int-domain column"),
                        other => panic!("non-integer fact column: {other:?}"),
                    })
                    .collect();
                    assert_eq!(
                        got_real, got_laws,
                        "sys_at={sys_at} valid_at={valid_at} rows={rows:?}"
                    );
                }
            }
        }
    }
}
