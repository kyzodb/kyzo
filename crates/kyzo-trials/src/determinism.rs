//! Thread-count determinism lane (campaigns_proposed).
//!
//! Capability 1's `determinism_campaign` / `run_seed` in [`crate::gauntlet`]
//! is the living campaign: generated programs at 1/2/4/8 rayon threads with
//! byte-identical answers, witness tables, and refusals.
//!
//! This module is the named seat for that lane — points at Cap1 so board /
//! architecture references to `kyzo-trials/determinism.rs` resolve without
//! a second corpus.

#![cfg(test)]

/// Point at Cap1's campaign (defined in [`crate::gauntlet`]).
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn thread_count_determinism_lane_shares_cap1() {
    // Smoke: one seed through the shared Cap1 door. The full sweep lives
    // as `gauntlet::determinism_campaign` (KYZO_TRIALS_SEEDS / BASE).
    crate::gauntlet::run_seed(0).expect("Cap1 run_seed holds for seed 0");
}
