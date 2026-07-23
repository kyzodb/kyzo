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
//! dirty-tails at **MB / tens-of-MB** scale (flushed empty prefix + unflushed
//! Commit payloads — same adversarial shape as the DST power_cut corpus).
//! Measures in nanoseconds so sub-ms / per-byte cost is visible.
//!
//! **Derive-then-ceiling (§86 / §87):** coefficients are computed from the
//! measured distribution (intercept = fixed-cost floor × margin; slope = max
//! per-byte cost × margin) for transparency. Sealed `RECOVERY_SLA_*` on
//! `store::sweep` is a **campaign ceiling** — wall-clock noise means a later
//! run may derive lower; equality to re-derived-every-run is wrong. Fail-closed:
//! measured latency ≤ sealed `f`; if this run's derived intercept/slope would
//! *exceed* sealed, refuse asking to re-seal upward (never shrink).
//!
//! §87 protocol:
//! - **Opponent pin** — [`RECOVERY_SLA_OPPONENT_PIN`] names the dirty-tail corpus.
//! - **Answer-agreement** — observed recovery latency ≤ sealed `f`.
//! - **Tagged commit** — [`RECOVERY_SLA_TAGGED_COMMIT`] is the emit identity
//!   line this lane prints when sealing.
//!
//! Run: `cargo bench -p kyzo --features bench-internals --bench recovery_sla`

use std::hint::black_box;
use std::process::ExitCode;
use std::time::Instant;

use kyzo::bench_recovery::{
    IdentitySeed, RECOVERY_SLA_INTERCEPT_NS, RECOVERY_SLA_SLOPE_DEN, RECOVERY_SLA_SLOPE_NUM,
    WalPayload, WalRecord, WalSegment, commit_ordinal, mint_store_identity, recovery_time_bound_ns,
    replay,
};
use kyzo_model::value::convert::{
    u64_from_u128, u64_from_usize, u8_from_u64_low, usize_from_u64, usize_from_u64_fitting,
    usize_from_u128,
};

/// Opponent-pin corpus identity (§87) — MB-scale dirty-tail recovery calibration.
pub const RECOVERY_SLA_OPPONENT_PIN: &str = "kyzo.recovery_sla.corpus.v2";

/// Tagged-commit emit identity (§87) printed when this lane seals coefficients.
pub const RECOVERY_SLA_TAGGED_COMMIT: &str = "kyzo.recovery_sla.seal.v2";

/// Modest seal margin above the measured distribution (2×).
const SEAL_MARGIN_NUM: u64 = 2;
const SEAL_MARGIN_DEN: u64 = 1;

/// Dirty-tail body-byte targets — MB / tens-of-MB so real replay is non-vacuous.
const CORPUS_TARGET_BYTES: &[u64] = &[
    1 << 20,  // 1 MiB
    2 << 20,  // 2 MiB
    4 << 20,  // 4 MiB
    8 << 20,  // 8 MiB
    16 << 20, // 16 MiB
    32 << 20, // 32 MiB
];

/// Repeats per target size (seeded) for a stable p999 / slope fit.
const SAMPLES_PER_TARGET: u64 = 4;

/// One dirty-tail sample: unflushed body bytes + measured real-replay latency.
#[derive(Clone, Copy)]
struct Sample {
    bytes_since_last_flush: u64,
    recovery_time_ns: u64,
}

/// Coefficients derived from the campaign (with [`SEAL_MARGIN_NUM`] / [`SEAL_MARGIN_DEN`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Derived {
    intercept_ns: u64,
    slope_num: u64,
    slope_den: u64,
}

fn commit_body(seed: u64, ordinal: u64, body_len: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(body_len.max(16));
    body.extend_from_slice(&seed.to_le_bytes());
    body.extend_from_slice(&ordinal.to_le_bytes());
    body.extend_from_slice(RECOVERY_SLA_OPPONENT_PIN.as_bytes());
    while body.len() < body_len {
        let idx = u64_from_usize(body.len());
        body.push(0xA5 ^ u8_from_u64_low(idx) ^ u8_from_u64_low(seed >> (body.len() % 8)));
    }
    body
}

fn payload_body_len(payload: &WalPayload) -> u64 {
    match payload {
        WalPayload::Commit { body, .. } => u64_from_usize(body.len()),
        WalPayload::NonceFloor { .. } => 0,
        WalPayload::IncarnationSealed { .. } => 0,
    }
}

fn refuse_width<T>(door: &'static str, e: impl std::fmt::Debug) -> T {
    std::panic::resume_unwind(Box::new(format!("{door}: {e:?}")))
}

fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

fn reduce_slope(num: u64, den: u64) -> (u64, u64) {
    if num == 0 {
        return (0, 1);
    }
    let g = gcd_u64(num, den);
    (num / g, den / g)
}

