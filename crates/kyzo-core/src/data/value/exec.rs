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

use super::admission::{Admission, Denial};
use super::arena::BulkObserver;
use super::arity::Arity;
use super::code::Code;
use super::column::Domain;
use super::row::{AdmittedRows, Rows};
use crate::data::value::data_value_any;

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
    arity: Arity,
    /// Row-major: `codes[r * arity + c]` is row `r`, column `c`.
    codes: Vec<u32>,
}

impl ExecRows {
    /// THE DOOR (in): admit a code-backed [`Rows`] against `o` and copy its
    /// raw codes into owned execution rows. The admission (arena + epoch +
    /// visibility) is the container's own `admit`; nothing here injects a
    /// code. Arena/epoch mismatch is a typed refusal.
    pub fn admit<O: BulkObserver>(
        rows: &Rows,
        o: &O,
    ) -> Result<ExecRows, Denial> {
        let admitted: AdmittedRows<'_, O> = rows.admit(o)?;
        Ok(ExecRows {
            domain: rows.domain(),
            arity: rows.arity(),
            codes: admitted.raw().to_vec(),
        })
    }

    pub fn arity(&self) -> Arity {
        self.arity
    }

    pub fn len(&self) -> usize {
        self.codes.len().checked_div(self.arity.get()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    pub fn domain(&self) -> Domain {
        self.domain
    }

    /// Row `r`'s codes.
    pub fn row(&self, r: usize) -> &[u32] {
        let w = self.arity.get();
        &self.codes[r * w..(r + 1) * w]
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
    /// Typed refusal when the two inputs are not the same arena+epoch
    /// domain (u32 identity would not mean value identity across domains).
    ///
    /// **Coexisting-arena boundary:** both inputs are owned execution rows
    /// that outlive any observer nest and may have been built under
    /// different brands (or none). Shared identity is mint-checked
    /// [`Admission::prove_shared`] — a nest brand cannot unify two
    /// coexisting domains at compile time.
    pub fn join_project(
        &self,
        other: &ExecRows,
        self_col: usize,
        other_col: usize,
        out: &[(Side, usize)],
    ) -> Result<ExecRows, Denial> {
        // Shared context for raw-handle identity — mint proves both sides
        // name one domain; mismatch refuses typed, never panics.
        let ctx = Admission::prove_shared(
            self.domain.arena_id(),
            self.domain.epoch(),
            other.domain.arena_id(),
            other.domain.epoch(),
        )?;
        // Build a probe on `other`'s join column: code -> the row indices
        // carrying it. Keys are packed under `ctx`.
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
            debug_assert!(matches
                .iter()
                .all(|&rr| ctx.same_handle(Code(key), Code(other.row(rr)[other_col]))));
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
        // Empty projection is not a lawful width — refuse typed, never invent Arity::ONE.
        let arity = Arity::try_new(out_arity).ok_or(Denial::EmptyProjection)?;
        // The output's domain is the wider extent so every copied code
        // stays below it.
        let domain = if self.domain.extent() >= other.domain.extent() {
            self.domain
        } else {
            other.domain
        };
        Ok(ExecRows {
            domain,
            arity,
            codes,
        })
    }

    /// Row-major raw codes (for the dedup sink; still admitted).
    pub fn raw(&self) -> &[u32] {
        &self.codes
    }

    /// THE DOOR (out): canonical bytes of cell `(row, col)` -- the only
    /// place a code becomes bytes. Admits this domain against `o` (proving
    /// visibility) and spends the code through the observer. Callers
    /// materialize bytes only at storage/scan/output boundaries.
    /// Arena/epoch mismatch is a typed refusal.
    pub fn resolve_cell<'o, O: BulkObserver>(
        &self,
        o: &'o O,
        row: usize,
        col: usize,
    ) -> Result<&'o [u8], Denial> {
        let proof = self.domain.admit(o)?;
        Ok(o.resolve_raw(self.row(row)[col] as usize, proof))
    }

    /// The compare/identity context for raw handles in these rows.
    ///
    /// **Coexisting-arena boundary:** unbranded durable [`Admission`] —
    /// [`ExecRows`] outlives observer nests (see [`Domain::ctx`]).
    pub fn ctx(&self) -> Admission {
        self.domain.ctx()
    }
}

/// Packed `u32` tuple dedup under one [`Domain`]: the hot-loop identity.
/// Two derived tuples are the SAME iff their code tuples are equal —
/// `u32`-slice equality, no canonical encode. The only insert door is
/// [`ExecDedup::absorb`] from admitted [`ExecRows`] — bare `&[u32]` cannot
/// enter the sink.
///
/// @authority ExecDedup
/// @layer value
/// @owns the fixpoint dedup identity: packed admitted code tuples, u32-slice equality, no canonical encode in the hot loop
/// @constructs ExecDedup::new | ExecDedup::absorb
/// @forbids inserting codes that did not come from an admitted ExecRows | mixing domains in one sink | bare `&[u32]` insert
/// @converts ExecDedup -> ExecRows (to_exec: the distinct rows, same Domain)
/// @gate zero-canonical-encode-in-fixpoint law (#120)
/// @status established #119
pub struct ExecDedup {
    domain: Domain,
    arity: Arity,
    /// Row-major insertion-ordered codes of the DISTINCT tuples.
    rows: Vec<u32>,
    /// Membership by packed code tuple.
    seen: HashMap<Box<[u32]>, ()>,
}

impl ExecDedup {
    /// A fresh dedup sink over `domain`, holding `arity`-wide tuples.
    ///
    /// Zero width is unrepresentable: [`Arity`] is [`NonZeroUsize`](std::num::NonZeroUsize)-backed.
    pub fn new(domain: Domain, arity: Arity) -> ExecDedup {
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

    /// Is this exact admitted row already present? Lookup under a shared
    /// domain proof — no bare `&[u32]` door.
    pub fn contains(&self, rows: &ExecRows, row: usize) -> Result<bool, Denial> {
        Admission::prove_shared(
            self.domain.arena_id(),
            self.domain.epoch(),
            rows.domain().arena_id(),
            rows.domain().epoch(),
        )?;
        if self.arity != rows.arity() {
            return Err(Denial::ArityMismatch {
                expected: self.arity.get(),
                got: rows.arity().get(),
            });
        }
        Ok(self.seen.contains_key(rows.row(row)))
    }

    /// Admit one row already proven by `rows`' domain. Private — the only
    /// public insert path is [`ExecDedup::absorb`].
    fn admit_row(&mut self, tuple: &[u32]) -> bool {
        if self.seen.contains_key(tuple) {
            return false;
        }
        self.seen.insert(tuple.into(), ());
        self.rows.extend_from_slice(tuple);
        true
    }

    /// Absorb every row of admitted `rows` (same arena+epoch), deduping.
    /// Returns the count of genuinely-new tuples. Typed refusal when
    /// domains or arities disagree. Bare `&[u32]` cannot enter.
    ///
    /// **Coexisting-arena boundary:** two owned sinks/rows; mint-checked
    /// [`Admission::prove_shared`].
    pub fn absorb(&mut self, rows: &ExecRows) -> Result<usize, Denial> {
        if self.arity != rows.arity() {
            return Err(Denial::ArityMismatch {
                expected: self.arity.get(),
                got: rows.arity().get(),
            });
        }
        Admission::prove_shared(
            self.domain.arena_id(),
            self.domain.epoch(),
            rows.domain().arena_id(),
            rows.domain().epoch(),
        )?;
        // Cover the source extent so the sink's Domain stamp remains a
        // sound upper bound on every packed code it holds.
        if rows.domain().extent() > self.domain.extent() {
            self.domain = rows.domain();
        }
        let mut new = 0;
        for r in 0..rows.len() {
            if self.admit_row(rows.row(r)) {
                new += 1;
            }
        }
        Ok(new)
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
        let mut rows = Rows::new_in(Arity::try_new(2).expect("test arity 2"), &f);
        for (a, b) in stamps {
            rows.push_row(&[a, b]).expect("lawful push");
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
        let e = ExecRows::admit(&edges, &f).expect("lawful admit");
        // one TC step: join edge(x,y) with edge(y,z) on y, emit (x, z).
        let step = e
            .join_project(&e, 1, 0, &[(Side::Left, 0), (Side::Right, 1)])
            .expect("lawful join");
        // Resolve the produced code pairs back to values and compare to
        // the hand oracle {(1,3),(2,4)}.
        let mut got: Vec<(i64, i64)> = Vec::new();
        for r in 0..step.len() {
            let x = decode_int(step.resolve_cell(&f, r, 0).expect("lawful resolve"));
            let z = decode_int(step.resolve_cell(&f, r, 1).expect("lawful resolve"));
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
        let e = ExecRows::admit(&rows, &f).expect("lawful admit");
        let mut dedup = ExecDedup::new(e.domain(), Arity::try_new(2).expect("test arity 2"));
        let new = dedup.absorb(&e).expect("lawful absorb");
        assert_eq!(new, 2, "the duplicate (1,2) must not be a new tuple");
        assert_eq!(dedup.len(), 2);
        // A second absorb of the same rows adds nothing.
        assert_eq!(dedup.absorb(&e).expect("lawful absorb"), 0);
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
        let e = ExecRows::admit(&edges, &f).expect("lawful admit");
        // Two TC steps + dedup -- the recombination hot loop.
        let step1 = e
            .join_project(&e, 1, 0, &[(Side::Left, 0), (Side::Right, 1)])
            .expect("lawful join");
        let step2 = step1
            .join_project(&e, 1, 0, &[(Side::Left, 0), (Side::Right, 1)])
            .expect("lawful join");
        let mut dedup = ExecDedup::new(e.domain(), Arity::try_new(2).expect("test arity 2"));
        dedup.absorb(&e).expect("lawful absorb");
        dedup.absorb(&step1).expect("lawful absorb");
        dedup.absorb(&step2).expect("lawful absorb");
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
        let e = ExecRows::admit(&rows, &f).expect("lawful admit");
        assert_eq!(e.arity(), Arity::try_new(2).expect("test arity 2"));
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
            let e = ExecRows::admit(&edges, &f).expect("lawful admit");
            // Code path: edge(x,y) ⋈_y edge(y,z) → (x, z).
            let step = e
                .join_project(&e, 1, 0, &[(Side::Left, 0), (Side::Right, 1)])
                .expect("lawful join");
            let mut got: Vec<(i64, i64)> = (0..step.len())
                .map(|r| {
                    (
                        decode_int(step.resolve_cell(&f, r, 0).expect("lawful resolve")),
                        decode_int(step.resolve_cell(&f, r, 1).expect("lawful resolve")),
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
        let e = ExecRows::admit(&edges, &f).expect("lawful admit");
        let a = e
            .join_project(&e, 1, 0, &[(Side::Left, 0), (Side::Right, 1)])
            .expect("lawful join");
        let b = e
            .join_project(&e, 1, 0, &[(Side::Left, 0), (Side::Right, 1)])
            .expect("lawful join");
        assert_eq!(
            a.raw(),
            b.raw(),
            "join_project output order is not deterministic"
        );
        // And it is the left-row-major order the fixpoint relies on.
        assert_eq!(a.len(), 9, "3 left rows × 3 right matches on code 9");
    }

    /// Empty projection invents no width — refuse typed, never fabricate Arity::ONE.
    #[test]
    fn join_project_empty_projection_refuses_typed() {
        let mut arena = Arena::new();
        let rows = rows_of(&mut arena, &[(1, 2)]);
        let f = arena.frame();
        let e = ExecRows::admit(&rows, &f).expect("lawful admit");
        assert!(
            matches!(
                e.join_project(&e, 0, 0, &[]),
                Err(Denial::EmptyProjection)
            ),
            "empty out must refuse typed — never invent a width"
        );
    }

    /// The domain guard: joining rows from two DIFFERENT arenas is refused
    /// typed — u32 code identity is only value identity within one arena+epoch.
    #[test]
    fn join_project_across_arenas_refuses_typed() {
        let mut a1 = Arena::new();
        let r1 = rows_of(&mut a1, &[(1, 2)]);
        let f1 = a1.frame();
        let e1 = ExecRows::admit(&r1, &f1).expect("lawful admit");

        let mut a2 = Arena::new();
        let r2 = rows_of(&mut a2, &[(2, 3)]);
        let f2 = a2.frame();
        let e2 = ExecRows::admit(&r2, &f2).expect("lawful admit");

        // Same shape, foreign arena: the codes are incomparable.
        assert!(
            matches!(
                e1.join_project(&e2, 1, 0, &[(Side::Left, 0), (Side::Right, 1)]),
                Err(Denial::ArenaMismatch { .. })
            ),
            "cross-arena join must refuse typed — never panic"
        );
    }

    fn decode_int(bytes: &[u8]) -> i64 {
        match super::super::canonical::decode(bytes).expect("lawful") {
            DataValue::Num(n) => n.as_int().expect("int"),
            other @ (data_value_any!()) => panic!("not an int: {other:?}"),
        }
    }
}
