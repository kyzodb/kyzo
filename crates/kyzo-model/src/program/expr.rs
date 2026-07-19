//! Expression AST vocabulary shared by parse and exec.
//!
//! Holds the language tree only. Evaluation and builtin bodies live elsewhere.

/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): `OpDecl` carries a `deterministic` flag that gates constant
 * folding; deserialized expressions re-prove their arity; the eval loop
 * and serde visitors return errors where the original panicked.
 * `eval_to_const` also accepts closed deterministic constructs the
 * original refused (e.g. a Cond of constants) — accept-more only; no
 * valid original program changes meaning.
 *
 * DEMOLITION (story #301): the second, independently-written evaluator
 * (bytecode compile/interpret path) is deleted outright. `Expr::eval`
 * (the tree walker) is the one remaining expression-semantics owner.
 * Every former bytecode call site across the engine is left broken,
 * intentionally: T2 rebuilds each one on `Expr::eval` directly.
 */

//! Expression tree and lazy connectives. [`OpDecl`] is declaration-only.

use std::cmp::{max, min};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Display, Formatter};
use std::mem;

use miette::{Diagnostic, Result, bail};
use serde::de::{Error, Visitor};
use serde::{Deserializer, Serializer};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::SourceSpan;
use crate::program::op::{self as opdecl, OpDecl, resolve_decl};
use crate::program::symbol::Symbol;
use crate::value::{DataValue, LARGEST_UTF_CHAR, ScanBound};
use crate::data_value_any;

#[derive(Error, Diagnostic, Debug)]
#[error("The variable '{0}' is unbound")]
#[diagnostic(code(eval::unbound))]
pub struct UnboundVariableError(pub String, #[label] pub SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("The tuple bound by variable '{0}' is too short: index is {1}, length is {2}")]
#[diagnostic(help("This is definitely a bug. Please report it."))]
#[diagnostic(code(eval::tuple_too_short))]
pub struct TupleTooShortError(
    pub String,
    pub usize,
    pub usize,
    #[label] pub SourceSpan,
);

/// Deserialized data applied an op to an argument count the op does not
/// accept. Rejected at the serde boundary so no op body ever sees it.
#[derive(Error, Diagnostic, Debug)]
#[error("Deserialized program applies '{0}' to {1} argument(s); it requires {2}")]
#[diagnostic(code(eval::deserialized_arity_mismatch))]
struct ArityMismatchError(&'static str, usize, String);

/// Binding position in a tuple — unresolved until [`Expr::fill_binding_indices`],
/// then resolved. Replaces `Option<usize>` so the phase is a named sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum BindingPos {
    Unresolved,
    Resolved(usize),
}

impl BindingPos {
    pub fn resolved(self) -> Option<usize> {
        match self {
            BindingPos::Resolved(i) => Some(i),
            BindingPos::Unresolved => None,
        }
    }
}

/// The language's expression tree: a KyzoScript expression as parsed,
/// evaluable to a [`DataValue`] against a tuple of bindings.
#[derive(Clone, PartialEq, Eq, serde_derive::Serialize)]
pub enum Expr {
    /// Binding to variables
    Binding {
        /// The variable name to bind
        var: Symbol,
        /// When executing in the context of a tuple, the position of the
        /// binding within the tuple — [`BindingPos::Unresolved`] between
        /// parsing and [`Expr::fill_binding_indices`], then
        /// [`BindingPos::Resolved`].
        tuple_pos: BindingPos,
    },
    /// Constant expression containing a value
    Const {
        /// The value
        val: DataValue,
        /// Source span
        #[serde(skip)]
        span: SourceSpan,
    },
    /// Function application
    Apply {
        /// OpDecl representing the function to apply
        op: OpDecl,
        /// Arguments to the application
        args: Box<[Expr]>,
        /// Source span
        #[serde(skip)]
        span: SourceSpan,
    },
    /// Unbound function application
    UnboundApply {
        /// OpDecl representing the function to apply
        op: SmartString<LazyCompact>,
        /// Arguments to the application
        args: Box<[Expr]>,
        /// Source span
        #[serde(skip)]
        span: SourceSpan,
    },
    /// Conditional expressions
    Cond {
        /// Conditional clauses, the first expression in each tuple should
        /// evaluate to a boolean
        clauses: Vec<(Expr, Expr)>,
        /// Source span
        #[serde(skip)]
        span: SourceSpan,
    },
    /// A short-circuiting connective: arguments evaluate left to right,
    /// and evaluation STOPS at the deciding argument — later arguments are
    /// never touched, so their errors never fire. This is what makes the
    /// guard idiom (`k != 0 && v / k > 1`, `maybe ~ fallback`) a language
    /// guarantee instead of an accident of filter splitting.
    Lazy {
        /// Which connective.
        op: LazyOp,
        /// Arguments, evaluated left to right.
        args: Box<[Expr]>,
        /// Source span
        #[serde(skip)]
        span: SourceSpan,
    },
}

/// The short-circuiting connectives. `And` and `Or` require every argument
/// they evaluate to be a boolean; `Coalesce` takes any values and yields
/// the first non-null one.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde_derive::Serialize,
    serde_derive::Deserialize,
)]
pub enum LazyOp {
    /// True iff every argument is true; false decides at the first false.
    And,
    /// True iff any argument is true; true decides at the first true.
    Or,
    /// The first non-null argument, or null if all are.
    Coalesce,
}

