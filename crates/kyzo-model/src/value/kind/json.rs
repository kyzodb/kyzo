/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Json`: canonical JSON **semantics**, never syntax — two replicas
//! ingesting "the same" document must intern one value.
//!
//! The identity law:
//! - Object keys canonicalize to sorted order (byte order of the key
//!   text); key writing order is not identity.
//! - **Duplicate keys are refused at construction** ([`JsonObj::new`]) —
//!   not last-wins, not first-wins: an ambiguous document is not a
//!   value. The refusal is typed.
//! - Numbers go through [`Num`]'s law exactly (one `-0.0`, int/float
//!   identity as ruled there) AND must be finite: JSON has no NaN or
//!   infinity, and this kind is JSON, not extended-JSON — [`JsonNum`]
//!   refuses non-finite at construction, and decode refuses it in bytes.
//!   (The int/float distinction inside finite numbers IS deliberately
//!   more than interchange JSON expresses; serializers map it
//!   faithfully.)
//! - Strings are exact byte identity of their UTF-8 (no unicode
//!   normalization: `é` composed and decomposed are different values,
//!   deliberately — normalization is a query-layer choice, not an
//!   identity-destroying default).
//! - Object keys sort by their RAW UTF-8 bytes; the grammar's escaping
//!   is order-preserving over raw bytes, so the escaped payload order and
//!   the raw key order are one order (law-tested with NUL-bearing keys).
//! - The canonical payload carries a trailing FNV-1a 64 hash of the
//!   canonical value bytes. The algorithm is pinned as format v1; the hash
//!   rides AFTER the self-terminating value bytes, so it can never
//!   influence order.
//!   **The hash is an accelerator and a filter, never equality
//!   authority**: FNV collides, so equality and join correctness always
//!   confirm the canonical value bytes; decode verifies the hash and
//!   refuses mismatches (identical values with different trailing bytes
//!   cannot exist).

use super::super::number::Num;

/// A JSON value tree in canonical identity form. `Hash` is lawful:
/// every constituent hashes by its identity law (JsonNum through Num).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Json {
    Null,
    Bool(bool),
    Num(JsonNum),
    Str(String),
    Arr(Vec<Json>),
    Obj(JsonObj),
}

/// A JSON number: a [`Num`] proven finite. JSON has no NaN or infinity;
/// an unlawful number cannot be written into a tree.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct JsonNum(Num);

/// The typed refusal for non-finite numbers.
///
/// Shared by plane [`JsonNum`] construction and the serde JSON wire encode
/// of [`crate::DataValue::Num`]: JSON has no NaN/Inf, so the boundary
/// refuses rather than remapping to Null/Str (which would change value kind).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NonFiniteJsonNumber;

impl std::fmt::Display for NonFiniteJsonNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JSON cannot represent non-finite numbers (NaN/Inf)")
    }
}

impl std::error::Error for NonFiniteJsonNumber {}

impl JsonNum {
    pub fn new(n: Num) -> Result<JsonNum, NonFiniteJsonNumber> {
        match n.as_float() {
            Some(f) if !f.is_finite() => Err(NonFiniteJsonNumber),
            _other => Ok(JsonNum(n)),
        }
    }

    pub fn num(self) -> Num {
        self.0
    }
}

/// A canonical JSON object: entries sorted by key, keys unique.
/// Constructible only through [`JsonObj::new`], which sorts and refuses
/// duplicates — an unlawful object cannot be written down.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct JsonObj(Vec<(String, Json)>);

/// The typed refusal for ambiguous documents.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DuplicateKey(pub String);

impl JsonObj {
    /// Sort entries by key bytes; refuse duplicates.
    pub fn new(mut entries: Vec<(String, Json)>) -> Result<JsonObj, DuplicateKey> {
        entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        for w in entries.windows(2) {
            if w[0].0 == w[1].0 {
                return Err(DuplicateKey(w[0].0.clone()));
            }
        }
        Ok(JsonObj(entries))
    }

    pub fn entries(&self) -> &[(String, Json)] {
        &self.0
    }
}

/// FNV-1a 64: the pinned v1 hash for JSON canonical bytes. Chosen for
/// being dependency-free, byte-exact, and trivially reimplementable by
/// any reader of the spec (which is also how the tests verify it:
/// against an independent second implementation).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        // INVARIANT(fnv1a): FNV-1a prime mix is defined as wrapping mul on u64.
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

#[cfg(test)]
mod tests {
    use miette::{IntoDiagnostic, Result, miette};

    use super::*;

    #[test]
    fn json_numbers_must_be_finite() {
        assert!(JsonNum::new(Num::int(7)).is_ok());
        assert!(JsonNum::new(Num::float(1.5)).is_ok());
        assert_eq!(JsonNum::new(Num::float(f64::NAN)), Err(NonFiniteJsonNumber));
        assert_eq!(
            JsonNum::new(Num::float(f64::INFINITY)),
            Err(NonFiniteJsonNumber)
        );
        assert_eq!(
            JsonNum::new(Num::float(f64::NEG_INFINITY)),
            Err(NonFiniteJsonNumber)
        );
    }

    #[test]
    fn objects_canonicalize_key_order_and_refuse_duplicates() -> Result<()> {
        let a = JsonObj::new(vec![
            ("b".into(), Json::Null),
            ("a".into(), Json::Bool(true)),
        ])
        .into_diagnostic()?;
        assert_eq!(a.entries()[0].0, "a");
        assert_eq!(a.entries()[1].0, "b");
        let dup = JsonObj::new(vec![
            ("k".into(), Json::Null),
            ("k".into(), Json::Bool(false)),
        ]);
        assert_eq!(dup, Err(DuplicateKey("k".into())));
        Ok(())
    }

    #[test]
    fn fnv_vectors_pinned() {
        // Standard FNV-1a 64 test vectors: the algorithm is format v1.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }
}
