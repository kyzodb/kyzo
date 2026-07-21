/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). AccessLevel + InsufficientAccessLevel peeled from
 * runtime/relation.rs into the session/access seat (story #350 T2).
 */

//! Who may read or write which relations in this session.
//!
//! **The `Ord` derive IS the semantics**: every gate is a comparison,
//! `Hidden < ReadOnly < Protected < Normal`, so "at least protected" is
//! `>= Protected` and nothing re-encodes the ladder. Do not reorder the
//! variants.

use std::fmt::{Display, Formatter};

use miette::Diagnostic;
use thiserror::Error;

use crate::parse::sys::AccessLevel as ParseAccessLevel;

/// What operations a stored relation admits. **The `Ord` derive IS the
/// semantics** (an existing type-driven win of the original, preserved
/// deliberately): every gate is a comparison, `Hidden < ReadOnly <
/// Protected < Normal`, so "at least protected" is `>= Protected` and
/// nothing re-encodes the ladder. Do not reorder the variants.
#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    PartialEq,
    serde_derive::Serialize,
    serde_derive::Deserialize,
    Default,
    Ord,
    PartialOrd,
)]
pub enum AccessLevel {
    Hidden,
    ReadOnly,
    Protected,
    #[default]
    Normal,
}

impl Display for AccessLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessLevel::Normal => f.write_str("normal"),
            AccessLevel::Protected => f.write_str("protected"),
            AccessLevel::ReadOnly => f.write_str("read_only"),
            AccessLevel::Hidden => f.write_str("hidden"),
        }
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("Insufficient access level {2} for {1} on stored relation '{0}'")]
#[diagnostic(code(tx::insufficient_access_level))]
pub struct InsufficientAccessLevel(pub(crate) String, pub(crate) String, pub(crate) AccessLevel);

/// Map the parser's access-level enum to the catalog's. Both are the same
/// four-rung ladder; the parse tier and runtime tier keep distinct types.
pub(crate) fn map_access_level(level: ParseAccessLevel) -> AccessLevel {
    match level {
        ParseAccessLevel::Hidden => AccessLevel::Hidden,
        ParseAccessLevel::ReadOnly => AccessLevel::ReadOnly,
        ParseAccessLevel::Protected => AccessLevel::Protected,
        ParseAccessLevel::Normal => AccessLevel::Normal,
    }
}
