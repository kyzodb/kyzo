/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Session body normalizer: NNF → DNF → well-ordering via exec/plan/normalize.
//!
//! [`SessionNormalizer`] is the session seat's [`BodyNormalizer`]: DNF conversion
//! (named-field relation atoms against the catalog) plus binding-safety
//! well-ordering. Pure NNF/DNF/reorder live in [`crate::exec::plan::normalize`].
//! Magic-sets end-to-end differentials that drive this seat live in the tests
//! below (peeled from `session/db.rs` under #351 T3).

use miette::Result;

use crate::exec::plan::program::{BodyNormalizer, NormalFormInlineRule};
use crate::rules::contract::CancelFlag;
use crate::session::db::SessionView;
use crate::store::ReadTx;
use kyzo_model::program::{InputAtom, TempSymbGen};

/// The session's [`BodyNormalizer`]: DNF conversion (which resolves
/// named-field relation atoms against the catalog) plus binding-safety
/// well-ordering. Pure NNF/DNF/reorder live in [`crate::exec::plan::normalize`].
pub(crate) struct SessionNormalizer<'a, T> {
    pub(crate) view: SessionView<'a, T>,
    cancel: CancelFlag,
    symb_gen: TempSymbGen,
}

impl<'a, T> SessionNormalizer<'a, T> {
    pub(crate) fn new(view: SessionView<'a, T>, cancel: CancelFlag) -> Self {
        Self {
            view,
            cancel,
            symb_gen: TempSymbGen::default(),
        }
    }
}

impl<T: ReadTx> BodyNormalizer for SessionNormalizer<'_, T> {
    fn disjunctive_normal_form(
        &mut self,
        body: InputAtom,
    ) -> Result<Vec<Vec<crate::exec::plan::program::NormalFormAtom>>> {
        use crate::exec::plan::normalize::{do_disjunctive_normal_form, negation_normal_form};
        let nnf = negation_normal_form(body)?;
        let disjunction = do_disjunctive_normal_form(
            nnf,
            &mut self.symb_gen,
            &|name| self.view.handle(name).map(|h| h.metadata),
            &|name| self.view.handle(name),
            &self.cancel,
        )?;
        Ok(disjunction)
    }

    fn well_order(&mut self, rule: NormalFormInlineRule) -> Result<NormalFormInlineRule> {
        crate::exec::plan::normalize::convert_to_well_ordered_rule(rule)
    }
}

#[cfg(test)]
#[path = "normalize_tests.rs"]
mod tests;
