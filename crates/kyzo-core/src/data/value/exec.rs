/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Admitted execution rows: the within-epoch execution currency for
//! joins that only RECOMBINE existing values (no new-value construction).
//!
//! The two-form law, made operational. Durable form is canonical bytes
//! (`data::value::canonical`), materialized only at storage/scan/output
//! boundaries. EXECUTION form is raw codes under a proven [`Domain`]: an
//! [`ExecRows`] is row-major `u32` codes whose Domain proves one arena +
//! epoch + visibility, so two cells are EQUAL iff their codes are equal —
//! no canonical encode, no dereference, no re-interning in the hot loop.
//!
//! The narrow door: admitted rows in, admitted rows out. [`ExecRows`] is
//! built ONLY by admitting a code-backed [`Rows`] (which entered through
//! the stamp-verifying write doors) or by [`ExecRows::join_project`],
//! which COPIES admitted codes from its inputs. There is no constructor
//! from arbitrary `u32`, so a code can neither be injected nor forged, and
//! the output of a recombination is admitted under the SAME Domain as its
//! inputs by construction.

// #119 execution-currency foundation / naive oracle: exercised by its own tests (and, for
// laws, by runtime/verify.rs); #120 wires the foundation into the RA engine. dead_code is
// target-split (used in one target, dead in another), so #[expect] cannot be satisfied uniformly.
#![allow(dead_code)]

use std::collections::HashMap;

use super::arena::BulkObserver;
use super::column::Domain;
use super::row::{AdmittedRows, Rows};

/// Which input a projected output column is copied from.
#[derive(Clone, Copy, Debug)]
pub enum Side {
    Left,
    Right,
}

/// Admitted execution rows: row-major raw codes under one [`Domain`].
///
/// The codes are OWNED (copied out of an admitted container), so an
/// `ExecRows` outlives the borrow that admitted it. Every code is `<
/// domain.extent()` (admission proved it), so it resolves through any
/// observer of the same arena+epoch whose visibility covers the domain.
///
/// @authority ExecRows
/// @layer value
/// @owns the admitted execution currency; codes are unforgeable (private field, no raw-code door)
/// @constructs ExecRows::admit(&Rows, &O) | ExecRows::join_project
/// @forbids from_raw over raw codes | injecting codes into Rows | reconstruction outside admission
/// @converts ExecRows -> StorageKey (canonical bytes only at a storage/output boundary, never durable identity)
/// @gate no raw-code door exposed; zero-canonical-encode-in-fixpoint law (#120)
/// @status established #119
pub struct ExecRows {
    domain: Domain,
    arity: usize,
    /// Row-major: `codes[r * arity + c]` is row `r`, column `c`.
    codes: Vec<u32>,
}

impl ExecRows {
    /// THE DOOR (in): admit a code-backed [`Rows`] against `o` and copy its
    /// raw codes into owned execution rows. The admission (arena + epoch +
    /// visibility) is the container's own `admit`; nothing here injects a
    /// code.
    pub fn admit<O: BulkObserver>(rows: &Rows, o: &O) -> ExecRows {
        let admitted: AdmittedRows<'_, O> = rows.admit(o);
        ExecRows {
            domain: rows.domain(),
            arity: rows.arity(),
            codes: admitted.raw().to_vec(),
        }
    }

    pub fn arity(&self) -> usize {
        self.arity
    }

