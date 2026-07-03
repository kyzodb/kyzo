/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the name-prefix namespaces are an exhaustive kind, not
 * scattered string tests.
 */

//! Names and their namespaces.
//!
//! A [`Symbol`] is a name as the user (or the compiler) wrote it, with its
//! source span. Identity is the name alone — two symbols with the same name
//! are the same symbol wherever they appear.
//!
//! A name's leading characters carry namespace semantics, and there are
//! **two distinct namespaces** with different prefix rules:
//!
//! - The **variable** namespace ([`Symbol::kind`]): `?` is the entry rule,
//!   a bare `_` is an ignored binding, `~`/`*` prefixes mark generated
//!   names, and everything else — including `_`-prefixed names like `_x`
//!   — is an ordinary user variable.
//! - The **relation-name** namespace ([`Symbol::is_temp_relation_name`]):
//!   a `_` prefix (including the bare `_`) marks a temporary relation.
//!
//! Upstream tested prefixes ad hoc at each call site; here each namespace
//! has exactly one classifier, so the two can never be conflated.

use std::cmp::Ordering;
use std::fmt::{Debug, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::ops::Deref;

use miette::{Diagnostic, Result, bail};
use serde_derive::{Deserialize, Serialize};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::span::SourceSpan;

/// The entry rule's name: the `?` whose rows are the query's answer.
pub(crate) const PROG_ENTRY: &str = "?";

/// Which **variable-namespace** kind a name is, read off its shape. This
/// classifies names in binding position only; relation names are a
/// different namespace with different prefix semantics — see
/// [`Symbol::is_temp_relation_name`]. One name has exactly one kind;
/// generated names cannot collide with user names because the generator
/// prefixes (`~`, `*`) are not valid user identifiers in the grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolKind {
    /// `?` — the entry rule.
    Entry,
    /// `_` alone — an ignored binding: matches anything, binds nothing.
    Ignored,
    /// `~`-prefixed — a compiler-generated ignored binding.
    GeneratedIgnored,
    /// `*`-prefixed — a compiler-generated binding.
    Generated,
    /// Anything else — a user-written name. `_`-prefixed names longer than
    /// the bare `_` (such as `_x`) are ordinary user variables, exactly as
    /// upstream treats them; the `_` prefix means "temporary" only in the
    /// relation-name namespace.
    User,
}

/// A name with its source span. Identity (`Eq`/`Ord`/`Hash`) is the name
/// alone; the span rides along for diagnostics and is not part of identity.
#[derive(Clone, Deserialize, Serialize)]
pub struct Symbol {
    pub(crate) name: SmartString<LazyCompact>,
    #[serde(skip)]
    pub(crate) span: SourceSpan,
}

impl Symbol {
    pub(crate) fn new(name: impl Into<SmartString<LazyCompact>>, span: SourceSpan) -> Self {
        Self {
            name: name.into(),
            span,
        }
    }

    /// The entry symbol `?`. The one constructor for it: call sites never
    /// re-spell the convention.
    pub(crate) fn prog_entry(span: SourceSpan) -> Self {
        Self::new(PROG_ENTRY, span)
    }

    /// This name classified in the **variable** namespace. For the
    /// relation-name namespace, use [`Symbol::is_temp_relation_name`]
    /// instead — the two disagree about the `_` prefix by design.
    pub(crate) fn kind(&self) -> SymbolKind {
        match self.name.as_str() {
            PROG_ENTRY => SymbolKind::Entry,
            "_" => SymbolKind::Ignored,
            s if s.starts_with('~') => SymbolKind::GeneratedIgnored,
            s if s.starts_with('*') => SymbolKind::Generated,
            _ => SymbolKind::User,
        }
    }

    /// This name read in the **relation-name** namespace: a `_` prefix
    /// marks a temporary relation, living in the session's scratch store,
    /// never persisted, never write-locked. True for the bare `_` too —
    /// upstream `:create _ ...` makes a temporary relation named `_`, and
    /// KyzoDB preserves that. As a *variable*, the same `_x` is an ordinary
    /// user name; see [`Symbol::kind`].
    pub(crate) fn is_temp_relation_name(&self) -> bool {
        self.name.starts_with('_')
    }

    /// Stored-relation field names must round-trip through headers and
    /// display; parentheses would collide with the aggregation-call syntax
    /// used in returned headers.
    pub(crate) fn ensure_valid_field(&self) -> Result<()> {
        if self.name.contains('(') || self.name.contains(')') {
            #[derive(Debug, Error, Diagnostic)]
            #[error("The symbol {0} is not valid as a field")]
            #[diagnostic(code(parser::symbol_invalid_as_field))]
            struct SymbolInvalidAsField(String, #[label] SourceSpan);

            bail!(SymbolInvalidAsField(self.name.to_string(), self.span))
        }
        Ok(())
    }
}

impl Deref for Symbol {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.name
    }
}

impl Hash for Symbol {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state)
    }
}

impl PartialEq for Symbol {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Symbol {}

impl PartialOrd for Symbol {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Symbol {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name.cmp(&other.name)
    }
}

impl Display for Symbol {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl Debug for Symbol {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str) -> Symbol {
        Symbol::new(name, SourceSpan(0, 0))
    }

    /// The variable-namespace classification table. In particular, `_x` is
    /// an ordinary user variable — the `_` prefix means "temporary" only in
    /// the relation-name namespace.
    #[test]
    fn kind_classifies_the_variable_namespace() {
        for (name, kind) in [
            ("?", SymbolKind::Entry),
            ("_", SymbolKind::Ignored),
            ("~2", SymbolKind::GeneratedIgnored),
            ("*1", SymbolKind::Generated),
            ("*^*3", SymbolKind::Generated),
            ("_x", SymbolKind::User),
            ("x", SymbolKind::User),
            ("some_name", SymbolKind::User),
        ] {
            assert_eq!(sym(name).kind(), kind, "kind of {name:?}");
        }
    }

    /// The relation-name namespace: `_`-prefixed means temporary, and the
    /// bare `_` is a temporary relation too (upstream `:create _` works).
    #[test]
    fn temp_relation_names() {
        assert!(sym("_").is_temp_relation_name());
        assert!(sym("_x").is_temp_relation_name());
        assert!(!sym("x").is_temp_relation_name());
    }

    #[test]
    fn prog_entry_is_the_entry() {
        let entry = Symbol::prog_entry(SourceSpan(0, 0));
        assert_eq!(entry.name.as_str(), PROG_ENTRY);
        assert_eq!(entry.kind(), SymbolKind::Entry);
    }

    /// Field names must round-trip through headers: parentheses collide
    /// with the aggregation-call syntax and are rejected.
    #[test]
    fn ensure_valid_field_rejects_parentheses() {
        assert!(sym("a_field").ensure_valid_field().is_ok());
        assert!(sym("min(x)").ensure_valid_field().is_err());
        assert!(sym("(paren").ensure_valid_field().is_err());
    }
}
