//! Program IR vocabulary shared across parse and exec.

pub mod expr;
pub mod op;
pub mod span;
pub mod symbol;

pub use expr::{BindingPos, Decision, Expr, LazyOp};
pub use op::{OpDecl, resolve_decl};
pub use span::SourceSpan;
pub use symbol::{Symbol, SymbolKind};