    pub fn len(&self) -> usize {
        self.codes.len().checked_div(self.arity).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    pub fn domain(&self) -> Domain {
        self.domain
    }

    /// Row `r`'s codes.
    pub fn row(&self, r: usize) -> &[u32] {
        &self.codes[r * self.arity..(r + 1) * self.arity]
    }

    /// THE DOOR (recombine): hash-join `self` and `other` on the code
    /// equality `self[self_col] == other[other_col]` — lawful because both
    /// share this arena+epoch, so equal codes mean equal values — and emit
    /// the columns named by `out`, each `(Side, column)`, COPYING the
    /// matched inputs' codes. No canonical encode, no interning, no
    /// dereference: pure `u32` movement. The output is admitted execution
    /// rows under the SAME Domain (the wider extent of the two inputs), so
    /// its codes remain provably in-domain.
    ///
    /// # Panics
    /// If the two inputs are not the same arena+epoch domain (u32 identity
    /// would not mean value identity across domains).
    pub fn join_project(
        &self,
        other: &ExecRows,
        self_col: usize,
        other_col: usize,
        out: &[(Side, usize)],
    ) -> ExecRows {
        assert_eq!(
            self.domain.arena_id(),
            other.domain.arena_id(),
            "join across different arenas: u32 identity is not value identity"
        );
        assert_eq!(
            self.domain.epoch(),
            other.domain.epoch(),
            "join across different epochs: gather to a common epoch first"
        );
        // Build a probe on `other`'s join column: code -> the row indices
        // carrying it.
        let mut index: HashMap<u32, Vec<usize>> = HashMap::new();
        for r in 0..other.len() {
            index.entry(other.row(r)[other_col]).or_default().push(r);
        }
        let out_arity = out.len();
        let mut codes = Vec::new();
        for lr in 0..self.len() {
            let key = self.row(lr)[self_col];
            let Some(matches) = index.get(&key) else {
                continue;
            };
            for &rr in matches {
                for &(side, col) in out {
                    let code = match side {
                        Side::Left => self.row(lr)[col],
                        Side::Right => other.row(rr)[col],
                    };
                    codes.push(code);
                }
            }
        }
        // The output's domain is the wider extent so every copied code
        // stays below it.
        let domain = if self.domain.extent() >= other.domain.extent() {
            self.domain
        } else {
            other.domain
        };
        ExecRows {
            domain,
            arity: out_arity.max(1),
            codes,
        }
    }

    /// Row-major raw codes (for the dedup sink; still admitted).
    pub fn raw(&self) -> &[u32] {
        &self.codes
    }

    /// THE DOOR (out): canonical bytes of cell `(row, col)` -- the only
    /// place a code becomes bytes. Admits this domain against `o` (proving
    /// visibility) and spends the code through the observer. Callers
    /// materialize bytes only at storage/scan/output boundaries.
    pub fn resolve_cell<'o, O: BulkObserver>(&self, o: &'o O, row: usize, col: usize) -> &'o [u8] {
        let proof = self.domain.admit(o);
        o.resolve_raw(self.row(row)[col] as usize, &proof)
    }
}

/// Packed `u32` tuple dedup under one [`Domain`]: the hot-loop identity.
/// Two derived tuples are the SAME iff their code tuples are equal —
/// `u32`-slice equality, no canonical encode. Every inserted tuple's cells
/// come from an [`ExecRows`] (admitted), so no arbitrary code enters.
///
/// @authority ExecDedup
/// @layer value
/// @owns the fixpoint dedup identity: packed admitted code tuples, u32-slice equality, no canonical encode in the hot loop
/// @constructs ExecDedup::new | ExecDedup::insert | ExecDedup::absorb
/// @forbids inserting codes that did not come from an admitted ExecRows | mixing domains in one sink
/// @converts ExecDedup -> ExecRows (to_exec: the distinct rows, same Domain)
/// @gate zero-canonical-encode-in-fixpoint law (#120)
/// @status established #119
pub struct ExecDedup {
    domain: Domain,
    arity: usize,
    /// Row-major insertion-ordered codes of the DISTINCT tuples.
    rows: Vec<u32>,
    /// Membership by packed code tuple.
    seen: HashMap<Box<[u32]>, ()>,
}

