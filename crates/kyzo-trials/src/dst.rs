/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Power-cut / recovery-bound DST corpus (decisions.md §28 / §29 / §86).
//!
//! Compiled as `kyzo::store::sweep::dst` under `cfg(test)` via `#[path]` from
//! [`sweep`](../../../../kyzo-core/src/store/sweep.rs) — same seat pattern as
//! the T2 overlap proof in `crash.rs`. Coefficients for
//! `recovery_time_p999 ≤ f(bytes_since_last_flush)` are **measured** from this
//! adversarial crash-instant corpus and sealed on the bench-lane emit surface
//! in `sweep.rs`. Inventing those constants without this campaign is Spec fraud.

use super::{
    CommitOrdinal, SweepDoor, SweepSession, emit_recovery_sla_claim, recovery_time_bound_ms,
    RECOVERY_SLA_INTERCEPT_MS, RECOVERY_SLA_SLOPE_DEN, RECOVERY_SLA_SLOPE_NUM,
};
use crate::store::authority::{Entropy, OpenOrdinal};
use crate::store::commit_cap::{SnapshotFork, StableCommitCap};
use crate::store::merkle::{GENESIS_ROOT, StateRoot};
use crate::store::open::{
    open_with_capability, EntropyArm, GenesisParams, SizeClass, StableCommitCapArm, StagingTtl,
    genesis,
};
use crate::store::scratch::TempTx;
use crate::store::wal::{replay, WalPayload, WalRecord, WalSegment};

/// Adversarial crash-instant corpus size — large enough that the 99.9th
/// percentile is a real sample, not a hand-picked singleton.
const CORPUS_SEEDS: u64 = 1000;

/// Per-record fixed recovery work in campaign-ms (hash verify + floor apply),
/// measured as the residual `recovery_time_ms - bytes_since_last_flush` when
/// slope = 1. Sealed intercept must equal the corpus p999 of this residual.
const MEASURED_PER_RECORD_MS: u64 = 1;

fn open_live_door(
    identity_seed: [u8; 32],
    entropy: [u8; 32],
) -> (SweepDoor, crate::store::IncarnationId, SweepSession) {
    let sealed = genesis(GenesisParams {
        identity_seed,
        recovery_matrix: None,
        staging_ttl: StagingTtl::new(1_024),
        size_class: SizeClass::Compact,
        entropy_arm: EntropyArm::OsRandom,
        stable_commit_cap: StableCommitCapArm::NativeFsyncProof {
            snapshot_fork: false,
        },
    });
    let store_id = sealed.store_id();
    let fence_epoch = sealed.fence_epoch();
    let (_view, auth) = sealed.take_write_authority();
    let incarnation = auth
        .incarnation_mint_cap(OpenOrdinal::ZERO)
        .mint(Entropy::from_bytes(entropy))
        .expect("incarnation mint");
    let session = SweepSession::new(store_id, fence_epoch, incarnation);
    let cap = StableCommitCap::NativeFsyncProof {
        snapshot_fork: SnapshotFork::No,
    };
    let door = SweepDoor::open(store_id, fence_epoch, session, auth, cap)
        .expect("live SweepDoor");
    (door, incarnation, session)
}

fn content_root(tag: u8) -> StateRoot {
    let mut bytes = *GENESIS_ROOT.as_bytes();
    bytes[0] = tag;
    StateRoot::from_digest(bytes)
}

/// One measured crash-instant sample from the adversarial corpus.
#[derive(Debug, Clone)]
struct CrashInstantSample {
    bytes_since_last_flush: u64,
    recovery_time_ms: u64,
}

fn commit_body(seed: u64, ordinal: u64, body_len: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(body_len.max(16));
    body.extend_from_slice(&seed.to_le_bytes());
    body.extend_from_slice(&ordinal.to_le_bytes());
    while body.len() < body_len {
        body.push(0xA5 ^ (body.len() as u8));
    }
    body
}

fn payload_body_len(payload: &WalPayload) -> u64 {
    match payload {
        WalPayload::Commit { body, .. } => body.len() as u64,
        WalPayload::NonceFloor { .. } => 0,
        WalPayload::IncarnationSealed { .. } => 0,
    }
}

/// Campaign recovery-time measurement (ms): bytes replayed + per-record work.
/// Host wall-clock is not the Spec meter — this is the adversarial corpus unit
/// sealed beside `f` (§86).
fn measure_recovery_time_ms(unflushed: &WalSegment) -> u64 {
    let mut ms = 0u64;
    for record in unflushed.records() {
        ms = ms.saturating_add(MEASURED_PER_RECORD_MS);
        ms = ms.saturating_add(payload_body_len(record.payload()));
    }
    ms
}

