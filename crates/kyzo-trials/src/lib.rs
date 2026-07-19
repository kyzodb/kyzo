//! Adversarial campaigns crate — trial batteries land here by seat.
//!
//! Capabilities relocated from condemned `kyzo-core::query::trials`:
//! - [`gauntlet`] — GenParams generator + Capability 1 (determinism)
//! - [`provenance`] — Capability 2 (proof reconstruction + four-corruption)
//! - [`determinism`] — thread-count lane pointing at Cap1
//! - [`time_travel`] — Capabilities 3–4 (temporal generator + refusal-lift)
//!
//! Other campaign files may still be orphan seats mid-cut; wire them when
//! their migrate entries run.

#![forbid(unsafe_code)]

pub mod determinism;
pub mod gauntlet;
pub mod provenance;
pub mod time_travel;
/// Oracle-differential verify corpus (re-homed from kyzo-core session/verify).
/// Test-only module (`#![cfg(test)]` inside).
pub mod verify_differential;