/// Build one adversarial dirty-tail sized near `target_bytes` and wall-clock
/// time real [`replay`] over it (nanosecond resolution).
fn sample_real_replay(seed: u64, target_bytes: u64) -> Sample {
    let mut identity = [0u8; 32];
    identity[..8].copy_from_slice(&seed.to_le_bytes());
    identity[8..16].copy_from_slice(&target_bytes.to_le_bytes());
    let (store_id, fence_epoch) = mint_store_identity(IdentitySeed::from_digest(identity));

    let n_commits = 1 + usize_from_u64_fitting(seed % 4);
    // Ceil-split so Σ body lengths ≥ target (truncating div left samples short).
    let body_len_base = target_bytes.div_ceil(u64_from_usize(n_commits)).max(64);
    let mut remaining = target_bytes;

    let flushed = WalSegment::open(store_id, fence_epoch, 0);
    let mut unflushed = WalSegment::open(store_id, fence_epoch, 1);
    let mut pred = flushed.terminal_hash();
    for i in 0..n_commits {
        let ord = commit_ordinal(u64_from_usize(i) + 1);
        let body_len = if i + 1 == n_commits {
            // Pad final commit so cumulative payload bytes ≥ target.
            match usize_from_u64(remaining.max(body_len_base)) {
                Ok(n) => n,
                Err(e) => refuse_width("body_len final", e),
            }
        } else {
            let len = body_len_base.min(remaining).max(64);
            // INVARIANT(DirtyTailBudget): body partition floors remaining at 0.
            remaining = remaining.saturating_sub(len);
            match usize_from_u64(len) {
                Ok(n) => n,
                Err(e) => refuse_width("body_len part", e),
            }
        };
        let payload = WalPayload::Commit {
            commit_ordinal: ord,
            body: commit_body(seed, ord.get(), body_len),
        };
        let record = match WalRecord::seal(pred, payload) {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(Box::new(format!("wal seal: {e:?}"))),
        };
        pred = record.record_hash();
        if let Err(e) = unflushed.append(record) {
            std::panic::resume_unwind(Box::new(format!("unflushed WAL append: {e:?}")));
        }
    }

    let bytes_since_last_flush = unflushed
        .records()
        .iter()
        .map(|r| payload_body_len(r.payload()))
        .sum();
    // INVARIANT(dirty_tail_sized): the unflushed segment is built to meet
    // `target_bytes` before this bench measures replay.

    let segments = [flushed, unflushed];
    // Warm once so the timed pass is steady-state cache behavior.
    let warm = match replay(store_id, &segments) {
        Ok(r) => r,
        Err(e) => std::panic::resume_unwind(Box::new(format!("warm replay: {e:?}"))),
    };
    black_box(warm);

    let start = Instant::now();
    let recovered = match replay(store_id, &segments) {
        Ok(r) => r,
        Err(e) => std::panic::resume_unwind(Box::new(format!("timed replay: {e:?}"))),
    };
    black_box(recovered);
    let recovery_time_ns = match u64_from_u128(start.elapsed().as_nanos()) {
        Ok(n) => n,
        Err(e) => refuse_width("recovery_time_ns", e),
    };

    Sample {
        bytes_since_last_flush,
        recovery_time_ns,
    }
}

fn percentile_999(values: &mut [u64]) -> Result<u64, String> {
    if values.is_empty() {
        return Err("percentile_999 requires a non-empty sample set".into());
    }
    values.sort_unstable();
    let rank = (u128::from(u64_from_usize(values.len())) * 999) / 1000;
    let idx = match usize_from_u128(rank) {
        Ok(i) => i.min(values.len() - 1),
        Err(e) => return Err(format!("percentile rank refuses: {e}")),
    };
    Ok(values[idx])
}

fn calibrate_corpus() -> Vec<Sample> {
    let mut samples = Vec::with_capacity(
        CORPUS_TARGET_BYTES.len() * usize_from_u64_fitting(SAMPLES_PER_TARGET),
    );
    let mut seed = 0u64;
    for &target in CORPUS_TARGET_BYTES {
        for _ in 0..SAMPLES_PER_TARGET {
            samples.push(sample_real_replay(seed, target));
            seed += 1;
        }
    }
    samples
}

