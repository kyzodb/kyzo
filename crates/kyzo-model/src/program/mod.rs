/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Program IR vocabulary shared across parse and exec.

pub mod aggregate;
pub mod expr;
pub mod op;
pub mod query;
pub mod rule;
pub mod span;
pub mod symbol;

pub use aggregate::{AggrKind, AggrRefuse, Aggregation, parse_aggr};
pub use expr::{BindingPos, Decision, Expr, LazyOp};
pub use op::{OpDecl, resolve_decl};
pub use query::{
    InputRelationHandle, QueryAssertion, QueryOutOptions, RelationOp, ReturnMutation, SortDir,
    WriteValidity,
};
pub use rule::{
    Comment, DeltaAxis, EmptyRuleSet, EntryHeadNotExplicitlyDefined, FixedRuleApply, FixedRuleArg,
    FixedRuleHandle, HeadAggrSlot, HeadColumn, InputAtom, InputInlineRule, InputInlineRulesOrFixed,
    InputProgram, InputRuleApplyAtom, NoEntry, SearchInput, TempSymbGen, Trivia, Unification,
    ValidityClause, aligned_head, collect_trivia_anchors, shares_a_line_with_preceding_content,
    split_head_columns,
};
pub use span::SourceSpan;
pub use symbol::{Symbol, SymbolKind};
