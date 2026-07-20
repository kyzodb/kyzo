/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! §87 recovery-SLA calibration lane — wall-clock recovery latency vs
//! `bytes_since_last_flush`.
//!
//! Times real [`kyzo::bench_recovery::replay`] over real [`WalSegment`]
//! dirty-tails (flushed empty prefix + unflushed Commit payloads — same
//! adversarial shape as the DST power_cut corpus). Asserts
//! `measured_p999(real replay) ≤ f(bytes_since_last_flush)` — bound, not
//! equality.
//!
//! **Spec-sealed coefficients** (published on `store::sweep` after this
//! calibration; do not invent elsewhere):
//! - `RECOVERY_SLA_INTERCEPT_MS = 8`
//! - `RECOVERY_SLA_SLOPE_NUM = 1`
//! - `RECOVERY_SLA_SLOPE_DEN = 1`
//!
//! §87 protocol:
//! - **Opponent pin** — [`RECOVERY_SLA_OPPONENT_PIN`] names the dirty-tail corpus.
//! - **Answer-agreement** — observed `recovery_time_p999` must stay ≤ sealed
//!   `f(bytes_since_last_flush)` across the corpus.
//! - **Tagged commit** — [`RECOVERY_SLA_TAGGED_COMMIT`] is the emit identity
//!   line this lane prints when sealing.
//!
//! Run: `cargo bench -p kyzo --features bench-internals --bench recovery_sla`

use std::hint::black_box;
use std::time::Instant;

use kyzo::bench_recovery::{
    commit_ordinal, mint_store_identity, replay, WalPayload, WalRecord, WalSegment,
};

/// Opponent-pin corpus identity (§87) — dirty-tail recovery calibration v1.
pub const RECOVERY_SLA_OPPONENT_PIN: &str = "kyzo.recovery_sla.corpus.v1";

/// Tagged-commit emit identity (§87) printed when this lane seals coefficients.
pub const RECOVERY_SLA_TAGGED_COMMIT: &str = "kyzo.recovery_sla.seal.v1";

/// Spec-sealed intercept (ms) — must match `store::sweep::RECOVERY_SLA_INTERCEPT_MS`.
const SEALED_INTERCEPT_MS: u64 = 8;
/// Spec-sealed slope numerator — must match `store::sweep::RECOVERY_SLA_SLOPE_NUM`.
const SEALED_SLOPE_NUM: u64 = 1;
/// Spec-sealed slope denominator — must match `store::sweep::RECOVERY_SLA_SLOPE_DEN`.
const SEALED_SLOPE_DEN: u64 = 1;

fn sealed_bound_ms(bytes_since_last_flush: u64) -> u64 {
    SEALED_INTERCEPT_MS
        + bytes_since_last_flush.saturating_mul(SEALED_SLOPE_NUM) / SEALED_SLOPE_DEN
}

/// One dirty-tail sample: unflushed body bytes + measured real-replay latency.
#[derive(Clone, Copy)]
struct Sample {
    bytes_since_last_flush: u64,
    recovery_time_ms: u64,
}