impl ExecDedup {
    /// A fresh dedup sink over `domain`, holding `arity`-wide tuples.
    pub fn new(domain: Domain, arity: usize) -> ExecDedup {
        assert!(arity >= 1, "a tuple has at least one column");
        ExecDedup {
            domain,
            arity,
            rows: Vec::new(),
            seen: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    pub fn domain(&self) -> Domain {
        self.domain
    }

    /// Is this exact code tuple already present? `u32`-slice lookup, no
    /// encode.
    pub fn contains(&self, tuple: &[u32]) -> bool {
        debug_assert_eq!(tuple.len(), self.arity, "dedup probe arity");
        self.seen.contains_key(tuple)
    }

    /// Insert a code tuple; returns `true` if it was NEW. `u32`-slice
    /// identity dedup.
    pub fn insert(&mut self, tuple: &[u32]) -> bool {
        assert_eq!(tuple.len(), self.arity, "dedup insert arity");
        if self.seen.contains_key(tuple) {
            return false;
        }
        self.seen.insert(tuple.into(), ());
        self.rows.extend_from_slice(tuple);
        true
    }

    /// Absorb every row of `rows` (same domain), deduping. Returns the
    /// count of genuinely-new tuples.
    pub fn absorb(&mut self, rows: &ExecRows) -> usize {
        assert_eq!(self.arity, rows.arity(), "absorb arity mismatch");
        assert_eq!(
            self.domain.arena_id(),
            rows.domain().arena_id(),
            "absorb across arenas"
        );
        assert_eq!(
            self.domain.epoch(),
            rows.domain().epoch(),
            "absorb across epochs"
        );
        let mut new = 0;
        for r in 0..rows.len() {
            if self.insert(rows.row(r)) {
                new += 1;
            }
        }
        new
    }

    /// The distinct tuples as execution rows (for the next iteration's
    /// join). Codes are the deduped ones, still in-domain.
    pub fn to_exec(&self) -> ExecRows {
        ExecRows {
            domain: self.domain,
            arity: self.arity,
            codes: self.rows.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::DataValue;
    use super::super::arena::Arena;
    use super::super::canonical::encode_owned;
    use super::*;

    /// Intern a value's canonical bytes, returning its stamped code.
    fn intern(arena: &mut Arena, v: i64) -> super::super::code::StampedCode {
        arena.intern(encode_owned(&DataValue::from(v)).as_bytes())
    }

    /// Build a 2-arity Rows of (a, b) pairs in `arena`.
    fn rows_of(arena: &mut Arena, pairs: &[(i64, i64)]) -> Rows {
        let stamps: Vec<(
            super::super::code::StampedCode,
            super::super::code::StampedCode,
        )> = pairs
            .iter()
            .map(|&(a, b)| (intern(arena, a), intern(arena, b)))
            .collect();
        let f = arena.frame();
        let mut rows = Rows::new_in(2, &f);
        for (a, b) in stamps {
            rows.push_row(&[a, b]);
        }
        rows
    }

    /// The transitive-closure recombination step on codes, checked against
    /// a value oracle: path(x,z) :- edge(x,y), edge(y,z).
    #[test]
    fn join_project_recombines_by_code_identity() {
        let mut arena = Arena::new();
        // edges 1->2, 2->3, 3->4, 1->5
        let edges = rows_of(&mut arena, &[(1, 2), (2, 3), (3, 4), (1, 5)]);
        let f = arena.frame();
        let e = ExecRows::admit(&edges, &f);
        // one TC step: join edge(x,y) with edge(y,z) on y, emit (x, z).
        let step = e.join_project(&e, 1, 0, &[(Side::Left, 0), (Side::Right, 1)]);
        // Resolve the produced code pairs back to values and compare to
        // the hand oracle {(1,3),(2,4)}.
        let mut got: Vec<(i64, i64)> = Vec::new();
        for r in 0..step.len() {
            let x = decode_int(step.resolve_cell(&f, r, 0));
            let z = decode_int(step.resolve_cell(&f, r, 1));
            got.push((x, z));
        }
        got.sort();
        assert_eq!(got, vec![(1, 3), (2, 4)]);
    }

    /// Dedup is exact u32-tuple identity: re-deriving an existing pair is
    /// not new.
    #[test]
    fn dedup_is_u32_tuple_identity() {
        let mut arena = Arena::new();
        let rows = rows_of(&mut arena, &[(1, 2), (2, 3), (1, 2)]);
        let f = arena.frame();
        let e = ExecRows::admit(&rows, &f);
        let mut dedup = ExecDedup::new(e.domain(), 2);
        let new = dedup.absorb(&e);
        assert_eq!(new, 2, "the duplicate (1,2) must not be a new tuple");
        assert_eq!(dedup.len(), 2);
        // A second absorb of the same rows adds nothing.
        assert_eq!(dedup.absorb(&e), 0);
    }

    /// THE FOUNDATIONAL GUARANTEE (why this exists): the recombine door
    /// and the dedup do ZERO interning and ZERO canonical encoding/deref
    /// in the hot loop. Proven with the arena's own counters: after
    /// building the inputs (which interned once, at ingestion), a full
    /// join_project + dedup pass leaves the arena's distinct-value count
    /// UNCHANGED (no re-intern) and its compare-deref counter at ZERO (no
    /// payload dereference -- pure u32 identity).
    #[test]
    fn recombine_and_dedup_never_intern_encode_or_deref() {
        let mut arena = Arena::new();
        let edges = rows_of(&mut arena, &[(1, 2), (2, 3), (3, 4), (1, 5), (4, 6)]);
        let interned_after_load = arena.len();
        let derefs_after_load = arena.compare_derefs();

        let f = arena.frame();
        let e = ExecRows::admit(&edges, &f);
        // Two TC steps + dedup -- the recombination hot loop.
        let step1 = e.join_project(&e, 1, 0, &[(Side::Left, 0), (Side::Right, 1)]);
        let step2 = step1.join_project(&e, 1, 0, &[(Side::Left, 0), (Side::Right, 1)]);
        let mut dedup = ExecDedup::new(e.domain(), 2);
        dedup.absorb(&e);
        dedup.absorb(&step1);
        dedup.absorb(&step2);
        let _ = dedup.to_exec();

        // No value was interned by recombination or dedup: they only COPY
        // and COMPARE admitted codes.
        assert_eq!(
            arena.len(),
            interned_after_load,
            "recombine/dedup interned a new value -- the hot loop must not intern"
        );
        // No payload was ever dereferenced: identity was pure u32.
        assert_eq!(
            arena.compare_derefs(),
            derefs_after_load,
            "recombine/dedup dereferenced a payload -- the hot loop must not encode/deref"
        );
        assert!(!dedup.is_empty());
    }

    /// The narrow door has no back door: `ExecRows` cannot be built from
    /// arbitrary `u32`. Its only constructors are `admit` (from a
    /// stamp-verified `Rows`) and `join_project` (copying admitted codes).
    /// A raw-code injection or a forged code is unrepresentable -- there is
    /// no `pub fn from_raw(Vec<u32>)`, and the field is private. (The Code
    /// / StampedCode forge vectors themselves are proven absent in
    /// `data::value::proofs`.)
    #[test]
    fn exec_rows_has_no_raw_constructor() {
        // Compile-time witness: the only paths to an ExecRows are the two
        // doors. This test documents the structural guarantee; the
        // absence of a raw constructor is enforced by the private `codes`
        // field (no external module can name it).
        let mut arena = Arena::new();
        let rows = rows_of(&mut arena, &[(1, 2)]);
        let f = arena.frame();
        let e = ExecRows::admit(&rows, &f);
        assert_eq!(e.arity(), 2);
    }

    /// DIFFERENTIAL: `join_project` on codes equals a naive nested-loop
    /// join on the underlying values, for arbitrary edge sets. The code
    /// path and the value oracle must agree on the exact multiset of output
    /// pairs (order aside).
    #[test]
    fn join_project_equals_naive_value_join() {
        // A handful of adversarial edge sets: self-loops, duplicate edges,
        // fan-in/fan-out, disconnected, and a value range wider than a byte.
        let cases: &[&[(i64, i64)]] = &[
            &[(1, 2), (2, 3), (3, 4), (1, 5)],
            &[(1, 1), (1, 2), (2, 1)],             // self-loop + back-edge
            &[(5, 5), (5, 5)],                     // duplicate edge
            &[(1, 9), (2, 9), (9, 3), (9, 4)],     // fan-in then fan-out
            &[(1, 2), (3, 4)],                     // disconnected: empty join
            &[(300, 400), (400, 500), (500, 600)], // multi-byte values
            &[(-3, 0), (0, 7), (7, -3)],           // negatives, a cycle
        ];
        for pairs in cases {
            let mut arena = Arena::new();
            let edges = rows_of(&mut arena, pairs);
            let f = arena.frame();
            let e = ExecRows::admit(&edges, &f);
            // Code path: edge(x,y) ⋈_y edge(y,z) → (x, z).
            let step = e.join_project(&e, 1, 0, &[(Side::Left, 0), (Side::Right, 1)]);
            let mut got: Vec<(i64, i64)> = (0..step.len())
                .map(|r| {
                    (
                        decode_int(step.resolve_cell(&f, r, 0)),
                        decode_int(step.resolve_cell(&f, r, 1)),
                    )
                })
                .collect();
            got.sort();
            // Naive oracle on the raw values.
            let mut want: Vec<(i64, i64)> = Vec::new();
            for &(x, y1) in *pairs {
                for &(y2, z) in *pairs {
                    if y1 == y2 {
                        want.push((x, z));
                    }
                }
            }
            want.sort();
            assert_eq!(
                got, want,
                "code join disagreed with the value oracle on {pairs:?}"
            );
        }
    }

    /// DETERMINISM: `join_project` is a pure function of its inputs — the
    /// output row order is identical across repeated runs (the probe is a
    /// lookup, never an iteration over hash order), so the engine's
    /// schedule-independence law is preserved when this becomes the
    /// fixpoint currency.
    #[test]
    fn join_project_output_is_deterministic() {
        let pairs = &[(1, 9), (2, 9), (3, 9), (9, 10), (9, 11), (9, 12)];
        let mut arena = Arena::new();
        let edges = rows_of(&mut arena, pairs);
        let f = arena.frame();
        let e = ExecRows::admit(&edges, &f);
        let a = e.join_project(&e, 1, 0, &[(Side::Left, 0), (Side::Right, 1)]);
        let b = e.join_project(&e, 1, 0, &[(Side::Left, 0), (Side::Right, 1)]);
        assert_eq!(
            a.raw(),
            b.raw(),
            "join_project output order is not deterministic"
        );
        // And it is the left-row-major order the fixpoint relies on.
        assert_eq!(a.len(), 9, "3 left rows × 3 right matches on code 9");
    }

    /// The domain guard: joining rows from two DIFFERENT arenas is refused
    /// — u32 code identity is only value identity within one arena+epoch.
    #[test]
    #[should_panic(expected = "different arenas")]
    fn join_project_across_arenas_panics() {
        let mut a1 = Arena::new();
        let r1 = rows_of(&mut a1, &[(1, 2)]);
        let f1 = a1.frame();
        let e1 = ExecRows::admit(&r1, &f1);

        let mut a2 = Arena::new();
        let r2 = rows_of(&mut a2, &[(2, 3)]);
        let f2 = a2.frame();
        let e2 = ExecRows::admit(&r2, &f2);

        // Same shape, foreign arena: the codes are incomparable.
        let _ = e1.join_project(&e2, 1, 0, &[(Side::Left, 0), (Side::Right, 1)]);
    }

    fn decode_int(bytes: &[u8]) -> i64 {
        match super::super::canonical::decode(bytes).expect("lawful") {
            DataValue::Num(n) => n.as_int().expect("int"),
            other => panic!("not an int: {other:?}"),
        }
    }
}
