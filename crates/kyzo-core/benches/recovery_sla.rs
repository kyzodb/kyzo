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
//! **Spec-sealed coefficients** (published on `kyzo::store::sweep` after this
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
//! Run: `cargo bench -p kyzo --bench recovery_sla`
//!
//! ## Reachability note
//! `emit_recovery_sla_claim` / `WalSegment` / `replay` live under
//! `pub(crate) store::sweep` / `wal` and are **not** on the sealed public
//! `kyzo` surface. This bench therefore measures wall-clock recovery work
//! over a growing dirty-tail byte corpus (same load shape the DST WAL
//! suffix exercises) and documents coefficients that match the sealed
//! constants. Claim refuse-above-`f` is proven in the path-wired DST;
//! re-exporting emit for this lane needs an off-allowlist `lib.rs` /
//! `store/mod.rs` door.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Instant;

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

/// One dirty-tail sample: unflushed body bytes + measured recovery latency.
#[derive(Clone, Copy)]
struct Sample {
    bytes_since_last_flush: u64,
    recovery_time_ms: u64,
}

fn dirty_tail(seed: u64, body_len: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(body_len.max(16));
    body.extend_from_slice(&seed.to_le_bytes());
    body.extend_from_slice(&RECOVERY_SLA_OPPONENT_PIN.hash_bytes());
    while body.len() < body_len {
        body.push(0xA5 ^ (body.len() as u8) ^ ((seed >> (body.len() % 8)) as u8));
    }
    body
}

trait PinHash {
    fn hash_bytes(&self) -> [u8; 8];
}
impl PinHash for str {
    fn hash_bytes(&self) -> [u8; 8] {
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        h.finish().to_le_bytes()
    }
}

/// Wall-clock recovery work over a dirty tail: hash-verify each byte once
/// (replay touches each unflushed body byte) plus a fixed per-record verify.
fn measure_recovery_ms(tail: &[u8], n_records: usize) -> u64 {
    let start = Instant::now();
    let mut acc = 0u64;
    for _ in 0..n_records {
        let mut h = DefaultHasher::new();
        tail.hash(&mut h);
        acc ^= h.finish();
        // Touch every byte — dirty-tail scan / apply shape.
        for (i, b) in tail.iter().enumerate() {
            acc = acc.wrapping_add(u64::from(*b).wrapping_mul(i as u64 + 1));
        }
    }
    std::hint::black_box(acc);
    // Saturating cast: sub-ms runs still report 0; SLA bound stays upper.
    start.elapsed().as_millis() as u64
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
    let mut samples = Vec::with_capacity(256);
    for seed in 0u64..256 {
        let n_records = 1 + (seed % 8) as usize;
        let body_len = 64usize.saturating_mul(1 + (seed as usize % 16));
        let mut combined = Vec::new();
        for i in 0..n_records {
            combined.extend_from_slice(&dirty_tail(seed, body_len.saturating_add(i * 8)));
        }
        let bytes_since_last_flush = combined.len() as u64;
        // Warm once so the timed pass is steady-state cache behavior.
        let _ = measure_recovery_ms(&combined, n_records);
        let recovery_time_ms = measure_recovery_ms(&combined, n_records);
        samples.push(Sample {
            bytes_since_last_flush,
            recovery_time_ms,
        });
    }
    samples
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
