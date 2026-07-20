//! Adversarial campaigns crate — trial batteries land here by seat.
//!
//! Capabilities relocated from condemned `kyzo-core::query::trials`:
//! - [`gauntlet`] — GenParams generator + Capability 1 (determinism)
//! - [`provenance`] — Capability 2 (proof reconstruction + four-corruption)
//! - [`determinism`] — thread-count lane pointing at Cap1
//! - [`time_travel`] — Capabilities 3–4 (temporal generator + refusal-lift)
//!
//! ## Storage conformance kit (§85)
//!
//! [`conformance`] is public law for second backends: call
//! [`run_full_battery`] (or the individual `law_*` functions) from any crate
//! that depends on `kyzo-trials`, with a factory that yields a fresh empty
//! [`kyzo::Storage`]. Do not copy the scenarios into the adopting crate.

#![forbid(unsafe_code)]

/// Storage-contract conformance kit — `pub` surface for out-of-crate backends.
pub mod conformance;
pub mod determinism;
pub mod gauntlet;
pub mod provenance;
pub mod time_travel;
/// Oracle-differential verify corpus (re-homed from kyzo-core session/verify).
/// Test-only module (`#![cfg(test)]` inside).
pub mod verify_differential;

pub use conformance::{
    law_concurrent_writers_across_threads, law_del_range_chunk_boundaries,
    law_del_range_kills_own_writes, law_kv_matches_model_oracle, law_mvcc_first_committer_wins,
    law_phantom_protection, law_read_your_own_writes_and_snapshot_isolation,
    law_send_sync_bounds_are_compiler_checked, run_full_battery,
};
