//! Encoding-law battery seat (story #350 T4 / 03-storage-store.json;
//! story #352 T1 Expr canonical serde).
//!
//! The one law (binary order = semantic order) is model vocabulary; its
//! property battery lives in [`tests`]. Value byte encode/decode itself
//! remains in [`crate::value::canonical`]. Expr's one normative serde
//! codec (seat 59) is pinned by a golden round-trip vector in [`tests`].

#![cfg_attr(not(test), allow(dead_code))]

#[cfg(test)]
mod tests;
