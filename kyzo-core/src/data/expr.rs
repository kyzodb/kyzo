/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): `Op` carries a `deterministic` flag that gates constant
 * folding, deserialized expressions and bytecode re-prove their arity, the
 * eval loop and serde visitors return errors where the original panicked,
 * and `expr2bytecode` (expression compilation) is relocated here from the
 * original's `parse/expr.rs`. `eval_to_const` also accepts closed
 * deterministic constructs the original refused (e.g. a Cond of constants)
 * — accept-more only; no valid original program changes meaning.
 */

//! Expressions, and the operations and bytecode that give them meaning.
//!
//! Three essences live here:
//!
//! - [`Expr`] is the language's expression tree: what a KyzoScript
//!   expression *is* after parsing — bindings, constants, applications, and
//!   conditionals, each carrying its source span.
//! - [`Op`] is a total function over values. Applied to arguments of any
//!   shape it returns a value or an error — never panics; errors are values.
//!   Each op also states, as data, whether it is *deterministic*: whether the
//!   same arguments always yield the same result. That single bit is what
//!   licenses (or forbids) constant folding.
//! - [`Bytecode`] is the expression tree's compiled form: a flat program for
//!   a small stack machine, evaluated without recursion in the row loop.
//!
//! The load-bearing law: by the time an op body runs, its argument slice has
//! the arity the op declared. The parser proves this at build time
//! (`parse/expr.rs`), and the serde boundary re-proves it on deserialization
//! — so op bodies may index `args[N]` below their declared minimum arity
//! without checking, and nothing else may construct an `Apply`.

use std::cmp::{max, min};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Display, Formatter};
use std::mem;

use itertools::Itertools;
use miette::{Diagnostic, Result, bail};
use serde::de::{Error, Visitor};
use serde::{Deserializer, Serializer};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::functions::*;
// Only the `CustomOp` return-type contract needs the schema vocabulary.
use crate::data::relation::NullableColType;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, GermanStr, LARGEST_UTF_CHAR};

/// One instruction of the compiled expression form: a stack-machine program
/// produced by `expr2bytecode` from a validated [`Expr`].
///
/// The compiler guarantees stack discipline (every `Apply` has its `arity`
/// operands on the stack, every jump targets an instruction boundary, the
/// program nets exactly one value). Deserialized bytecode is *claimed*
/// discipline, not proven — so the evaluator checks its stack operations and
/// reports corruption as an error, never a panic.
#[derive(Clone, PartialEq, Eq, serde_derive::Serialize, Debug)]
pub enum Bytecode {
    /// push 1
    Binding {
        var: Symbol,
        tuple_pos: Option<usize>,
    },
    /// push 1
    Const {
        val: DataValue,
        #[serde(skip)]
        span: SourceSpan,
    },
    /// pop n, push 1
    Apply {
        op: &'static Op,
        arity: usize,
        #[serde(skip)]
        span: SourceSpan,
    },
    /// pop 1
    JumpIfFalse {
        jump_to: usize,
        #[serde(skip)]
        span: SourceSpan,
    },
    /// pop 1
    JumpIfTrue {
        jump_to: usize,
        #[serde(skip)]
        span: SourceSpan,
    },
    /// peek: jump keeping the value when it is non-null, else pop it and
    /// fall through (the coalesce step)
    JumpNotNull {
        jump_to: usize,
        #[serde(skip)]
        span: SourceSpan,
    },
    /// unchanged
    Goto {
        jump_to: usize,
        #[serde(skip)]
        span: SourceSpan,
    },
}

/// Wire twin of [`Bytecode`]: what serde may construct before the arity law
/// has been re-proven. Field-for-field identical to the real enum, so the
/// serialized format is unchanged from the derived one.
#[derive(serde_derive::Deserialize)]
enum BytecodeDe {
    Binding {
        var: Symbol,
        tuple_pos: Option<usize>,
    },
    Const {
        val: DataValue,
        #[serde(skip)]
        span: SourceSpan,
    },
    Apply {
        op: &'static Op,
        arity: usize,
        #[serde(skip)]
        span: SourceSpan,
    },
    JumpIfFalse {
        jump_to: usize,
        #[serde(skip)]
        span: SourceSpan,
    },
    JumpIfTrue {
        jump_to: usize,
        #[serde(skip)]
        span: SourceSpan,
    },
    JumpNotNull {
        jump_to: usize,
        #[serde(skip)]
        span: SourceSpan,
    },
    Goto {
        jump_to: usize,
        #[serde(skip)]
        span: SourceSpan,
    },
}

