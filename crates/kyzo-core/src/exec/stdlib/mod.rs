//! Builtin stdlib: BoundOp registry + kernels. No catch-all functions module.
pub mod bind;
pub mod bound_op;
pub mod collection;
pub mod compare;
pub mod convert;
pub mod errors;
pub mod geo;
pub mod interval;
pub mod metric;
pub mod nondet;
pub mod numeric;
pub mod temporal_format;
pub mod text;

#[cfg(test)]
mod tests;

#[allow(unused_imports)] // reexport surface; callers bind later or via tests
pub use bind::{bind_op, resolve_op};
#[allow(unused_imports)] // reexport surface; callers bind later or via tests
pub use bound_op::BoundOp;
#[allow(unused_imports)] // reexport surface; callers bind later or via tests
pub use errors::StdlibRefuse;
