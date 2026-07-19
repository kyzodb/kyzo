//! Program IR vocabulary shared across parse and exec.

pub mod span;
pub mod symbol;

pub use span::SourceSpan;
pub use symbol::{Symbol, SymbolKind};