impl BytecodeDe {
    fn into_checked(self) -> std::result::Result<Bytecode, ArityMismatchError> {
        Ok(match self {
            BytecodeDe::Binding { var, tuple_pos } => Bytecode::Binding { var, tuple_pos },
            BytecodeDe::Const { val, span } => Bytecode::Const { val, span },
            BytecodeDe::Apply { op, arity, span } => {
                if !op.arity_matches(arity) {
                    return Err(ArityMismatchError(op.name, arity, op.arity_requirement()));
                }
                Bytecode::Apply { op, arity, span }
            }
            BytecodeDe::JumpIfFalse { jump_to, span } => Bytecode::JumpIfFalse { jump_to, span },
            BytecodeDe::JumpIfTrue { jump_to, span } => Bytecode::JumpIfTrue { jump_to, span },
            BytecodeDe::JumpNotNull { jump_to, span } => Bytecode::JumpNotNull { jump_to, span },
            BytecodeDe::Goto { jump_to, span } => Bytecode::Goto { jump_to, span },
        })
    }
}

impl<'de> serde::Deserialize<'de> for Bytecode {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        BytecodeDe::deserialize(deserializer)?
            .into_checked()
            .map_err(D::Error::custom)
    }
}

#[derive(Error, Diagnostic, Debug)]
#[error("The variable '{0}' is unbound")]
#[diagnostic(code(eval::unbound))]
pub(crate) struct UnboundVariableError(pub(crate) String, #[label] pub(crate) SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("The tuple bound by variable '{0}' is too short: index is {1}, length is {2}")]
#[diagnostic(help("This is definitely a bug. Please report it."))]
#[diagnostic(code(eval::tuple_too_short))]
pub(crate) struct TupleTooShortError(
    pub(crate) String,
    pub(crate) usize,
    pub(crate) usize,
    #[label] pub(crate) SourceSpan,
);

/// A bytecode program that violates stack discipline or jumps out of range.
/// Compiled programs cannot produce this; it is reachable only through
/// deserialized bytecode — which the law says must error, never abort.
#[derive(Error, Diagnostic, Debug)]
#[error("Corrupt bytecode: {0}")]
#[diagnostic(code(eval::corrupt_bytecode))]
#[diagnostic(help("This is definitely a bug or corrupt stored program. Please report it."))]
struct CorruptBytecodeError(&'static str);

/// Deserialized data applied an op to an argument count the op does not
/// accept. Rejected at the serde boundary so no op body ever sees it.
#[derive(Error, Diagnostic, Debug)]
#[error("Deserialized program applies '{0}' to {1} argument(s); it requires {2}")]
#[diagnostic(code(eval::deserialized_arity_mismatch))]
struct ArityMismatchError(&'static str, usize, String);

/// Compile a validated [`Expr`] into a [`Bytecode`] program.
///
/// Relocated from the CozoDB original's `parse/expr.rs`: compiling an
/// expression to its stack-machine form is the expression's own domain, not
/// the parser's — moving it here removes the data tier's only forward
/// dependency on the parse tier.
///
/// The compiler establishes the stack discipline the evaluator relies on:
/// every `Apply` is emitted after exactly its `arity` operand programs,
/// every jump targets an instruction boundary, and the whole program nets
/// exactly one value.
pub(crate) fn expr2bytecode(expr: &Expr, collector: &mut Vec<Bytecode>) -> Result<()> {
    match expr {
        Expr::Binding { var, tuple_pos } => collector.push(Bytecode::Binding {
            var: var.clone(),
            tuple_pos: *tuple_pos,
        }),
        Expr::Const { val, span } => collector.push(Bytecode::Const {
            val: val.clone(),
            span: *span,
        }),
        Expr::Apply { op, args, span } => {
            let arity = args.len();
            for arg in args.iter() {
                expr2bytecode(arg, collector)?;
            }
            collector.push(Bytecode::Apply {
                op,
                arity,
                span: *span,
            })
        }
        Expr::Lazy { op, args, span } => {
            // Short-circuit via jumps, derived from the connective's
            // declaration: a boolean-decided form (`deciding_bool`) jumps
            // out on its deciding value and nets the decision constant;
            // Coalesce jumps out keeping the first non-null value itself.
            match op.deciding_bool() {
                Some(deciding) => {
                    // Each argument's jump carries THAT ARGUMENT's span:
                    // the type refusal for a non-boolean argument reports
                    // where the offending argument is, exactly as the tree
                    // evaluator's `Decision::Refused` arm does.
                    let jump = |jump_to: usize, span: SourceSpan| -> Bytecode {
                        if deciding {
                            Bytecode::JumpIfTrue { jump_to, span }
                        } else {
                            Bytecode::JumpIfFalse { jump_to, span }
                        }
                    };
                    let mut decided_jumps = vec![];
                    for arg in args.iter() {
                        expr2bytecode(arg, collector)?;
                        collector.push(jump(0, arg.span()));
                        decided_jumps.push((collector.len() - 1, arg.span()));
                    }
                    collector.push(Bytecode::Const {
                        val: op.identity(),
                        span: *span,
                    });
                    collector.push(Bytecode::Goto {
                        jump_to: collector.len() + 2,
                        span: *span,
                    });
                    let decided_target = collector.len();
                    for (pos, arg_span) in decided_jumps {
                        collector[pos] = jump(decided_target, arg_span);
                    }
                    collector.push(Bytecode::Const {
                        val: DataValue::from(deciding),
                        span: *span,
                    });
                }
                None => {
                    let mut done_jumps = vec![];
                    for arg in args.iter() {
                        expr2bytecode(arg, collector)?;
                        collector.push(Bytecode::JumpNotNull {
                            jump_to: 0,
                            span: *span,
                        });
                        done_jumps.push(collector.len() - 1);
                    }
                    collector.push(Bytecode::Const {
                        val: op.identity(),
                        span: *span,
                    });
                    let end = collector.len();
                    for pos in done_jumps {
                        collector[pos] = Bytecode::JumpNotNull {
                            jump_to: end,
                            span: *span,
                        };
                    }
                }
            }
        }
        Expr::Cond { clauses, span } => {
            let mut return_jump_pos = vec![];
            for (cond, val) in clauses {
                // +1
                expr2bytecode(cond, collector)?;
                // -1
                collector.push(Bytecode::JumpIfFalse {
                    jump_to: 0,
                    span: *span,
                });
                let false_jump_amend_pos = collector.len() - 1;
                // +1 in this branch
                expr2bytecode(val, collector)?;
                collector.push(Bytecode::Goto {
                    jump_to: 0,
                    span: *span,
                });
                return_jump_pos.push(collector.len() - 1);
                collector[false_jump_amend_pos] = Bytecode::JumpIfFalse {
                    jump_to: collector.len(),
                    span: *span,
                };
            }
            let total_len = collector.len();
            for pos in return_jump_pos {
                collector[pos] = Bytecode::Goto {
                    jump_to: total_len,
                    span: *span,
                }
            }
        }
        Expr::UnboundApply { op, span, .. } => {
            bail!(NoImplementationError(*span, op.to_string()));
        }
    }
    Ok(())
}

pub fn eval_bytecode_pred(
    bytecodes: &[Bytecode],
    bindings: impl AsRef<[DataValue]>,
    stack: &mut Vec<DataValue>,
    span: SourceSpan,
) -> Result<bool> {
    match eval_bytecode(bytecodes, bindings, stack)? {
        DataValue::Bool(b) => Ok(b),
        v => bail!(PredicateTypeError(span, v)),
    }
}

pub fn eval_bytecode(
    bytecodes: &[Bytecode],
    bindings: impl AsRef<[DataValue]>,
    stack: &mut Vec<DataValue>,
) -> Result<DataValue> {
    stack.clear();
    let mut pointer = 0;
    loop {
        if pointer == bytecodes.len() {
            break;
        }
        // Compiled jumps always land on instruction boundaries; a pointer
        // past the end can only come from corrupt deserialized bytecode.
        let current_instruction = bytecodes
            .get(pointer)
            .ok_or(CorruptBytecodeError("jump beyond the end of the program"))?;
        match current_instruction {
            Bytecode::Binding { var, tuple_pos, .. } => match tuple_pos {
                None => {
                    bail!(UnboundVariableError(var.name.to_string(), var.span))
                }
                Some(i) => {
                    let val = bindings
                        .as_ref()
                        .get(*i)
                        .ok_or_else(|| {
                            TupleTooShortError(
                                var.name.to_string(),
                                *i,
                                bindings.as_ref().len(),
                                var.span,
                            )
                        })?
                        .clone();
                    stack.push(val);
                    pointer += 1;
                }
            },
            Bytecode::Const { val, .. } => {
                stack.push(val.clone());
                pointer += 1;
            }
            Bytecode::Apply { op, arity, span } => {
                // Compiler-produced programs always have `arity` operands on
                // the stack here; checked because deserialized bytecode is
                // only claimed, not proven.
                let frame_start = stack.len().checked_sub(*arity).ok_or(CorruptBytecodeError(
                    "application consumes more values than the stack holds",
                ))?;
                let args_frame = &stack[frame_start..];
                let result = apply_op(op, args_frame)
                    .map_err(|err| EvalRaisedError(*span, err.to_string()))?;
                stack.truncate(frame_start);
                stack.push(result);
                pointer += 1;
            }
            Bytecode::JumpIfFalse { jump_to, span } => {
                let val = stack
                    .pop()
                    .ok_or(CorruptBytecodeError("conditional jump on an empty stack"))?;
                let cond = val
                    .get_bool()
                    .ok_or_else(|| PredicateTypeError(*span, val))?;
                if cond {
                    pointer += 1;
                } else {
                    pointer = *jump_to;
                }
            }
            Bytecode::JumpIfTrue { jump_to, span } => {
                let val = stack
                    .pop()
                    .ok_or(CorruptBytecodeError("conditional jump on an empty stack"))?;
                let cond = val
                    .get_bool()
                    .ok_or_else(|| PredicateTypeError(*span, val))?;
                if cond {
                    pointer = *jump_to;
                } else {
                    pointer += 1;
                }
            }
            Bytecode::JumpNotNull { jump_to, .. } => {
                let val = stack
                    .last()
                    .ok_or(CorruptBytecodeError("coalesce jump on an empty stack"))?;
                if *val == DataValue::Null {
                    stack.pop();
                    pointer += 1;
                } else {
                    pointer = *jump_to;
                }
            }
            Bytecode::Goto { jump_to, .. } => {
                pointer = *jump_to;
            }
        }
    }
    // A compiled program nets exactly one value; an empty stack here means
    // the bytecode was corrupt, and corruption is an error, not an abort.
    match stack.pop() {
        Some(val) => Ok(val),
        None => bail!(CorruptBytecodeError("program left no value on the stack")),
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
        /// binding within the tuple.
        ///
        /// Two-phase: `None` between parsing and `fill_binding_indices`,
        /// `Some` afterwards, and evaluation errors on `None`. A typestate
        /// split (unresolved vs. resolved expression) would put that law in
        /// the types; it spans the whole program representation, so it is
        /// deliberately deferred to the program-tier port, not redesigned
        /// here.
        tuple_pos: Option<usize>,
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
        /// Op representing the function to apply
        op: &'static Op,
        /// Arguments to the application
        args: Box<[Expr]>,
        /// Source span
        #[serde(skip)]
        span: SourceSpan,
    },
    /// Unbound function application
    UnboundApply {
        /// Op representing the function to apply
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
pub(crate) enum Decision {
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
    pub(crate) fn identity(self) -> DataValue {
        match self {
            LazyOp::And => DataValue::from(true),
            LazyOp::Or => DataValue::from(false),
            LazyOp::Coalesce => DataValue::Null,
        }
    }
    /// THE truth table. Every machine that evaluates a lazy connective —
    /// the tree evaluator, the constant folder, the bytecode compiler —
    /// derives from this single declaration.
    pub(crate) fn decide(self, val: &DataValue) -> Decision {
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
    /// The boolean that decides the form, when the connective is decided
    /// by a boolean at all (`None` for Coalesce, which is decided by
    /// non-nullness and needs its own jump shape).
    fn deciding_bool(self) -> Option<bool> {
        match self {
            LazyOp::And => Some(false),
            LazyOp::Or => Some(true),
            LazyOp::Coalesce => None,
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
        tuple_pos: Option<usize>,
    },
    Const {
        val: DataValue,
        #[serde(skip)]
        span: SourceSpan,
    },
    Apply {
        op: &'static Op,
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
                // (`define_op!` stringifies the const's own name); fall back
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
pub(crate) struct NoImplementationError(#[label] pub(crate) SourceSpan, pub(crate) String);

#[derive(Debug, Error, Diagnostic)]
#[error("Found value {1:?} where a boolean value is expected")]
#[diagnostic(code(eval::predicate_not_bool))]
pub(crate) struct PredicateTypeError(#[label] pub(crate) SourceSpan, pub(crate) DataValue);

#[derive(Error, Diagnostic, Debug)]
#[error("Evaluation of expression failed")]
#[diagnostic(code(eval::throw))]
pub(crate) struct EvalRaisedError(#[label] pub(crate) SourceSpan, #[help] pub(crate) String);

impl Expr {
    pub(crate) fn compile(&self) -> Result<Vec<Bytecode>> {
        let mut collector = vec![];
        expr2bytecode(self, &mut collector)?;
        Ok(collector)
    }
    pub(crate) fn span(&self) -> SourceSpan {
        match self {
            Expr::Binding { var, .. } => var.span,
            Expr::Const { span, .. }
            | Expr::Apply { span, .. }
            | Expr::Cond { span, .. }
            | Expr::Lazy { span, .. } => *span,
            Expr::UnboundApply { span, .. } => *span,
        }
    }
    pub(crate) fn get_binding(&self) -> Option<&Symbol> {
        if let Expr::Binding { var, .. } = self {
            Some(var)
        } else {
            None
        }
    }
    pub(crate) fn get_const(&self) -> Option<&DataValue> {
        if let Expr::Const { val, .. } = self {
            Some(val)
        } else {
            None
        }
    }
    pub(crate) fn build_equate(exprs: Vec<Expr>, span: SourceSpan) -> Self {
        Expr::Apply {
            op: &OP_EQ,
            args: exprs.into(),
            span,
        }
    }
    pub(crate) fn build_and(exprs: Vec<Expr>, span: SourceSpan) -> Self {
        Expr::Lazy {
            op: LazyOp::And,
            args: exprs.into(),
            span,
        }
    }
    pub(crate) fn build_is_in(exprs: Vec<Expr>, span: SourceSpan) -> Self {
        Expr::Apply {
            op: &OP_IS_IN,
            args: exprs.into(),
            span,
        }
    }
    pub(crate) fn negate(self, span: SourceSpan) -> Self {
        Expr::Apply {
            op: &OP_NEGATE,
            args: Box::new([self]),
            span,
        }
    }
    pub(crate) fn to_conjunction(&self) -> Vec<Self> {
        match self {
            Expr::Lazy {
                op: LazyOp::And,
                args,
                ..
            } => args.to_vec(),
            v => vec![v.clone()],
        }
    }
    pub(crate) fn fill_binding_indices(
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
                *tuple_pos = Some(found_idx)
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
    #[allow(dead_code)]
    pub(crate) fn binding_indices(&self) -> Result<BTreeSet<usize>> {
        let mut ret = BTreeSet::default();
        self.do_binding_indices(&mut ret)?;
        Ok(ret)
    }
    #[allow(dead_code)]
    fn do_binding_indices(&self, coll: &mut BTreeSet<usize>) -> Result<()> {
        match self {
            Expr::Binding { tuple_pos, .. } => {
                if let Some(idx) = tuple_pos {
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
        // Not folded, but closed: evaluate once against the empty tuple.
        // Anything with free variables is genuinely not constant.
        if self.bindings()?.is_empty() {
            return self.eval(&[] as &[DataValue]);
        }
        bail!(NotConstError(span))
    }
    pub(crate) fn partial_eval(&mut self) -> Result<()> {
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
            if all_evaluated && op.deterministic {
                let result = self.eval(vec![])?;
                *self = Expr::Const { val: result, span };
            }
            // nested not's can accumulate during conversion to normal form
            if let Expr::Apply {
                op: op1,
                args: arg1,
                ..
            } = self
                && op1.name == OP_NEGATE.name
                && let Some(Expr::Apply {
                    op: op2,
                    args: arg2,
                    ..
                }) = arg1.first()
                && op2.name == OP_NEGATE.name
            {
                let mut new_self = arg2[0].clone();
                mem::swap(self, &mut new_self);
            }
        }
        Ok(())
    }
    pub(crate) fn bindings(&self) -> Result<BTreeSet<Symbol>> {
        let mut ret = BTreeSet::new();
        self.collect_bindings(&mut ret)?;
        Ok(ret)
    }
    pub(crate) fn collect_bindings(&self, coll: &mut BTreeSet<Symbol>) -> Result<()> {
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
    pub(crate) fn eval(&self, bindings: impl AsRef<[DataValue]>) -> Result<DataValue> {
        match self {
            Expr::Binding { var, tuple_pos, .. } => match tuple_pos {
                None => {
                    bail!(UnboundVariableError(var.name.to_string(), var.span))
                }
                Some(i) => Ok(bindings
                    .as_ref()
                    .get(*i)
                    .ok_or_else(|| {
                        TupleTooShortError(
                            var.name.to_string(),
                            *i,
                            bindings.as_ref().len(),
                            var.span,
                        )
                    })?
                    .clone()),
            },
            Expr::Const { val, .. } => Ok(val.clone()),
            Expr::Apply { op, args, .. } => {
                let args: Box<[DataValue]> = args
                    .iter()
                    .map(|v| v.eval(bindings.as_ref()))
                    .try_collect()?;
                Ok(apply_op(op, &args)
                    .map_err(|err| EvalRaisedError(self.span(), err.to_string()))?)
            }
            Expr::Cond { clauses, .. } => {
                for (cond, val) in clauses {
                    let cond_val = cond.eval(bindings.as_ref())?;
                    let cond_val = cond_val
                        .get_bool()
                        .ok_or_else(|| PredicateTypeError(cond.span(), cond_val))?;

                    if cond_val {
                        return val.eval(bindings.as_ref());
                    }
                }
                Ok(DataValue::Null)
            }
            Expr::Lazy { op, args, .. } => {
                for arg in args.iter() {
                    let v = arg.eval(bindings.as_ref())?;
                    match op.decide(&v) {
                        Decision::Decided(d) => return Ok(d),
                        Decision::Continue => {}
                        Decision::Refused => bail!(PredicateTypeError(arg.span(), v)),
                    }
                }
                Ok(op.identity())
            }
            Expr::UnboundApply { op, span, .. } => {
                bail!(NoImplementationError(*span, op.to_string()));
            }
        }
    }
    pub(crate) fn extract_bound(&self, target: &Symbol) -> Result<ValueRange> {
        Ok(match self {
            Expr::Binding { .. } | Expr::Const { .. } | Expr::Cond { .. } | Expr::Lazy { .. } => {
                ValueRange::default()
            }
            Expr::Apply { op, args, .. } => match op.name {
                n if n == OP_GE.name || n == OP_GT.name => {
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
                n if n == OP_LE.name || n == OP_LT.name => {
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
                n if n == OP_STARTS_WITH.name => {
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
                        let lower = DataValue::from(s);
                        let mut upper = s.to_string();
                        upper.push(LARGEST_UTF_CHAR);
                        let upper = DataValue::Str(GermanStr::from_str(&upper));
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
    pub(crate) fn get_variables(&self) -> Result<BTreeSet<String>> {
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
    pub(crate) fn to_var_list(&self) -> Result<Vec<SmartString<LazyCompact>>> {
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
                if op.name != "OP_LIST" {
                    Err(
                        InvalidFieldsError(format!("expected a list, got `{}`", op.name), *span)
                            .into(),
                    )
                } else {
                    let mut collected = vec![];
                    for field in args.iter() {
                        match field {
                            Expr::Binding { var, .. } => collected.push(var.name.clone()),
                            _ => {
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
            _ => Err(InvalidFieldsError(
                format!("`{self}` is not a variable or list"),
                self.span(),
            )
            .into()),
        }
    }
}

pub(crate) fn compute_bounds(
    filters: &[Expr],
    symbols: &[Symbol],
) -> Result<(Vec<DataValue>, Vec<DataValue>)> {
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
pub(crate) struct ValueRange {
    pub(crate) lower: DataValue,
    pub(crate) upper: DataValue,
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
    fn null() -> Self {
        Self {
            lower: DataValue::Bot,
            upper: DataValue::Bot,
        }
    }
    fn new(lower: DataValue, upper: DataValue) -> Self {
        Self { lower, upper }
    }
    fn lower_bound(val: DataValue) -> Self {
        Self {
            lower: val,
            upper: DataValue::Bot,
        }
    }
    fn upper_bound(val: DataValue) -> Self {
        Self {
            lower: DataValue::Null,
            upper: val,
        }
    }
}

impl Default for ValueRange {
    fn default() -> Self {
        Self {
            lower: DataValue::Null,
            upper: DataValue::Bot,
        }
    }
}

/// A built-in operation: a total function over values. Every field is a
/// compile-time fact about the operation, declared once in `define_op!` so
/// the name, the implementing function, the arity, and the determinism claim
/// cannot drift apart.
#[derive(Clone)]
pub struct Op {
    /// The const's own name (`"OP_ADD"`); `define_op!` stringifies it, so
    /// the `OP_` prefix is guaranteed by construction.
    pub(crate) name: &'static str,
    /// Fewest arguments the op accepts. With `vararg` this is a floor;
    /// without, it is exact.
    pub(crate) min_arity: usize,
    /// Whether the op accepts more than `min_arity` arguments.
    pub(crate) vararg: bool,
    /// Same arguments ⇒ same result. `false` for the clock and randomness
    /// ops; a `false` here forbids constant folding, so the op evaluates
    /// per row at runtime.
    pub(crate) deterministic: bool,
    /// The implementation. Total: returns a value or an error for any
    /// argument slice satisfying the declared arity; never panics.
    pub(crate) inner: fn(&[DataValue]) -> Result<DataValue>,
}

/// The KyzoScript-facing spelling of an op: [`Op::name`] is the
/// screaming-case Rust const identifier (`"OP_ADD"`, guaranteed by
/// `define_op!`'s `stringify!`), never what a user typed or should read.
/// Every place that shows an op's name to a user needs this same transform
/// (strip the `OP_` prefix, lowercase); one shared function so the
/// pretty-printer (`format.rs`) and the op-application NaN checkpoints
/// below can never drift apart on it.
pub(crate) fn op_display_name(name: &'static str) -> String {
    name.strip_prefix("OP_").unwrap_or(name).to_lowercase()
}

/// THE enforced checkpoint every row-path op application routes through —
/// the bytecode VM's `Apply` instruction and the tree-walking `Expr::Apply`
/// arm alike. Calls the op, then refuses a `NaN` float or vector-lane
/// result the same way regardless of whether the op remembered its own
/// `no_nan` guard: the per-op guards in `data/functions.rs` stay as a
/// belt-and-suspenders first line (they carry a more specific domain
/// diagnosis before the result even exists), but no op — present or
/// future — can bypass this backstop and hand a poison value to a caller.
/// Structural absorption for story #62's silent-NaN class.
pub(crate) fn apply_op(op: &Op, args: &[DataValue]) -> Result<DataValue> {
    let result = (op.inner)(args)?;
    if crate::data::functions::result_has_nan(&result) {
        bail!(DomainError {
            op: op_display_name(op.name).into()
        });
    }
    Ok(result)
}

/// Used as `Arc<dyn CustomOp>`
pub trait CustomOp {
    fn name(&self) -> &'static str;
    fn min_arity(&self) -> usize;
    fn vararg(&self) -> bool;
    /// Same arguments ⇒ same result. Defaults to `false`: foreign code is
    /// assumed nondeterministic unless it says otherwise, which only ever
    /// costs a missed folding opportunity, never a wrong one.
    fn deterministic(&self) -> bool {
        false
    }
    fn return_type(&self) -> NullableColType;
    fn call(&self, args: &[DataValue]) -> Result<DataValue>;
}

impl serde::Serialize for &'_ Op {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.name)
    }
}

impl<'de> serde::Deserialize<'de> for &'static Op {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(OpVisitor)
    }
}

struct OpVisitor;

impl<'de> Visitor<'de> for OpVisitor {
    type Value = &'static Op;

    fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("name of the op")
    }

    fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E>
    where
        E: Error,
    {
        // Serialized data is hostile until proven: a name without the `OP_`
        // prefix is a decode error, never a panic (the original unwrapped).
        let name = v
            .strip_prefix("OP_")
            .ok_or_else(|| E::custom(format!("malformed op name in serialized data: {v}")))?
            .to_ascii_lowercase();
        get_op(&name).ok_or_else(|| E::custom(format!("op not found in serialized data: {v}")))
    }
}

impl PartialEq for Op {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Op {}

impl Debug for Op {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

pub(crate) fn get_op(name: &str) -> Option<&'static Op> {
    Some(match name {
        "list" => &OP_LIST,
        "json" => &OP_JSON,
        "set_json_path" => &OP_SET_JSON_PATH,
        "remove_json_path" => &OP_REMOVE_JSON_PATH,
        "parse_json" => &OP_PARSE_JSON,
        "dump_json" => &OP_DUMP_JSON,
        "json_object" => &OP_JSON_OBJECT,
        "is_json" => &OP_IS_JSON,
        "json_to_scalar" => &OP_JSON_TO_SCALAR,
        "add" => &OP_ADD,
        "sub" => &OP_SUB,
        "mul" => &OP_MUL,
        "div" => &OP_DIV,
        "minus" => &OP_MINUS,
        "abs" => &OP_ABS,
        "signum" => &OP_SIGNUM,
        "floor" => &OP_FLOOR,
        "ceil" => &OP_CEIL,
        "round" => &OP_ROUND,
        "mod" => &OP_MOD,
        "max" => &OP_MAX,
        "min" => &OP_MIN,
        "pow" => &OP_POW,
        "sqrt" => &OP_SQRT,
        "exp" => &OP_EXP,
        "exp2" => &OP_EXP2,
        "ln" => &OP_LN,
        "log2" => &OP_LOG2,
        "log10" => &OP_LOG10,
        "sin" => &OP_SIN,
        "cos" => &OP_COS,
        "tan" => &OP_TAN,
        "asin" => &OP_ASIN,
        "acos" => &OP_ACOS,
        "atan" => &OP_ATAN,
        "atan2" => &OP_ATAN2,
        "sinh" => &OP_SINH,
        "cosh" => &OP_COSH,
        "tanh" => &OP_TANH,
        "asinh" => &OP_ASINH,
        "acosh" => &OP_ACOSH,
        "atanh" => &OP_ATANH,
        "eq" => &OP_EQ,
        "neq" => &OP_NEQ,
        "gt" => &OP_GT,
        "ge" => &OP_GE,
        "lt" => &OP_LT,
        "le" => &OP_LE,
        "negate" => &OP_NEGATE,
        "bit_and" => &OP_BIT_AND,
        "bit_or" => &OP_BIT_OR,
        "bit_not" => &OP_BIT_NOT,
        "bit_xor" => &OP_BIT_XOR,
        "pack_bits" => &OP_PACK_BITS,
        "unpack_bits" => &OP_UNPACK_BITS,
        "concat" => &OP_CONCAT,
        "str_includes" => &OP_STR_INCLUDES,
        "lowercase" => &OP_LOWERCASE,
        "uppercase" => &OP_UPPERCASE,
        "trim" => &OP_TRIM,
        "trim_start" => &OP_TRIM_START,
        "trim_end" => &OP_TRIM_END,
        "starts_with" => &OP_STARTS_WITH,
        "ends_with" => &OP_ENDS_WITH,
        "is_null" => &OP_IS_NULL,
        "is_int" => &OP_IS_INT,
        "is_float" => &OP_IS_FLOAT,
        "is_num" => &OP_IS_NUM,
        "is_string" => &OP_IS_STRING,
        "is_list" => &OP_IS_LIST,
        "is_bytes" => &OP_IS_BYTES,
        "is_in" => &OP_IS_IN,
        "is_finite" => &OP_IS_FINITE,
        "is_infinite" => &OP_IS_INFINITE,
        "is_nan" => &OP_IS_NAN,
        "is_uuid" => &OP_IS_UUID,
        "is_vec" => &OP_IS_VEC,
        "length" => &OP_LENGTH,
        "sorted" => &OP_SORTED,
        "reverse" => &OP_REVERSE,
        "append" => &OP_APPEND,
        "prepend" => &OP_PREPEND,
        "unicode_normalize" => &OP_UNICODE_NORMALIZE,
        "haversine" => &OP_HAVERSINE,
        "haversine_deg_input" => &OP_HAVERSINE_DEG_INPUT,
        "deg_to_rad" => &OP_DEG_TO_RAD,
        "rad_to_deg" => &OP_RAD_TO_DEG,
        "get" => &OP_GET,
        "maybe_get" => &OP_MAYBE_GET,
        "chars" => &OP_CHARS,
        "slice_string" => &OP_SLICE_STRING,
        "from_substrings" => &OP_FROM_SUBSTRINGS,
        "slice" => &OP_SLICE,
        "regex_matches" => &OP_REGEX_MATCHES,
        "regex_replace" => &OP_REGEX_REPLACE,
        "regex_replace_all" => &OP_REGEX_REPLACE_ALL,
        "regex_extract" => &OP_REGEX_EXTRACT,
        "regex_extract_first" => &OP_REGEX_EXTRACT_FIRST,
        "t2s" => &OP_T2S,
        "encode_base64" => &OP_ENCODE_BASE64,
        "decode_base64" => &OP_DECODE_BASE64,
        "first" => &OP_FIRST,
        "last" => &OP_LAST,
        "chunks" => &OP_CHUNKS,
        "chunks_exact" => &OP_CHUNKS_EXACT,
        "windows" => &OP_WINDOWS,
        "to_int" => &OP_TO_INT,
        "to_float" => &OP_TO_FLOAT,
        "to_string" => &OP_TO_STRING,
        "l2_dist" => &OP_L2_DIST,
        "l2_normalize" => &OP_L2_NORMALIZE,
        "ip_dist" => &OP_IP_DIST,
        "cos_dist" => &OP_COS_DIST,
        "int_range" => &OP_INT_RANGE,
        "rand_float" => &OP_RAND_FLOAT,
        "rand_bernoulli" => &OP_RAND_BERNOULLI,
        "rand_int" => &OP_RAND_INT,
        "rand_choose" => &OP_RAND_CHOOSE,
        "assert" => &OP_ASSERT,
        "union" => &OP_UNION,
        "intersection" => &OP_INTERSECTION,
        "difference" => &OP_DIFFERENCE,
        "to_uuid" => &OP_TO_UUID,
        "to_bool" => &OP_TO_BOOL,
        "to_unity" => &OP_TO_UNITY,
        "rand_uuid_v1" => &OP_RAND_UUID_V1,
        "rand_uuid_v4" => &OP_RAND_UUID_V4,
        "uuid_timestamp" => &OP_UUID_TIMESTAMP,
        "validity" => &OP_VALIDITY,
        "make_interval" => &OP_MAKE_INTERVAL,
        "interval_start" => &OP_INTERVAL_START,
        "interval_end" => &OP_INTERVAL_END,
        "interval_before" => &OP_INTERVAL_BEFORE,
        "interval_meets" => &OP_INTERVAL_MEETS,
        "interval_overlaps" => &OP_INTERVAL_OVERLAPS,
        "interval_starts" => &OP_INTERVAL_STARTS,
        "interval_during" => &OP_INTERVAL_DURING,
        "interval_finishes" => &OP_INTERVAL_FINISHES,
        "interval_intersects" => &OP_INTERVAL_INTERSECTS,
        "now" => &OP_NOW,
        "format_timestamp" => &OP_FORMAT_TIMESTAMP,
        "parse_timestamp" => &OP_PARSE_TIMESTAMP,
        "vec" => &OP_VEC,
        "rand_vec" => &OP_RAND_VEC,
        _ => return None,
    })
}

impl Op {
    /// Whether `n` arguments satisfy this op's declared arity: at least
    /// `min_arity` when vararg, exactly `min_arity` otherwise. The parser
    /// and the serde boundary both enforce arity through this one predicate.
    pub(crate) fn arity_matches(&self, n: usize) -> bool {
        if self.vararg {
            n >= self.min_arity
        } else {
            n == self.min_arity
        }
    }

    /// Human phrasing of the arity law, for diagnostics.
    pub(crate) fn arity_requirement(&self) -> String {
        if self.vararg {
            format!("at least {}", self.min_arity)
        } else {
            format!("exactly {}", self.min_arity)
        }
    }

    /// ⚠ HIDDEN AST REWRITE — the one place an op edits its own arguments.
    ///
    /// For every `OP_REGEX_*` op, the second argument (the pattern) is
    /// wrapped in an `OP_REGEX` application at *parse time*, hoisting regex
    /// compilation to compile time: a constant pattern is compiled once by
    /// constant folding instead of once per row, and an invalid constant
    /// pattern is reported before the query runs. The cost is that the AST
    /// no longer matches the source text — `regex_matches(x, p)` becomes
    /// `regex_matches(x, regex(p))` — which anything walking or
    /// pretty-printing expressions must know.
    ///
    /// Called by the parser before its arity check; the CozoDB original
    /// indexed `args[1]` here and panicked on `regex_matches(x)`. A missing
    /// second argument is now left alone — the caller's arity check is about
    /// to reject it with a proper error.
    pub(crate) fn post_process_args(&self, args: &mut [Expr]) {
        if self.name.starts_with("OP_REGEX_")
            && let Some(pattern) = args.get_mut(1)
        {
            *pattern = Expr::Apply {
                op: &OP_REGEX,
                args: [pattern.clone()].into(),
                span: pattern.span(),
            }
        }
    }
}
