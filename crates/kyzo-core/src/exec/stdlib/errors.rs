//! Shared stdlib refuse types and NaN/domain checkpoint helpers.

use std::borrow::Cow;

use miette::{Diagnostic, Result, bail, miette};
use thiserror::Error;

use crate::data::value::{DataValue, Num, NumRepr, Vector, data_value_any};

/// A 64-bit integer scalar op overflowed. Arithmetic errors are typed
/// errors by law: never a silent panic (debug builds) and never silent
/// wraparound (release builds serving a wrong answer). Float paths are
/// untouched — they saturate to infinity legitimately.
#[derive(Debug, Error, Diagnostic)]
#[error("integer overflow evaluating '{op}'")]
#[diagnostic(code(eval::integer_overflow))]
#[diagnostic(help("The operands are exact 64-bit integers whose result does not fit in i64."))]
pub(crate) struct IntegerOverflow {
    pub(crate) op: &'static str,
}

/// A zero divisor was offered to `div` or `mod`, integer or float alike. A
/// silent `Infinity`/`NaN` is a poison value that buries the caller's logic
/// bug; this engine refuses instead, the same way `mod` always has and `div`
/// now does too — one typed shape for both ops, parameterized only by name.
#[derive(Debug, Error, Diagnostic)]
#[error("'{op}' requires a non-zero divisor")]
#[diagnostic(code(eval::division_by_zero))]
#[diagnostic(help(
    "Division and modulo both refuse a zero divisor rather than returning infinity or NaN."
))]
pub(crate) struct DivisionByZero {
    pub(crate) op: &'static str,
}

/// An argument fell outside a partial math op's domain: `sqrt` of a
/// negative, `ln`/`log2`/`log10` of a non-positive, `asin`/`acos` outside
/// `[-1, 1]`, `acosh` below `1`, `atanh` outside `(-1, 1)`, `pow` with a
/// negative base and fractional exponent or a zero base and negative
/// exponent. The same silent-poison shape as [`DivisionByZero`]: the raw
/// `f64` method would return `NaN`, or at an excluded boundary an infinity,
/// instead of failing loudly — one typed refusal for every partial math op,
/// parameterized only by name.
#[derive(Debug, Error, Diagnostic)]
#[error("'{op}' is undefined for the given argument")]
#[diagnostic(code(eval::domain_error))]
#[diagnostic(help(
    "This op is partial: some inputs have no real, finite result. Check the \
     argument lies within the function's mathematical domain before calling it."
))]
pub(crate) struct DomainError {
    /// Borrowed for every existing per-op guard's `&'static str` literal;
    /// owned for the op-application CHECKPOINTS (`data::expr::apply_op`,
    /// `query::vm`'s columnar kernel), which only have `Op::name` — the
    /// screaming-case Rust const identifier — and must render it through
    /// [`crate::data::expr::op_display_name`] first. One error type either
    /// way: the checkpoints are a backstop for the SAME domain violation a
    /// guard would have caught, never a different concept.
    pub(crate) op: Cow<'static, str>,
}

/// Backstop for the partial math ops below: even after the domain guard
/// each one runs before computing, a scalar result of `NaN` is refused the
/// same way. No math op may hand back a poison value, whether or not its
/// domain was characterized exhaustively enough to catch it up front.
fn no_nan(op: &'static str, x: f64) -> Result<DataValue> {
    if x.is_nan() {
        bail!(DomainError { op: op.into() });
    }
    Ok(DataValue::Num(Num::float(x)))
}

/// A vector op was invoked with an empty argument slice after the
/// single-argument early return — unrepresentable under `op_add` /
/// `op_mul`'s calling contract (a `Vector` argument implies `len >= 1`).
#[derive(Debug, Error, Diagnostic)]
#[error("'{op}' vector lane requires a non-empty argument slice")]
#[diagnostic(code(eval::vec_op_empty_args))]
pub(crate) struct VecOpEmptyArgs {
    pub(crate) op: &'static str,
}

/// The vector-lane counterpart of [`no_nan`]: an element-wise result with
/// any `NaN` left in it is refused rather than returned poisoned (the
/// refusal runs BEFORE [`Vector::try_new`], whose canonicalization would
/// otherwise normalize the poison into a legal value).
fn no_nan_vec(op: &'static str, v: Vec<f64>) -> Result<DataValue> {
    if v.iter().any(|x| x.is_nan()) {
        bail!(DomainError { op: op.into() });
    }
    Ok(DataValue::Vector(
        Vector::try_new(v).ok_or_else(|| miette!("vector dimension exceeds u32"))?,
    ))
}

/// Whether a function-op's result is carrying a poison `NaN` — as the
/// scalar float itself, or as any lane of a vector result. This is the
/// predicate the STRUCTURAL op-application checkpoints test on every op
/// result (the row evaluator's `data::expr::apply_op` and the columnar
/// kernel in `query::vm`), so a `NaN` escapes neither evaluator regardless
/// of whether the producing op remembered to route its own result through
/// [`no_nan`]/[`no_nan_vec_f32`]/[`no_nan_vec_f64`]. Those per-op guards
/// stay exactly as they are — belt-and-suspenders, and the only thing that
/// can turn a domain violation into a *targeted* diagnostic before the
/// generic result ever reaches this backstop. Infinity is untouched: it is
/// a legitimate result (e.g. `pow` saturating), never poison.
pub(crate) fn result_has_nan(v: &DataValue) -> bool {
    match v {
        DataValue::Num(n) => matches!(n.repr(), NumRepr::Float(x) if x.is_nan()),
        DataValue::Vector(v) => v.to_f64s().iter().any(|x| x.is_nan()),
        data_value_any!() => false,
    }
}

/// `a.powf(b)` is partial in two ways: a negative base raised to a
/// fractional exponent has no real result (`NaN` — e.g. `(-1)^0.5`), and a
/// zero base raised to a negative exponent diverges to an infinity (e.g.
/// `0^-1`, the same shape as a division by zero expressed through `pow`).
fn pow_out_of_domain(a: f64, b: f64) -> bool {
    (a < 0.0 && b.fract() != 0.0) || (a == 0.0 && b < 0.0)
}
