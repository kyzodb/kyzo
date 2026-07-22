/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): query-output options vocabulary seated in kyzo-model.
 * `WriteValidity` is declaration-only here — `PerRow` carries the full
 * `Expr` as data; row evaluation and terminal-tick re-proof live at the
 * write boundary in kyzo-core (`resolve_write_validity` via `eval_expr`).
 */

//! Query-output options vocabulary: what a query asserts, mutates, and returns.
//!
//! Also seats the Candidates&lt;K&gt; seam vocabulary (decisions.md seat 14) that
//! the program-AST search atom lowers onto as an ordinary relation.

use std::fmt::{Debug, Display, Formatter};

use crate::SourceSpan;
use crate::program::expr::Expr;
use crate::program::symbol::Symbol;
use crate::schema::relation::StoredRelationMetadata;
use crate::value::ValidityTs;

// ─────────────────────────────────────────────────────────────────────────
// Candidates<K> seam (seat 14) — search atom is an ordinary relation here
// ─────────────────────────────────────────────────────────────────────────

/// Output-cardinality bound K on the Candidates seam.
///
/// The Unconstructible law bounds **output cardinality only** — never
/// scoring work. Exact full-scan top-k is legal under cost refuse. K is a
/// body-bound [`Expr`] so a correlated subquery may supply it, not only a
/// script `$param`.
#[derive(Debug, Clone, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct CandidatesBound {
    pub k: Expr,
}

impl CandidatesBound {
    pub fn new(k: Expr) -> Self {
        Self { k }
    }
}

/// Seat 14 selection contract for Candidates membership.
///
/// Exact membership is DST bit-identical. Approximate is
/// benchmark-qualified, not recall-guaranteed — sealed under bench law with
/// typed provenance on the emitted relation.
#[derive(Debug, Copy, Clone, Eq, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum SelectionContract {
    Exact,
    Approximate,
}

/// A `:assert none` / `:assert some` clause: the query fails unless its
/// result set is empty / non-empty.
#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum QueryAssertion {
    AssertNone(#[serde(skip, default = "SourceSpan::empty")] SourceSpan),
    AssertSome(#[serde(skip, default = "SourceSpan::empty")] SourceSpan),
}

/// Whether a mutating query reports the mutated rows back (`:returning`).
#[derive(Debug, Copy, Clone, Eq, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum ReturnMutation {
    NotReturning,
    Returning,
}

/// Sort direction in an `:order` clause.
#[derive(Debug, Copy, Clone, Eq, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum SortDir {
    Asc,
    Dsc,
}

/// What a query does to its output stored relation.
#[derive(Debug, Copy, Clone, Eq, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum RelationOp {
    Create,
    Replace,
    Put,
    Insert,
    Update,
    Rm,
    Delete,
    Ensure,
    EnsureNot,
}

/// The valid-time coordinate a mutation's rows are asserted at — the write
/// side's `@` clause. There is no system-time counterpart here by design:
/// the system coordinate is always the committing transaction's own
/// engine-minted stamp; a script has no syntax to set it, which is what
/// keeps "system time" meaning "when the database learned this" rather than
/// something a writer can forge.
#[derive(Debug, Clone, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum WriteValidity {
    /// No `@` clause: every row lands at the transaction's own system
    /// stamp — byte-for-byte the pre-`@` behavior.
    Now,
    /// `@ <constant>`: one valid instant for every row this mutation
    /// writes, resolved once at parse time exactly like the read side's
    /// single-coordinate `@`.
    Fixed(ValidityTs),
    /// `@ <expr over one of this mutation's own output columns>`: each row
    /// supplies its own valid instant, extracted per row like any other
    /// column — the backfill/import case, where every row carries its own
    /// timestamp.
    PerRow(Expr),
}

/// The output stored relation as the query *declares* it: name, declared
/// schema, and which head bindings feed the key and non-key columns.
#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct InputRelationHandle {
    pub name: Symbol,
    pub metadata: StoredRelationMetadata,
    pub key_bindings: Vec<Symbol>,
    pub dep_bindings: Vec<Symbol>,
    #[serde(skip, default = "SourceSpan::empty")]
    pub span: SourceSpan,
}

impl InputRelationHandle {
    /// Bind each schema column name as a zero-span symbol — the one harness
    /// door for metadata→handle minting (hostile/filter index tests).
    pub fn from_metadata(name: &str, metadata: StoredRelationMetadata) -> Self {
        let key_bindings = metadata
            .keys
            .iter()
            .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
            .collect();
        let dep_bindings = metadata
            .non_keys
            .iter()
            .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
            .collect();
        Self {
            name: Symbol::new(name, SourceSpan(0, 0)),
            metadata,
            key_bindings,
            dep_bindings,
            span: SourceSpan(0, 0),
        }
    }
}

