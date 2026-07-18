//! Temporal trial batteries — split by kind of proof, not by size.
//!
//! - [`script`]: language-surface as-of via `Db::run_script` + real `@` clauses
//! - [`path`]: full compile → RA → eval differential against the naive as-of oracle

#[cfg(test)]
mod path;
#[cfg(test)]
mod script;
