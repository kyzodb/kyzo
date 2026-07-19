//! Expression evaluation doors (row + columnar).
pub(crate) mod batch;
pub(crate) mod eval;

pub(crate) use eval::{eval_expr, eval_pred, eval_to_const};
