/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The parse bridge: kyzo-core's window onto kyzo-model's parse zone.
//!
//! The language door — grammar, [`Script`], the `parse_*` lifts, and the
//! FTS/schema/expression AST — is seated in kyzo-model
//! (`crates/kyzo-model/src/parse/`), which never imports the engine. This
//! module re-exports that zone wholesale so the crate's `crate::parse::…`
//! call sites resolve unchanged, and adds one engine-facing seat that the
//! model zone cannot hold: [`sys`], the typed lift of a parsed `::…` system
//! script into the engine-shaped `SysOp`.
//!
//! Why `sys` lives here and not in the model: a `SysOp`'s index-declaration
//! configs carry engine objects — an analyzer [`crate::project::text::TokenizerConfig`],
//! the persisted manifests they seed — that kyzo-model must never depend on.
//! kyzo-model parses a `::…` script into pure-data [`kyzo_model::parse::sys::SysScript`]
//! syntax (names, constants, extractor `Expr`s, tokenizer name+args); this
//! module's [`sys`] admits that syntax into the sealed engine `SysOp`.

pub(crate) use kyzo_model::parse::*;

pub(crate) mod sys;