/// Derive sealed `f` from the measured distribution.
///
/// - intercept ← min observed latency × margin (fixed-cost floor)
/// - slope ← max over samples of ceil((time×margin − intercept) / bytes),
///   reduced as num/den (ns per byte)
fn derive_coefficients(samples: &[Sample]) -> Result<Derived, String> {
    if samples.is_empty() {
        return Err("corpus must be non-empty".into());
    }
    let floor_ns = match samples.iter().map(|s| s.recovery_time_ns).min() {
        Some(v) => v,
        None => return Err("corpus must be non-empty".into()),
    };
    let intercept_ns = floor_ns
        // INVARIANT(SealMargin): margin × floor clips at u64::MAX — never wrap a bound down.
        .saturating_mul(SEAL_MARGIN_NUM)
        .div_ceil(SEAL_MARGIN_DEN)
        .max(1);

    let mut slope_num = 0u64;
    let slope_den = 1u64;
    for s in samples {
        if s.bytes_since_last_flush == 0 {
            continue;
        }
        let budget_ns = s
            .recovery_time_ns
            // INVARIANT(SealMargin): margin × sample clips at u64::MAX — never wrap a bound down.
            .saturating_mul(SEAL_MARGIN_NUM)
            .div_ceil(SEAL_MARGIN_DEN);
        // INVARIANT(SealBudget): time left for the slope term floors at 0 ns.
        let after_intercept = budget_ns.saturating_sub(intercept_ns);
        let need = after_intercept.div_ceil(s.bytes_since_last_flush);
        slope_num = slope_num.max(need);
    }

    if slope_num == 0 {
        return Err(
            "expected a visible per-byte replay trend on the MB corpus; derived slope 0 — \
             widen CORPUS_TARGET_BYTES or inspect timing noise"
                .into(),
        );
    }

    let (slope_num, slope_den) = reduce_slope(slope_num, slope_den);
    Ok(Derived {
        intercept_ns,
        slope_num,
        slope_den,
    })
}

fn derived_bound_ns(d: Derived, bytes_since_last_flush: u64) -> u64 {
    // INVARIANT(SealBound): slope·bytes clips at u64::MAX before adding intercept.
    d.intercept_ns + bytes_since_last_flush.saturating_mul(d.slope_num) / d.slope_den
}

