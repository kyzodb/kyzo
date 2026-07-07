/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Row`: an interned tuple — a slice of `Code`s — with a `Value`-cell view only at the API boundary. `EncodedKey` is the written form.
//!
//! ## The code-lifetime law (the two-form Row)
//!
//! **Codes never persist across a seal. The durable form is canonical
//! bytes. Codes are within-epoch execution currency.**
//!
//! The two forms are two types with one conversion authority each way:
//!
//! - [`Rows`] is the execution form: row-major packed raw codes under ONE
//!   container domain (it *is* a [`CodeColumn`] with an arity, inheriting
//!   the write door, the admission theorem, and the gather law wholesale).
//!   It has **no serialization surface** — you cannot write codes down.
//! - [`EncodedKey`] is the written form: the tuple's canonical encodings
//!   concatenated. Self-terminating element encodings make concatenation
//!   order-preserving (lexicographic tuple order = elementwise semantic
//!   order) and unambiguous to split. It has **no code accessors** — you
//!   cannot smuggle execution currency out of stored bytes.
//! - The doors: [`AdmittedRows::encode_row`] (execution → bytes, through
//!   an admitted observer) and [`Rows::push_encoded`] (bytes → execution,
//!   validated element-by-element, re-interned into the current epoch).
//!
//! Consequence: a seal invalidates nothing durable. Standing state at
//! rest is bytes and needs no gather; only live in-memory containers
//! cross epochs, explicitly, through their gather doors.
//!
//! ## The fixpoint choreography
//!
//! A semi-naive iteration alternates a **read phase** (frame open, joins
//! and dedup on admitted raw codes — identity is exact within the domain)
//! with a **mint phase** (frame dropped, newly derived values interned,
//! stamps pushed through the write door). `intern` takes `&mut Arena`, so
//! the borrow checker enforces the alternation; the epoch is unchanged
//! throughout, so held containers stay admissible with no remap mid-run.
//! The choreography is pinned as a law test below.

use super::arena::{Arena, BulkObserver, EpochRemap};
use super::canonical::{DecodeError, decode_one};
use super::column::{AdmittedCodes, CodeColumn, Domain};

/// The execution form of a relation fragment: `arity`-wide tuples as
/// row-major packed codes under one container domain.
pub struct Rows {
    arity: usize,
    codes: CodeColumn,
}

impl Rows {
    /// An empty tuple container in the observer's domain.
    ///
    /// # Panics
    ///
    /// Panics on zero arity (a relation has columns).
    pub fn new_in<O: BulkObserver>(arity: usize, o: &O) -> Rows {
        assert!(arity >= 1, "a relation has at least one column");
        Rows {
            arity,
            codes: CodeColumn::new_in(o),
        }
    }

    pub fn arity(&self) -> usize {
        self.arity
    }

    pub fn len(&self) -> usize {
        self.codes.len() / self.arity
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    pub fn domain(&self) -> Domain {
        self.codes.domain()
    }

    /// The write door: one tuple of stamped codes, verified element by
    /// element into the domain.
    ///
    /// # Panics
    ///
    /// Panics on arity mismatch or a stamp outside the domain.
    pub fn push_row(&mut self, stamps: &[super::code::StampedCode]) {
        assert_eq!(stamps.len(), self.arity, "tuple arity mismatch");
        for &sc in stamps {
            self.codes.push(sc);
        }
    }

    /// The bytes→execution door: validate a written key element by
    /// element (total: typed errors, never trust) and re-intern its
    /// values into the CURRENT epoch. The stamps the interns mint must
    /// match this container's domain — pushing into a stale container
    /// refuses at the write door like any other stale stamp.
    pub fn push_encoded(&mut self, key: &EncodedKey, arena: &mut Arena) -> Result<(), DecodeError> {
        let bytes = key.as_bytes();
        // Validate and split FIRST — nothing is interned unless the whole
        // key is lawful (no partial tuples on refusal).
        let mut splits = Vec::with_capacity(self.arity);
        let mut at = 0usize;
        for _ in 0..self.arity {
            let (_, used) = decode_one(&bytes[at..])?;
            splits.push((at, at + used));
            at += used;
        }
        if at != bytes.len() {
            return Err(DecodeError::TrailingBytes);
        }
        for (lo, hi) in splits {
            let sc = arena.intern(&bytes[lo..hi]);
            self.codes.push(sc);
        }
        Ok(())
    }

    /// The admission: one container-domain check for the whole relation
    /// fragment.
    pub fn admit<'a, O: BulkObserver>(&'a self, o: &'a O) -> AdmittedRows<'a, O> {
        AdmittedRows {
            arity: self.arity,
            codes: self.codes.admit(o),
        }
    }

    /// The gather door (see the gather law): consuming, the only mint of
    /// a new-epoch tuple container.
    pub fn gather(self, remap: &EpochRemap) -> Rows {
        Rows {
            arity: self.arity,
            codes: self.codes.gather(remap),
        }
    }
}

/// Admitted tuples: raw-code reads under the proven domain.
pub struct AdmittedRows<'a, O: BulkObserver> {
    arity: usize,
    codes: AdmittedCodes<'a, O>,
}

impl<'a, O: BulkObserver> AdmittedRows<'a, O> {
    pub fn len(&self) -> usize {
        self.codes.len() / self.arity
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    pub fn arity(&self) -> usize {
        self.arity
    }

    /// The flat raw codes of every tuple — identity currency for bulk
    /// dedup within this domain; never an ordering surface.
    pub fn raw(&self) -> &'a [u32] {
        self.codes.raw()
    }

    /// The raw codes of row `i` — tuple identity within this domain
    /// (equality/hash/dedup currency; never an ordering surface).
    pub fn row(&self, i: usize) -> &'a [u32] {
        &self.codes.raw()[i * self.arity..(i + 1) * self.arity]
    }

    /// Canonical bytes of cell `(row, col)`.
    pub fn resolve_cell(&self, row: usize, col: usize) -> &'a [u8] {
        self.codes.resolve(row * self.arity + col)
    }

    /// Semantic tuple order: elementwise value order (which is exactly
    /// what the written form's byte order embeds).
    pub fn cmp_rows(&self, i: usize, j: usize) -> std::cmp::Ordering {
        for k in 0..self.arity {
            let c = self.codes.cmp_at(i * self.arity + k, j * self.arity + k);
            if c != std::cmp::Ordering::Equal {
                return c;
            }
        }
        std::cmp::Ordering::Equal
    }

    /// The execution→bytes door: the written form of row `i`. Minted only
    /// here — an `EncodedKey` in hand is proof its bytes are concatenated
    /// canonical encodings.
    pub fn encode_row(&self, i: usize) -> EncodedKey {
        let mut out = Vec::new();
        for k in 0..self.arity {
            out.extend_from_slice(self.resolve_cell(i, k));
        }
        EncodedKey(out)
    }
}

/// The written form of a tuple: concatenated canonical encodings. Byte
/// order equals elementwise semantic tuple order (self-terminating
/// elements). No code accessors exist: stored bytes cannot leak execution
/// currency, and codes cannot leak into storage — the code-lifetime law,
/// held by the type surface.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EncodedKey(Vec<u8>);

impl EncodedKey {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::super::canonical::{Datum, encode};
    use super::super::code::StampedCode;
    use super::super::number::Num;
    use super::*;

    fn stamp_of(arena: &mut Arena, d: Datum<'_>) -> StampedCode {
        arena.intern(encode(d).as_bytes())
    }

    // ------------------------------------------------------------------
    // The two-form law: written bytes are the durable identity; codes
    // move under seals while the bytes do not.
    // ------------------------------------------------------------------

    #[test]
    fn written_form_is_durable_across_seals_while_codes_move() {
        let mut arena = Arena::new();
        let mut rows = Rows::new_in(2, &arena.frame());
        for i in 0..30i64 {
            let a = stamp_of(&mut arena, Datum::Num(Num::int(i * 7 % 13)));
            let b = stamp_of(
                &mut arena,
                Datum::Str(if i % 2 == 0 { "even" } else { "odd" }),
            );
            rows.push_row(&[a, b]);
        }
        let keys_before: Vec<EncodedKey> = {
            let f = arena.frame();
            let adm = rows.admit(&f);
            (0..adm.len()).map(|i| adm.encode_row(i)).collect()
        };
        let raw_before: Vec<Vec<u32>> = {
            let f = arena.frame();
            let adm = rows.admit(&f);
            (0..adm.len()).map(|i| adm.row(i).to_vec()).collect()
        };
        // Seal + gather: the execution currency moves...
        let remap = arena.seal();
        let rows = rows.gather(&remap);
        // ...and moves visibly (something re-ranked: 13 distinct nums +
        // 2 strings all started as tail codes).
        let f = arena.frame();
        let adm = rows.admit(&f);
        let raw_after: Vec<Vec<u32>> = (0..adm.len()).map(|i| adm.row(i).to_vec()).collect();
        assert_ne!(
            raw_before, raw_after,
            "seal moved no codes — test is vacuous"
        );
        // ...while the written form is byte-identical, row for row.
        for (i, k) in keys_before.iter().enumerate() {
            assert_eq!(
                &adm.encode_row(i),
                k,
                "the durable form moved with the seal"
            );
        }
    }

    /// The written form's byte order embeds elementwise tuple order.
    #[test]
    fn encoded_key_order_is_tuple_semantic_order() {
        let mut arena = Arena::new();
        let mut rows = Rows::new_in(2, &arena.frame());
        let tuples: [(i64, &str); 5] = [(3, "b"), (1, "zzz"), (3, "a"), (-5, "x"), (1, "a")];
        for (n, s) in tuples {
            let a = stamp_of(&mut arena, Datum::Num(Num::int(n)));
            let b = stamp_of(&mut arena, Datum::Str(s));
            rows.push_row(&[a, b]);
        }
        let f = arena.frame();
        let adm = rows.admit(&f);
        for i in 0..adm.len() {
            for j in 0..adm.len() {
                assert_eq!(
                    adm.encode_row(i).cmp(&adm.encode_row(j)),
                    adm.cmp_rows(i, j),
                    "key byte order diverged from tuple order at ({i},{j})"
                );
            }
        }
    }

    /// bytes → execution → bytes round-trips exactly; malformed keys
    /// refuse without partial pushes.
    #[test]
    fn push_encoded_round_trips_and_refuses_totally() {
        let mut arena = Arena::new();
        let mut rows = Rows::new_in(2, &arena.frame());
        let a = stamp_of(&mut arena, Datum::Num(Num::int(42)));
        let b = stamp_of(&mut arena, Datum::Str("hello"));
        rows.push_row(&[a, b]);
        let key = {
            let f = arena.frame();
            rows.admit(&f).encode_row(0)
        };
        // Re-enter through the bytes door.
        let mut rows2 = Rows::new_in(2, &arena.frame());
        rows2.push_encoded(&key, &mut arena).expect("lawful key");
        {
            let f = arena.frame();
            let adm2 = rows2.admit(&f);
            assert_eq!(adm2.encode_row(0), key, "bytes door changed the tuple");
            // Same epoch + arena dedup ⟹ same codes: tuple identity holds.
            let adm = rows.admit(&f);
            assert_eq!(adm.row(0), adm2.row(0));
        }
        // Truncated key: typed refusal, nothing pushed.
        let cut = EncodedKey(key.as_bytes()[..key.len() - 3].to_vec());
        let before = rows2.len();
        assert!(rows2.push_encoded(&cut, &mut arena).is_err());
        assert_eq!(rows2.len(), before, "refusal left a partial tuple");
        // Trailing garbage: refused.
        let mut fat = key.as_bytes().to_vec();
        fat.push(0x05);
        assert!(rows2.push_encoded(&EncodedKey(fat), &mut arena).is_err());
        assert_eq!(rows2.len(), before);
    }

    // ------------------------------------------------------------------
    // The fixpoint choreography, pinned: read phase / mint phase
    // alternation, identity-dedup on raw codes, stability across rounds,
    // and the commit boundary (seal + gather) at the end.
    // ------------------------------------------------------------------

    #[test]
    fn fixpoint_choreography_law() {
        let mut arena = Arena::new();
        let epoch0 = arena.epoch();
        // Seed relation: reach(x) for x in {0}; rule: reach(x+3) up to 12.
        let mut total = Rows::new_in(1, &arena.frame());
        let seed = stamp_of(&mut arena, Datum::Num(Num::int(0)));
        total.push_row(&[seed]);
        let mut frontier: Vec<Vec<u8>> = vec![encode(Datum::Num(Num::int(0))).as_bytes().to_vec()];
        let mut rounds = 0;
        while !frontier.is_empty() {
            rounds += 1;
            // MINT PHASE: derive new values from the frontier as bytes,
            // intern them (frame necessarily closed: intern is &mut).
            let mut fresh: Vec<StampedCode> = Vec::new();
            for bytes in frontier.drain(..) {
                let (datum, _) = decode_one(&bytes).expect("lawful");
                let n = match datum {
                    super::super::canonical::OwnedDatum::Num(n) => n.as_int().expect("int domain"),
                    other => panic!("wrong kind: {other:?}"),
                };
                if n + 3 <= 12 {
                    fresh.push(stamp_of(&mut arena, Datum::Num(Num::int(n + 3))));
                }
            }
            // READ PHASE: dedup the derived tuples against the total by
            // raw-code identity under one admitted domain, then extend.
            let novel: Vec<StampedCode> = {
                let f = arena.frame();
                let adm = total.admit(&f);
                let existing: std::collections::BTreeSet<u32> = adm.raw().iter().copied().collect();
                fresh
                    .into_iter()
                    .filter(|sc| !existing.contains(&sc.code().raw()))
                    .collect()
            };
            for sc in &novel {
                total.push_row(&[*sc]);
                let f = arena.frame();
                let adm = total.admit(&f);
                frontier.push(adm.resolve_cell(adm.len() - 1, 0).to_vec());
            }
            assert_eq!(arena.epoch(), epoch0, "no seal mid-fixpoint");
            assert!(rounds < 32, "fixpoint diverged");
        }
        // Fixpoint reached: {0,3,6,9,12}.
        assert_eq!(total.len(), 5);
        let keys_at_fixpoint: Vec<EncodedKey> = {
            let f = arena.frame();
            let adm = total.admit(&f);
            (0..adm.len()).map(|i| adm.encode_row(i)).collect()
        };
        // COMMIT BOUNDARY: seal once, gather the held container, and the
        // durable form is untouched.
        let remap = arena.seal();
        let total = total.gather(&remap);
        let f = arena.frame();
        let adm = total.admit(&f);
        for (i, k) in keys_at_fixpoint.iter().enumerate() {
            assert_eq!(&adm.encode_row(i), k);
        }
    }

    #[test]
    #[should_panic(expected = "tuple arity mismatch")]
    fn arity_is_enforced_at_the_write_door() {
        let mut arena = Arena::new();
        let sc = stamp_of(&mut arena, Datum::Null);
        let mut rows = Rows::new_in(2, &arena.frame());
        rows.push_row(&[sc]);
    }
}
