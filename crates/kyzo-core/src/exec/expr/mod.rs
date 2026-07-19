//! Expression evaluation doors (row + columnar).
pub(crate) mod batch;
pub(crate) mod eval;

#[allow(unused_imports)] // reexport surface; callers bind later or via tests
pub(crate) use eval::{eval_expr, eval_pred, eval_to_const, resolve_write_validity};