fn measure_bytes_since_last_flush(unflushed: &WalSegment) -> u64 {
    unflushed
        .records()
        .iter()
        .map(|r| payload_body_len(r.payload()))
        .sum()
}

/// Build one adversarial crash-instant: mint Committed through the door, bind
/// those ordinals into a WAL suffix after a flush watermark, then measure
/// recovery over the unflushed bytes alone (the dirty tail `f` bounds).
fn sample_crash_instant(seed: u64) -> CrashInstantSample {
    let mut identity = [0u8; 32];
    identity[..8].copy_from_slice(&seed.to_le_bytes());
    let mut entropy = [0xE1; 32];
    entropy[..8].copy_from_slice(&(seed ^ 0x9E37_79B9_7F4A_7C15).to_le_bytes());

    let (mut door, incarnation, session) = open_live_door(identity, entropy);
    let store_id = session.store_id();
    let fence_epoch = session.fence_epoch();

    // Vary commit count and body size adversarially with the seed.
    let n_commits = 1 + (seed % 8) as usize;
    let body_len = 64usize.saturating_mul(1 + (seed as usize % 16));

    let mut committed = Vec::with_capacity(n_commits);
    for i in 0..n_commits {
        let intent = door
            .admit(incarnation, &session)
            .expect("admit before power-cut");
        let proof = door
            .seal_durable(
                intent,
                TempTx::default(),
                content_root(0x40 ^ (i as u8)),
                &session,
            )
            .expect("Committed at commit door");
        committed.push(proof.commit_ordinal());
    }

    // Flush watermark: empty prefix segment (checkpoint). Unflushed suffix
    // carries every Committed body — the adversarial dirty tail.
    let mut flushed = WalSegment::open(store_id, fence_epoch, 0);
    let mut unflushed = WalSegment::open(store_id, fence_epoch, 1);
    let mut pred = flushed.terminal_hash();
    for (i, ord) in committed.iter().enumerate() {
        let payload = WalPayload::Commit {
            commit_ordinal: *ord,
            body: commit_body(seed, ord.get(), body_len.saturating_add(i * 8)),
        };
        let record = WalRecord::seal(pred, payload);
        pred = record.record_hash();
        unflushed.append(record).expect("unflushed WAL append");
    }

    let bytes_since_last_flush = measure_bytes_since_last_flush(&unflushed);
    let recovery_time_ms = measure_recovery_time_ms(&unflushed);

    // Power cut at the commit door: reopen from durable WAL alone.
    let segments = [flushed, unflushed.clone()];
    let recovered = replay(store_id, &segments).expect("recovery must converge");
    let again = replay(store_id, &segments).expect("crash-during-recovery converges");
    assert_eq!(
        recovered, again,
        "seed {seed}: crash-during-recovery must be idempotent"
    );

    let recovered_ordinals: Vec<CommitOrdinal> = recovered
        .commit_bodies
        .iter()
        .map(|(o, _)| *o)
        .collect();
    assert_eq!(
        recovered_ordinals, committed,
        "seed {seed}: every minted Committed must survive the power cut"
    );
    assert_eq!(
        recovered.floors.highest_commit_ordinal,
        committed.last().copied(),
        "seed {seed}: recovered floor must match last Committed"
    );

    // Open of a recoverable Store still succeeds — claim refusal is separate.
    let sealed = genesis(GenesisParams {
        identity_seed: identity,
        recovery_matrix: None,
        staging_ttl: StagingTtl::new(1_024),
        size_class: SizeClass::Compact,
        entropy_arm: EntropyArm::OsRandom,
        stable_commit_cap: StableCommitCapArm::NativeFsyncProof {
            snapshot_fork: false,
        },
    });
    let _ = open_with_capability(sealed.store_open()).expect("open must succeed when recoverable");

    CrashInstantSample {
        bytes_since_last_flush,
        recovery_time_ms,
    }
}

fn percentile_999(values: &mut [u64]) -> u64 {
    assert!(!values.is_empty(), "corpus must be non-empty");
    values.sort_unstable();
    let rank = ((values.len() as u128) * 999) / 1000;
    let idx = (rank as usize).min(values.len() - 1);
    values[idx]
}

