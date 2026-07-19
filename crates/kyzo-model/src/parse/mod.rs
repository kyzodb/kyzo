//! Parse zone: text → typed IR with spans and refusals.
//!
//! Search-atom AST seats land here first; full KyzoScript parse bodies
//! follow as their IR dependencies allow.

pub mod search;

pub use search::{
    FtsBooster, FtsExpr, FtsLiteral, FtsNear, NonEmptyFtsExprs, NonEmptyFtsLiterals,
};