/// The `:option`s of a query: limit/offset, timeout, ordering, the output
/// relation (if the query writes one), and assertions.
///
/// Fields are public: the parser assembles these incrementally and the
/// runtime reads them piecemeal; they carry no cross-field invariant that a
/// constructor could prove.
#[derive(Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct QueryOutOptions {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Terminate query with an error if it exceeds this many seconds.
    pub timeout: Option<f64>,
    /// Sleep after performing the query for this number of seconds. Ignored in WASM.
    pub sleep: Option<f64>,
    pub sorters: Vec<(Symbol, SortDir)>,
    pub store_relation: Option<(
        InputRelationHandle,
        RelationOp,
        ReturnMutation,
        WriteValidity,
    )>,
    pub assertion: Option<QueryAssertion>,
}

impl Debug for QueryOutOptions {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl Display for QueryOutOptions {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(l) = self.limit {
            writeln!(f, ":limit {l};")?;
        }
        if let Some(l) = self.offset {
            writeln!(f, ":offset {l};")?;
        }
        if let Some(l) = self.timeout {
            writeln!(f, ":timeout {l};")?;
        }
        for (symb, dir) in &self.sorters {
            write!(f, ":order ")?;
            if *dir == SortDir::Dsc {
                write!(f, "-")?;
            }
            writeln!(f, "{symb};")?;
        }
        if let Some((
            InputRelationHandle {
                name,
                metadata: StoredRelationMetadata { keys, non_keys },
                key_bindings,
                dep_bindings,
                ..
            },
            op,
            return_mutation,
            write_vld,
        )) = &self.store_relation
        {
            if *return_mutation == ReturnMutation::Returning {
                writeln!(f, ":returning")?;
            }
            match op {
                RelationOp::Create => {
                    write!(f, ":create ")?;
                }
                RelationOp::Replace => {
                    write!(f, ":replace ")?;
                }
                RelationOp::Insert => {
                    write!(f, ":insert ")?;
                }
                RelationOp::Put => {
                    write!(f, ":put ")?;
                }
                RelationOp::Update => {
                    write!(f, ":update ")?;
                }
                RelationOp::Rm => {
                    write!(f, ":rm ")?;
                }
                RelationOp::Delete => {
                    write!(f, ":delete ")?;
                }
                RelationOp::Ensure => {
                    write!(f, ":ensure ")?;
                }
                RelationOp::EnsureNot => {
                    write!(f, ":ensure_not ")?;
                }
            }
            write!(f, "{name} {{")?;
            let mut is_first = true;
            for (col, bind) in keys.iter().zip(key_bindings) {
                if is_first {
                    is_first = false
                } else {
                    write!(f, ", ")?;
                }
                write!(f, "{}: {}", col.name, col.typing)?;
                if let Some(generator) = &col.default_gen {
                    write!(f, " default {generator}")?;
                } else {
                    write!(f, " = {bind}")?;
                }
            }
            write!(f, " => ")?;
            let mut is_first = true;
            for (col, bind) in non_keys.iter().zip(dep_bindings) {
                if is_first {
                    is_first = false
                } else {
                    write!(f, ", ")?;
                }
                write!(f, "{}: {}", col.name, col.typing)?;
                if let Some(generator) = &col.default_gen {
                    write!(f, " default {generator}")?;
                } else {
                    write!(f, " = {bind}")?;
                }
            }
            write!(f, "}}")?;
            match write_vld {
                WriteValidity::Now => {}
                WriteValidity::Fixed(ts) => write!(f, " @ {}", ts.raw())?,
                WriteValidity::PerRow(expr) => write!(f, " @ {expr}")?,
            }
            writeln!(f, ";")?;
        }

        if let Some(a) = &self.assertion {
            match a {
                QueryAssertion::AssertNone(_) => {
                    writeln!(f, ":assert none;")?;
                }
                QueryAssertion::AssertSome(_) => {
                    writeln!(f, ":assert some;")?;
                }
            }
        }

        Ok(())
    }
}

impl QueryOutOptions {
    /// No limit/offset/timeout/sorters/store/assertion.
    pub fn empty() -> Self {
        Self {
            limit: None,
            offset: None,
            timeout: None,
            sleep: None,
            sorters: Vec::new(),
            store_relation: None,
            assertion: None,
        }
    }

    /// How many rows evaluation must produce before it may stop early:
    /// `limit + offset` when both are given.
    pub fn num_to_take(&self) -> Option<usize> {
        match (self.limit, self.offset) {
            (None, _) => None,
            (Some(i), None) => Some(i),
            (Some(i), Some(j)) => Some(i + j),
        }
    }
}