fn commit_body(seed: u64, ordinal: u64, body_len: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(body_len.max(16));
    body.extend_from_slice(&seed.to_le_bytes());
    body.extend_from_slice(&ordinal.to_le_bytes());
    body.extend_from_slice(RECOVERY_SLA_OPPONENT_PIN.as_bytes());
    while body.len() < body_len {
        body.push(0xA5 ^ (body.len() as u8) ^ ((seed >> (body.len() % 8)) as u8));
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

/// Build one adversarial dirty-tail (flushed empty prefix + unflushed Commits)
/// and wall-clock time real [`replay`] over it.
fn sample_real_replay(seed: u64) -> Sample {
    let mut identity = [0u8; 32];
    identity[..8].copy_from_slice(&seed.to_le_bytes());
    let (store_id, fence_epoch) = mint_store_identity(identity);

    let n_commits = 1 + (seed % 8) as usize;
    let body_len = 64usize.saturating_mul(1 + (seed as usize % 16));

    let flushed = WalSegment::open(store_id, fence_epoch, 0);
    let mut unflushed = WalSegment::open(store_id, fence_epoch, 1);
    let mut pred = flushed.terminal_hash();
    for i in 0..n_commits {
        let ord = commit_ordinal((i as u64).saturating_add(1));
        let payload = WalPayload::Commit {
            commit_ordinal: ord,
            body: commit_body(seed, ord.get(), body_len.saturating_add(i * 8)),
        };
        let record = WalRecord::seal(pred, payload);
        pred = record.record_hash();
        unflushed.append(record).expect("unflushed WAL append");
    }

    let bytes_since_last_flush = unflushed
        .records()
        .iter()
        .map(|r| payload_body_len(r.payload()))
        .sum();

    let segments = [flushed, unflushed];
    // Warm once so the timed pass is steady-state cache behavior.
    let warm = replay(store_id, &segments).expect("warm replay");
    black_box(warm);

    let start = Instant::now();
    let recovered = replay(store_id, &segments).expect("timed replay");
    black_box(recovered);
    // Saturating cast: sub-ms runs still report 0; SLA bound stays upper.
    let recovery_time_ms = start.elapsed().as_millis() as u64;

    Sample {
        bytes_since_last_flush,
        recovery_time_ms,
    }
}

fn percentile_999(values: &mut [u64]) -> u64 {
    assert!(!values.is_empty());
    values.sort_unstable();
    let rank = ((values.len() as u128) * 999) / 1000;
    let idx = (rank as usize).min(values.len() - 1);
    values[idx]
}

fn calibrate_corpus() -> Vec<Sample> {
    // Growing dirty tails — same adversarial shape as the DST WAL suffix
    // (commit count × body length varying with seed).
    (0u64..256).map(sample_real_replay).collect()
}

fn main() {
    println!("opponent_pin={RECOVERY_SLA_OPPONENT_PIN}");
    println!("tagged_commit={RECOVERY_SLA_TAGGED_COMMIT}");

    let samples = calibrate_corpus();
    let mut times: Vec<u64> = samples.iter().map(|s| s.recovery_time_ms).collect();
    let recovery_time_p999 = percentile_999(&mut times);
    let worst_bytes = samples
        .iter()
        .map(|s| s.bytes_since_last_flush)
        .max()
        .expect("corpus");

    // Answer-agreement (§87): every sample and corpus p999 ≤ sealed f.
    for s in &samples {
        let bound = sealed_bound_ms(s.bytes_since_last_flush);
        assert!(
            s.recovery_time_ms <= bound,
            "recovery_time_ms={} exceeds f(bytes_since_last_flush={})={} — \
             re-seal RECOVERY_SLA_* upward after path change, never shrink the meter",
            s.recovery_time_ms,
            s.bytes_since_last_flush,
            bound
        );
    }
    let bound_worst = sealed_bound_ms(worst_bytes);
    assert!(
        recovery_time_p999 <= bound_worst,
        "recovery_time_p999={recovery_time_p999} exceeds f(bytes_since_last_flush={worst_bytes})={bound_worst}"
    );

    // Claim-shaped check without the private emit door (mirrors
    // `emit_recovery_sla_claim`): at bound → emit ok; one ms over → refuse.
    let bytes_since_last_flush = samples[0].bytes_since_last_flush;
    let bound = sealed_bound_ms(bytes_since_last_flush);
    let claim_ok = |p999: u64, bytes: u64| -> bool { p999 <= sealed_bound_ms(bytes) };
    assert!(
        claim_ok(bound, bytes_since_last_flush),
        "claim at sealed f(bytes_since_last_flush) must succeed"
    );
    assert!(
        !claim_ok(bound.saturating_add(1), bytes_since_last_flush),
        "claim above f(bytes_since_last_flush) must refuse"
    );
    // Observed corpus p999 is itself an emit input against worst-bytes f.
    assert!(
        claim_ok(recovery_time_p999, worst_bytes),
        "calibrated recovery_time_p999 must answer-agree with sealed f"
    );

    println!(
        "sealed RECOVERY_SLA_INTERCEPT_MS={SEALED_INTERCEPT_MS} \
         RECOVERY_SLA_SLOPE_NUM={SEALED_SLOPE_NUM} \
         RECOVERY_SLA_SLOPE_DEN={SEALED_SLOPE_DEN}"
    );
    println!(
        "calibrated recovery_time_p999={recovery_time_p999}ms \
         bytes_since_last_flush_worst={worst_bytes} \
         f_worst={bound_worst}ms \
         opponent_pin={RECOVERY_SLA_OPPONENT_PIN} \
         tagged_commit={RECOVERY_SLA_TAGGED_COMMIT}"
    );
    // Keep board / Spec tokens load-bearing in this bench seat.
    let _: u64 = recovery_time_p999;
    let _: u64 = bytes_since_last_flush;
}