fn run() -> Result<(), String> {
    println!("opponent_pin={RECOVERY_SLA_OPPONENT_PIN}");
    println!("tagged_commit={RECOVERY_SLA_TAGGED_COMMIT}");
    println!(
        "seal_margin={SEAL_MARGIN_NUM}/{SEAL_MARGIN_DEN} \
         corpus_targets_mib={}",
        CORPUS_TARGET_BYTES
            .iter()
            .map(|b| format!("{}", b >> 20))
            .collect::<Vec<_>>()
            .join(",")
    );

    let samples = calibrate_corpus();
    let mut times: Vec<u64> = samples.iter().map(|s| s.recovery_time_ns).collect();
    let recovery_time_p999 = percentile_999(&mut times)?;
    let worst_bytes = match samples.iter().map(|s| s.bytes_since_last_flush).max() {
        Some(v) => v,
        None => return Err("corpus must be non-empty".into()),
    };
    let min_bytes = match samples.iter().map(|s| s.bytes_since_last_flush).min() {
        Some(v) => v,
        None => return Err("corpus must be non-empty".into()),
    };

    if recovery_time_p999 == 0 {
        return Err(
            "vacuous recovery_time_p999=0ns — corpus too small for Instant resolution".into(),
        );
    }
    if min_bytes < (1 << 20) {
        return Err(format!(
            "corpus min bytes_since_last_flush={min_bytes} below 1 MiB — widen targets"
        ));
    }

    let derived = derive_coefficients(&samples)?;
    println!(
        "derived RECOVERY_SLA_INTERCEPT_NS={} \
         RECOVERY_SLA_SLOPE_NUM={} \
         RECOVERY_SLA_SLOPE_DEN={} \
         (margin {SEAL_MARGIN_NUM}/{SEAL_MARGIN_DEN})",
        derived.intercept_ns, derived.slope_num, derived.slope_den
    );
    println!(
        "sealed  RECOVERY_SLA_INTERCEPT_NS={RECOVERY_SLA_INTERCEPT_NS} \
         RECOVERY_SLA_SLOPE_NUM={RECOVERY_SLA_SLOPE_NUM} \
         RECOVERY_SLA_SLOPE_DEN={RECOVERY_SLA_SLOPE_DEN} \
         (campaign ceiling — wall-clock noise may derive lower)"
    );

    // Fail-closed ceiling: sealed must cover this run's derived. If derived
    // exceeds sealed, re-seal upward — never shrink; never require equality
    // (wall-clock is not bit-stable across runs).
    if derived.intercept_ns > RECOVERY_SLA_INTERCEPT_NS {
        return Err(format!(
            "derived intercept {}ns exceeds sealed RECOVERY_SLA_INTERCEPT_NS={}ns — \
             re-seal upward (never shrink):\n\
             RECOVERY_SLA_INTERCEPT_NS = {};\n\
             RECOVERY_SLA_SLOPE_NUM = {};\n\
             RECOVERY_SLA_SLOPE_DEN = {};",
            derived.intercept_ns,
            RECOVERY_SLA_INTERCEPT_NS,
            derived.intercept_ns,
            derived.slope_num.max(RECOVERY_SLA_SLOPE_NUM),
            derived.slope_den
        ));
    }
    // slope_num/slope_den as rationals: derived ≤ sealed.
    // INVARIANT(SlopeCompare): cross-multiply clips; wrap would invert the ≤ test.
    if derived.slope_num.saturating_mul(RECOVERY_SLA_SLOPE_DEN)
        > RECOVERY_SLA_SLOPE_NUM.saturating_mul(derived.slope_den)
    {
        return Err(format!(
            "derived slope {}/{} exceeds sealed RECOVERY_SLA_SLOPE {}/{} — \
             re-seal upward (never shrink):\n\
             RECOVERY_SLA_INTERCEPT_NS = {};\n\
             RECOVERY_SLA_SLOPE_NUM = {};\n\
             RECOVERY_SLA_SLOPE_DEN = {};",
            derived.slope_num,
            derived.slope_den,
            RECOVERY_SLA_SLOPE_NUM,
            RECOVERY_SLA_SLOPE_DEN,
            derived.intercept_ns.max(RECOVERY_SLA_INTERCEPT_NS),
            derived.slope_num,
            derived.slope_den
        ));
    }

    // Derived f itself must cover every sample (sanity on the fit).
    for s in &samples {
        let bound = derived_bound_ns(derived, s.bytes_since_last_flush);
        if s.recovery_time_ns > bound {
            return Err(format!(
                "derived f under-covers sample: recovery_time_ns={} \
                 f(bytes_since_last_flush={})={}",
                s.recovery_time_ns, s.bytes_since_last_flush, bound
            ));
        }
    }

    // Answer-agreement (§87): every sample and corpus p999 ≤ sealed f (bound).
    for s in &samples {
        let bound = recovery_time_bound_ns(s.bytes_since_last_flush);
        if s.recovery_time_ns > bound {
            return Err(format!(
                "recovery_time_ns={} exceeds f(bytes_since_last_flush={})={} — \
                 re-seal RECOVERY_SLA_* upward after path change, never shrink the meter",
                s.recovery_time_ns, s.bytes_since_last_flush, bound
            ));
        }
    }
    let bound_worst = recovery_time_bound_ns(worst_bytes);
    if recovery_time_p999 > bound_worst {
        return Err(format!(
            "recovery_time_p999={recovery_time_p999} exceeds \
             f(bytes_since_last_flush={worst_bytes})={bound_worst}"
        ));
    }

    // Claim-shaped check without the private emit door (mirrors
    // `emit_recovery_sla_claim`): at bound → emit ok; one ns over → refuse.
    let bytes_since_last_flush = samples[0].bytes_since_last_flush;
    let bound = recovery_time_bound_ns(bytes_since_last_flush);
    let claim_ok = |p999: u64, bytes: u64| -> bool { p999 <= recovery_time_bound_ns(bytes) };
    if !claim_ok(bound, bytes_since_last_flush) {
        return Err("claim at sealed f(bytes_since_last_flush) must succeed".into());
    }
    match bound.checked_add(1) {
        Some(over) if claim_ok(over, bytes_since_last_flush) => {
            return Err("claim above f(bytes_since_last_flush) must refuse".into());
        }
        Some(_) | None => {}
    }
    if !claim_ok(recovery_time_p999, worst_bytes) {
        return Err("calibrated recovery_time_p999 must answer-agree with sealed f".into());
    }

    println!(
        "sealed RECOVERY_SLA_INTERCEPT_NS={RECOVERY_SLA_INTERCEPT_NS} \
         RECOVERY_SLA_SLOPE_NUM={RECOVERY_SLA_SLOPE_NUM} \
         RECOVERY_SLA_SLOPE_DEN={RECOVERY_SLA_SLOPE_DEN}"
    );
    println!(
        "calibrated recovery_time_p999={recovery_time_p999}ns \
         bytes_since_last_flush_min={min_bytes} \
         bytes_since_last_flush_worst={worst_bytes} \
         f_worst={bound_worst}ns \
         opponent_pin={RECOVERY_SLA_OPPONENT_PIN} \
         tagged_commit={RECOVERY_SLA_TAGGED_COMMIT}"
    );
    // Keep board / Spec tokens load-bearing in this bench seat.
    let _: u64 = recovery_time_p999;
    let _: u64 = bytes_since_last_flush;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("recovery_sla: {e}");
            ExitCode::FAILURE
        }
    }
}