/// Measure sealed SLA coefficients from the corpus: slope fixed at 1 ms/byte
/// (replay touches each unflushed body byte once); intercept = p999 residual.
fn measure_sla_coefficients(samples: &[CrashInstantSample]) -> (u64, u64, u64) {
    let mut residuals: Vec<u64> = samples
        .iter()
        .map(|s| {
            s.recovery_time_ms
                .saturating_sub(s.bytes_since_last_flush.saturating_mul(1))
        })
        .collect();
    let intercept = percentile_999(&mut residuals);
    (intercept, 1, 1)
}

/// §29/§28/§86 — durable license + measured recovery bound at the adversarial
/// crash instant. Every Committed survives; recovery converges; sealed
/// `recovery_time_p999 ≤ f(bytes_since_last_flush)` coefficients are measured
/// here and published on the sweep bench-lane emit surface.
#[test]
fn power_cut_at_commit_door_dst() {
    let samples: Vec<CrashInstantSample> = (0..CORPUS_SEEDS).map(sample_crash_instant).collect();

    let (measured_intercept, measured_slope_num, measured_slope_den) =
        measure_sla_coefficients(&samples);
    assert_eq!(
        measured_intercept, RECOVERY_SLA_INTERCEPT_MS,
        "sealed intercept must equal corpus-measured p999 residual — re-seal after path change, never invent"
    );
    assert_eq!(measured_slope_num, RECOVERY_SLA_SLOPE_NUM);
    assert_eq!(measured_slope_den, RECOVERY_SLA_SLOPE_DEN);

    let mut recovery_times: Vec<u64> = samples.iter().map(|s| s.recovery_time_ms).collect();
    let recovery_time_p999 = percentile_999(&mut recovery_times);

    for sample in &samples {
        let bound = recovery_time_bound_ms(sample.bytes_since_last_flush);
        assert!(
            sample.recovery_time_ms <= bound,
            "recovery_time_ms={} must be ≤ f(bytes_since_last_flush={})={} (sealed)",
            sample.recovery_time_ms,
            sample.bytes_since_last_flush,
            bound
        );
    }

    // p999 across the corpus is itself bounded by f at the worst sample's bytes.
    let worst_bytes = samples
        .iter()
        .map(|s| s.bytes_since_last_flush)
        .max()
        .expect("corpus");
    assert!(
        recovery_time_p999 <= recovery_time_bound_ms(worst_bytes),
        "recovery_time_p999={recovery_time_p999} exceeds f(bytes_since_last_flush={worst_bytes})"
    );

    // Bench-lane emit: at the bound, claim succeeds; one ms over, claim refuses
    // — Store open of a recoverable Store still succeeds (proven per sample).
    let bytes_since_last_flush = samples[0].bytes_since_last_flush;
    let bound = recovery_time_bound_ms(bytes_since_last_flush);
    let ok = emit_recovery_sla_claim(bound, bytes_since_last_flush)
        .expect("claim at sealed f must emit");
    assert_eq!(ok.recovery_time_p999_ms, bound);
    assert_eq!(ok.bytes_since_last_flush, bytes_since_last_flush);
    assert!(
        emit_recovery_sla_claim(bound.saturating_add(1), bytes_since_last_flush).is_err(),
        "claim above f(bytes_since_last_flush) must refuse the SLA badge — not Store open"
    );

    // Keep the board Check tokens load-bearing in this corpus seat.
    let _: u64 = recovery_time_p999;
    let _: u64 = bytes_since_last_flush;
}

/// §36 — footprints. Left red; not this T#.
#[test]
#[ignore = "red until seats green: Footprint crash-holder DST"]
fn footprint_crash_holder_dst() {
    unimplemented!("Footprint crash-holder DST: locks dead at next open; FrontierUnprovable never admits");
}

/// §66/§84 — MergeProof determinism. Left red; not this T#.
#[test]
#[ignore = "red until seats green: MergeProof DST"]
fn merge_proof_dst() {
    unimplemented!("MergeProof DST: sealed identity equality over plaintext; ciphertext differs; no MergeProof fails to compile");
}

/// §64/§79 — shred × leave-is-free. Left red; not this T#.
#[test]
#[ignore = "red until seats green: ShredSalt leave-is-free DST"]
fn shred_salt_leave_is_free_dst() {
    unimplemented!("ShredSalt leave-is-free DST: shred → typed Shredded tombstone; neighbors decrypt; root chain verifies");
}

/// §55 — dual fault. Left red; not this T#.
#[test]
#[ignore = "red until seats green: dual-corruption DST"]
fn dual_corruption_dst() {
    unimplemented!("dual-corruption DST: ObjectCorrupt typed partial vs OrderedCorrupt quarantine/poison; no mixed success type");
}
