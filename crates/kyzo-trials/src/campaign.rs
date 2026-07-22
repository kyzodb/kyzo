/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One door for seed-sweep campaigns shared by gauntlet / serializability.

/// Read `var` as `u64`, or `default` when unset / unparsable.
pub(crate) fn env_u64(var: &str, default: u64) -> u64 {
    match std::env::var(var) {
        Ok(s) => match s.parse() {
            Ok(n) => n,
            Err(_) => default,
        },
        Err(_) => default,
    }
}

/// Campaign index `i` → seed, matching `Rng::new(base ^ i×GOLDEN).next_u64()`.
pub(crate) fn seed_at(base: u64, i: u64) -> u64 {
    // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
    let mut state = base ^ u64::wrapping_mul(i, 0x9E37_79B9_7F4A_7C15);
    state = u64::wrapping_add(state, 0x9E37_79B9_7F4A_7C15);
    let mut z = state;
    z = u64::wrapping_mul(z ^ (z >> 30), 0xBF58_476D_1CE4_E5B9);
    z = u64::wrapping_mul(z ^ (z >> 27), 0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Sweep `count` seeds from `base`, collecting `(seed, failure)` for each `Err`.
pub(crate) fn run_seed_campaign(
    base: u64,
    count: u64,
    mut run: impl FnMut(u64) -> Result<(), String>,
) -> Vec<(u64, String)> {
    let mut failures = Vec::new();
    for i in 0..count {
        let seed = seed_at(base, i);
        if let Err(f) = run(seed) {
            failures.push((seed, f));
        }
    }
    failures
}