/// What one evaluated argument means to a [`LazyOp`].
pub enum Decision {
    /// This argument ends evaluation with this value; later arguments are
    /// dead and are never touched.
    Decided(DataValue),
    /// This argument is inert; evaluation moves to the next.
    Continue,
    /// This argument's type is refused by the connective (reported by the
    /// caller with the argument's span).
    Refused,
}

impl LazyOp {
    /// The identity element: the value of the empty form, and of any form
    /// whose arguments are all inert.
    pub fn identity(self) -> DataValue {
        match self {
            LazyOp::And => DataValue::from(true),
            LazyOp::Or => DataValue::from(false),
            LazyOp::Coalesce => DataValue::Null,
        }
    }
    /// THE truth table. Every machine that evaluates a lazy connective —
    /// the tree evaluator and the constant folder — derives from this
    /// single declaration.
    pub fn decide(self, val: &DataValue) -> Decision {
        match self {
            LazyOp::And => match val.get_bool() {
                Some(true) => Decision::Continue,
                Some(false) => Decision::Decided(DataValue::from(false)),
                None => Decision::Refused,
            },
            LazyOp::Or => match val.get_bool() {
                Some(false) => Decision::Continue,
                Some(true) => Decision::Decided(DataValue::from(true)),
                None => Decision::Refused,
            },
            LazyOp::Coalesce => {
                if *val == DataValue::Null {
                    Decision::Continue
                } else {
                    Decision::Decided(val.clone())
                }
            }
        }
    }
}

/// Wire twin of [`Expr`]: what serde may construct before the arity law has
/// been re-proven. `args` recurse through [`Expr`]'s own `Deserialize`, so
/// children are already checked by the time a node is converted; only the
/// node's own application needs proving here. Field-for-field identical to
/// the real enum, so the serialized format is unchanged from the derived one.
#[derive(serde_derive::Deserialize)]
enum ExprDe {
    Binding {
        var: Symbol,
        tuple_pos: BindingPos,
    },
    Const {
        val: DataValue,
        #[serde(skip)]
        span: SourceSpan,
    },
    Apply {
        op: OpDecl,
        args: Box<[Expr]>,
        #[serde(skip)]
        span: SourceSpan,
    },
    UnboundApply {
        op: SmartString<LazyCompact>,
        args: Box<[Expr]>,
        #[serde(skip)]
        span: SourceSpan,
    },
    Cond {
        clauses: Vec<(Expr, Expr)>,
        #[serde(skip)]
        span: SourceSpan,
    },
    Lazy {
        op: LazyOp,
        args: Box<[Expr]>,
        #[serde(skip)]
        span: SourceSpan,
    },
}

