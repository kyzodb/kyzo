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