impl ExprDe {
    fn into_checked(self) -> std::result::Result<Expr, ArityMismatchError> {
        Ok(match self {
            ExprDe::Binding { var, tuple_pos } => Expr::Binding { var, tuple_pos },
            ExprDe::Const { val, span } => Expr::Const { val, span },
            ExprDe::Apply { op, args, span } => {
                if !op.arity_matches(args.len()) {
                    return Err(ArityMismatchError(
                        op.name,
                        args.len(),
                        op.arity_requirement(),
                    ));
                }
                Expr::Apply { op, args, span }
            }
            ExprDe::UnboundApply { op, args, span } => Expr::UnboundApply { op, args, span },
            ExprDe::Cond { clauses, span } => Expr::Cond { clauses, span },
            ExprDe::Lazy { op, args, span } => Expr::Lazy { op, args, span },
        })
    }
}

impl<'de> serde::Deserialize<'de> for Expr {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // The parser proves arity at build time; this boundary re-proves it
        // for data, so no op body can be entered with too few arguments no
        // matter where the expression came from.
        ExprDe::deserialize(deserializer)?
            .into_checked()
            .map_err(D::Error::custom)
    }
}

impl Debug for Expr {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl Display for Expr {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Expr::Binding { var, .. } => {
                write!(f, "{}", var.name)
            }
            Expr::Const { val, .. } => {
                write!(f, "{val}")
            }
            Expr::Apply { op, args, .. } => {
                // Every op name is `OP_`-prefixed by construction
                // (OpDecl names are OP_-prefixed by construction); fall back
                // to the raw name rather than panic if that ever changes.
                let mut writer = f.debug_tuple(
                    op.name
                        .strip_prefix("OP_")
                        .unwrap_or(op.name)
                        .to_lowercase()
                        .as_str(),
                );
                for arg in args.iter() {
                    writer.field(arg);
                }
                writer.finish()
            }
            Expr::Lazy { op, args, .. } => {
                let name = match op {
                    LazyOp::And => "and",
                    LazyOp::Or => "or",
                    LazyOp::Coalesce => "coalesce",
                };
                let mut writer = f.debug_tuple(name);
                for arg in args.iter() {
                    writer.field(arg);
                }
                writer.finish()
            }
            Expr::UnboundApply { op, args, .. } => {
                let mut writer = f.debug_tuple(op);
                for arg in args.iter() {
                    writer.field(arg);
                }
                writer.finish()
            }
            Expr::Cond { clauses, .. } => {
                let mut writer = f.debug_tuple("cond");
                for (cond, expr) in clauses {
                    writer.field(cond);
                    writer.field(expr);
                }
                writer.finish()
            }
        }
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("No implementation found for op `{1}`")]
#[diagnostic(code(eval::no_implementation))]
pub struct NoImplementationError(#[label] pub SourceSpan, pub String);

#[derive(Debug, Error, Diagnostic)]
#[error("Found value {1:?} where a boolean value is expected")]
#[diagnostic(code(eval::predicate_not_bool))]
pub struct PredicateTypeError(#[label] pub SourceSpan, pub DataValue);

#[derive(Error, Diagnostic, Debug)]
#[error("Evaluation of expression failed")]
#[diagnostic(code(eval::throw))]
pub struct EvalRaisedError(#[label] pub SourceSpan, #[help] pub String);

impl Expr {
    pub fn span(&self) -> SourceSpan {
        match self {
            Expr::Binding { var, .. } => var.span,
            Expr::Const { span, .. }
            | Expr::Apply { span, .. }
            | Expr::Cond { span, .. }
            | Expr::Lazy { span, .. } => *span,
            Expr::UnboundApply { span, .. } => *span,
        }
    }
    pub fn get_binding(&self) -> Option<&Symbol> {
        if let Expr::Binding { var, .. } = self {
            Some(var)
        } else {
            None
        }
    }
    pub fn get_const(&self) -> Option<&DataValue> {
        if let Expr::Const { val, .. } = self {
            Some(val)
        } else {
            None
        }
    }
    pub fn build_equate(exprs: Vec<Expr>, span: SourceSpan) -> Self {
        Expr::Apply {
            op: opdecl::OP_EQ,
            args: exprs.into(),
            span,
        }
    }
    pub fn build_and(exprs: Vec<Expr>, span: SourceSpan) -> Self {
        Expr::Lazy {
            op: LazyOp::And,
            args: exprs.into(),
            span,
        }
    }
    pub fn build_is_in(exprs: Vec<Expr>, span: SourceSpan) -> Self {
        Expr::Apply {
            op: opdecl::OP_IS_IN,
            args: exprs.into(),
            span,
        }
    }
    pub fn negate(self, span: SourceSpan) -> Self {
        Expr::Apply {
            op: opdecl::OP_NEGATE,
            args: Box::new([self]),
            span,
        }
    }
    pub fn to_conjunction(&self) -> Vec<Self> {
        match self {
            Expr::Lazy {
                op: LazyOp::And,
                args,
                ..
            } => args.to_vec(),
            v @ Expr::Binding { .. } | v @ Expr::Const { .. } | v @ Expr::Apply { .. } | v @ Expr::UnboundApply { .. } | v @ Expr::Cond { .. } | v @ Expr::Lazy { .. } => vec![v.clone()],
        }
    }
    pub fn fill_binding_indices(
        &mut self,
        binding_map: &BTreeMap<Symbol, usize>,
    ) -> Result<()> {
        match self {
            Expr::Binding { var, tuple_pos, .. } => {
                #[derive(Debug, Error, Diagnostic)]
                #[error("Cannot find binding {0}")]
                #[diagnostic(code(eval::bad_binding))]
                struct BadBindingError(String, #[label] SourceSpan);

                let found_idx = *binding_map
                    .get(var)
                    .ok_or_else(|| BadBindingError(var.to_string(), var.span))?;
                *tuple_pos = BindingPos::Resolved(found_idx)
            }
            Expr::Const { .. } => {}
            Expr::Apply { args, .. } | Expr::Lazy { args, .. } => {
                for arg in args.iter_mut() {
                    arg.fill_binding_indices(binding_map)?;
                }
            }
            Expr::Cond { clauses, .. } => {
                for (cond, val) in clauses {
                    cond.fill_binding_indices(binding_map)?;
                    val.fill_binding_indices(binding_map)?;
                }
            }
            Expr::UnboundApply { op, span, .. } => {
                bail!(NoImplementationError(*span, op.to_string()));
            }
        }
        Ok(())
    }
    pub fn binding_indices(&self) -> Result<BTreeSet<usize>> {
        let mut ret = BTreeSet::default();
        self.do_binding_indices(&mut ret)?;
        Ok(ret)
    }
    fn do_binding_indices(&self, coll: &mut BTreeSet<usize>) -> Result<()> {
        match self {
            Expr::Binding { tuple_pos, .. } => {
                if let BindingPos::Resolved(idx) = tuple_pos {
                    coll.insert(*idx);
                }
            }
            Expr::Const { .. } => {}
            Expr::Apply { args, .. } | Expr::Lazy { args, .. } => {
                for arg in args.iter() {
                    arg.do_binding_indices(coll)?;
                }
            }
            Expr::Cond { clauses, .. } => {
                for (cond, val) in clauses {
                    cond.do_binding_indices(coll)?;
                    val.do_binding_indices(coll)?;
                }
            }
            Expr::UnboundApply { op, span, .. } => {
                bail!(NoImplementationError(*span, op.to_string()));
            }
        }
        Ok(())
    }
    /// Evaluate the expression to a constant value if possible.
    ///
    /// This is an explicit request for *one evaluation, now* — distinct from
    /// `partial_eval`'s folding, which refuses nondeterministic applications.
    /// A closed expression (no free variables) is honoured even when it
    /// contains nondeterministic ops: `rand_uuid_v4()` as a query option is
    /// asked for once and evaluated once, deliberately, rather than folded
    /// by accident.
    pub fn eval_to_const(mut self) -> Result<DataValue> {
        #[derive(Error, Diagnostic, Debug)]
        #[error("Expression contains unevaluated constant")]
        #[diagnostic(code(eval::not_constant))]
        #[diagnostic(help(
            "A constant is required here, but this expression still refers to a \
             variable and so has no value at parse time."
        ))]
        struct NotConstError(#[label("not a constant")] SourceSpan);

        // The offending construct is this expression itself; capture its span
        // before `partial_eval` folds/rewrites it away.
        let span = self.span();
        self.partial_eval()?;
        if let Expr::Const { val, .. } = self {
            return Ok(val);
        }
        // Closed nondeterministic / unfolder-able forms are evaluated in the
        // engine crate; this seat only returns values already folded to Const.
        bail!(NotConstError(span))
    }
    pub fn partial_eval(&mut self) -> Result<()> {
        if let Expr::Lazy { op, args, span } = self {
            let span = *span;
            let op = *op;
            // Fold left to right and STOP at the first argument that either
            // decides the form or resists folding. A deciding constant
            // prefix folds the whole form without touching later arguments:
            // under short-circuit semantics they are dead code, and their
            // errors must not fire at fold time any more than at runtime.
            for arg in args.iter_mut() {
                if arg.partial_eval().is_err() {
                    // The argument errors when evaluated. Whether it is
                    // ever evaluated is the deciding prefix's runtime
                    // business, not the folder's: leave it, stop folding.
                    return Ok(());
                }
                let Expr::Const { val, .. } = arg else {
                    // Not statically known; nothing past here can decide
                    // at compile time.
                    return Ok(());
                };
                match op.decide(val) {
                    Decision::Decided(v) => {
                        *self = Expr::Const { val: v, span };
                        return Ok(());
                    }
                    Decision::Continue => {}
                    // A refused constant is a runtime type error on every
                    // row it is reached on; leave it for the evaluator to
                    // report with its span rather than folding a lie.
                    Decision::Refused => return Ok(()),
                }
            }
            // Every argument folded inert: the form IS its identity.
            *self = Expr::Const {
                val: op.identity(),
                span,
            };
            return Ok(());
        }
        if let Expr::Apply { op, args, span } = self {
            let span = *span;
            let mut all_evaluated = true;
            for arg in args.iter_mut() {
                arg.partial_eval()?;
                all_evaluated = all_evaluated && matches!(arg, Expr::Const { .. });
            }
            // Fold only what is a fact at compile time. A nondeterministic
            // application over constants — `rand_float()`, `now()` — is NOT
            // a constant: it evaluates per row at runtime. The CozoDB
            // original folded these by accident, freezing e.g. `rand_float()`
            // into a single per-query number. If per-query-constant semantics
            // is ever wanted for `now()`, that is an engine decision to make
            // deliberately later, not a side effect of folding.
            // Deterministic Apply folding requires the engine apply door;
            // leave the Apply here for the engine folder/evaluator.
            let _ = (all_evaluated, op, span);
            // nested not's can accumulate during conversion to normal form
            if let Expr::Apply {
                op: op1,
                args: arg1,
                ..
            } = self
                && op1.name == opdecl::OP_NEGATE.name
                && let Some(Expr::Apply {
                    op: op2,
                    args: arg2,
                    ..
                }) = arg1.first()
                && op2.name == opdecl::OP_NEGATE.name
            {
                let mut new_self = arg2[0].clone();
                mem::swap(self, &mut new_self);
            }
        }
        Ok(())
    }
    pub fn bindings(&self) -> Result<BTreeSet<Symbol>> {
        let mut ret = BTreeSet::new();
        self.collect_bindings(&mut ret)?;
        Ok(ret)
    }
    pub fn collect_bindings(&self, coll: &mut BTreeSet<Symbol>) -> Result<()> {
        match self {
            Expr::Binding { var, .. } => {
                coll.insert(var.clone());
            }
            Expr::Const { .. } => {}
            Expr::Apply { args, .. } | Expr::Lazy { args, .. } => {
                for arg in args.iter() {
                    arg.collect_bindings(coll)?;
                }
            }
            Expr::Cond { clauses, .. } => {
                for (cond, val) in clauses {
                    cond.collect_bindings(coll)?;
                    val.collect_bindings(coll)?;
                }
            }
            Expr::UnboundApply { op, span, .. } => {
                bail!(NoImplementationError(*span, op.to_string()));
            }
        }
        Ok(())
    }

    pub fn extract_bound(&self, target: &Symbol) -> Result<ValueRange> {
        Ok(match self {
            Expr::Binding { .. } | Expr::Const { .. } | Expr::Cond { .. } | Expr::Lazy { .. } => {
                ValueRange::default()
            }
            Expr::Apply { op, args, .. } => match op.name {
                n if n == opdecl::OP_GE.name || n == opdecl::OP_GT.name => {
                    if let Some(symb) = args[0].get_binding()
                        && let Some(val) = args[1].get_const()
                        && target == symb
                    {
                        let tar_val = match val.get_int() {
                            Some(i) => DataValue::from(i),
                            None => val.clone(),
                        };
                        return Ok(ValueRange::lower_bound(tar_val));
                    }
                    if let Some(symb) = args[1].get_binding()
                        && let Some(val) = args[0].get_const()
                        && target == symb
                    {
                        let tar_val = match val.get_float() {
                            Some(i) => DataValue::from(i),
                            None => val.clone(),
                        };
                        return Ok(ValueRange::upper_bound(tar_val));
                    }
                    ValueRange::default()
                }
                n if n == opdecl::OP_LE.name || n == opdecl::OP_LT.name => {
                    if let Some(symb) = args[0].get_binding()
                        && let Some(val) = args[1].get_const()
                        && target == symb
                    {
                        let tar_val = match val.get_float() {
                            Some(i) => DataValue::from(i),
                            None => val.clone(),
                        };

                        return Ok(ValueRange::upper_bound(tar_val));
                    }
                    if let Some(symb) = args[1].get_binding()
                        && let Some(val) = args[0].get_const()
                        && target == symb
                    {
                        let tar_val = match val.get_int() {
                            Some(i) => DataValue::from(i),
                            None => val.clone(),
                        };

                        return Ok(ValueRange::lower_bound(tar_val));
                    }
                    ValueRange::default()
                }
                n if n == opdecl::OP_STARTS_WITH.name => {
                    if let Some(symb) = args[0].get_binding()
                        && let Some(val) = args[1].get_const()
                        && target == symb
                    {
                        let s = val.get_str().ok_or_else(|| {
                            #[derive(Debug, Error, Diagnostic)]
                            #[error("Cannot prefix scan with {0:?}")]
                            #[diagnostic(code(eval::bad_string_range_scan))]
                            #[diagnostic(help("A string argument is required"))]
                            struct StrRangeScanError(DataValue, #[label] SourceSpan);

                            StrRangeScanError(val.clone(), symb.span)
                        })?;
                        let lower = ScanBound::Value(DataValue::from(s));
                        let mut upper = s.to_string();
                        upper.push(LARGEST_UTF_CHAR);
                        let upper = ScanBound::Value(DataValue::Str(upper));
                        return Ok(ValueRange::new(lower, upper));
                    }
                    ValueRange::default()
                }
                _ => ValueRange::default(),
            },
            Expr::UnboundApply { op, span, .. } => {
                bail!(NoImplementationError(*span, op.to_string()));
            }
        })
    }
    pub fn get_variables(&self) -> Result<BTreeSet<String>> {
        let mut ret = BTreeSet::new();
        self.do_get_variables(&mut ret)?;
        Ok(ret)
    }
    fn do_get_variables(&self, coll: &mut BTreeSet<String>) -> Result<()> {
        match self {
            Expr::Binding { var, .. } => {
                coll.insert(var.to_string());
            }
            Expr::Const { .. } => {}
            Expr::Apply { args, .. } | Expr::Lazy { args, .. } => {
                for arg in args.iter() {
                    arg.do_get_variables(coll)?;
                }
            }
            Expr::Cond { clauses, .. } => {
                for (cond, act) in clauses.iter() {
                    cond.do_get_variables(coll)?;
                    act.do_get_variables(coll)?;
                }
            }
            Expr::UnboundApply { op, span, .. } => {
                bail!(NoImplementationError(*span, op.to_string()));
            }
        }
        Ok(())
    }
    pub fn to_var_list(&self) -> Result<Vec<SmartString<LazyCompact>>> {
        #[derive(Error, Diagnostic, Debug)]
        #[error("Invalid fields specification: {0}")]
        #[diagnostic(code(parser::invalid_fields))]
        #[diagnostic(help("A fields specification must be a variable or a list of variables."))]
        struct InvalidFieldsError(
            String,
            #[label("not a variable or list of variables")] SourceSpan,
        );

        match self {
            Expr::Apply { op, args, span } => {
                if op.name != opdecl::OP_LIST.name {
                    Err(
                        InvalidFieldsError(format!("expected a list, got `{}`", op.name), *span)
                            .into(),
                    )
                } else {
                    let mut collected = vec![];
                    for field in args.iter() {
                        match field {
                            Expr::Binding { var, .. } => collected.push(var.name.clone()),
                            Expr::Const { .. } | Expr::Apply { .. } | Expr::UnboundApply { .. } | Expr::Cond { .. } | Expr::Lazy { .. } => {
                                return Err(InvalidFieldsError(
                                    format!("`{field}` is not a plain variable"),
                                    field.span(),
                                )
                                .into());
                            }
                        }
                    }
                    Ok(collected)
                }
            }
            Expr::Binding { var, .. } => Ok(vec![var.name.clone()]),
            Expr::Const { .. } | Expr::UnboundApply { .. } | Expr::Cond { .. } | Expr::Lazy { .. } => Err(InvalidFieldsError(
                format!("`{self}` is not a variable or list"),
                self.span(),
            )
            .into()),
        }
    }
}

pub fn compute_bounds(
    filters: &[Expr],
    symbols: &[Symbol],
) -> Result<(Vec<ScanBound>, Vec<ScanBound>)> {
    let mut lowers = vec![];
    let mut uppers = vec![];
    for current in symbols {
        let mut cur_bound = ValueRange::default();
        for filter in filters {
            let nxt = filter.extract_bound(current)?;
            cur_bound = cur_bound.merge(nxt);
        }
        lowers.push(cur_bound.lower);
        uppers.push(cur_bound.upper);
    }

    Ok((lowers, uppers))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValueRange {
    pub(crate) lower: ScanBound,
    pub(crate) upper: ScanBound,
}

impl ValueRange {
    fn merge(self, other: Self) -> Self {
        let lower = max(self.lower, other.lower);
        let upper = min(self.upper, other.upper);
        if lower > upper {
            Self::null()
        } else {
            Self { lower, upper }
        }
    }
    /// The provably-empty range: lower past upper, so the scan visits
    /// nothing (`Greatest > Least` at the key level too).
    fn null() -> Self {
        Self {
            lower: ScanBound::Greatest,
            upper: ScanBound::Least,
        }
    }
    fn new(lower: ScanBound, upper: ScanBound) -> Self {
        Self { lower, upper }
    }
    fn lower_bound(val: DataValue) -> Self {
        Self {
            lower: ScanBound::Value(val),
            upper: ScanBound::Greatest,
        }
    }
    fn upper_bound(val: DataValue) -> Self {
        Self {
            lower: ScanBound::Least,
            upper: ScanBound::Value(val),
        }
    }
}

impl Default for ValueRange {
    fn default() -> Self {
        Self {
            lower: ScanBound::Least,
            upper: ScanBound::Greatest,
        }
    }
}
