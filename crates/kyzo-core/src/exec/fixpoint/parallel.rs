/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). Extracted from semi-naive stratified evaluation: rayon-vs-wasm
 * dispatch for the non-entry rule batch inside one stratum epoch.
 */

//! Parallel (rayon) vs sequential (wasm32) collection of non-entry rule
//! evaluations within a stratum epoch. The merge barrier stays in
//! [`super::eval::evaluate_stratum`]; this module only schedules the batch.

use std::collections::BTreeMap;

use crate::exec::plan::program::MagicSymbol;
use crate::exec::fixpoint::eval::EvalDefinition;

/// Collect results for every non-entry definition in `defs` (entry rules
/// under a limit are excluded — they run sequentially in
/// [`super::eval::evaluate_stratum`]).
///
/// On non-wasm hosts this uses rayon `par_iter`; on `wasm32` it runs
/// sequentially. Both paths share the same filter and map so schedule
/// differences cannot change which rules are evaluated.
pub(crate) fn collect_non_entry_batch<'a, R, F, Out, E, Exec>(
    defs: &'a BTreeMap<MagicSymbol, EvalDefinition<R, F>>,
    limiter_enabled: bool,
    execution: Exec,
) -> Vec<Result<Out, E>>
where
    R: Send + Sync,
    F: Send + Sync,
    Out: Send,
    E: Send,
    Exec: Fn((&'a MagicSymbol, &'a EvalDefinition<R, F>)) -> Result<Out, E> + Sync,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        use rayon::prelude::*;
        defs.par_iter()
            .filter(|(name, _)| !(limiter_enabled && name.is_prog_entry()))
            .map(&execution)
            .collect()
    }
    #[cfg(target_arch = "wasm32")]
    {
        defs.iter()
            .filter(|(name, _)| !(limiter_enabled && name.is_prog_entry()))
            .map(&execution)
            .collect()
    }
}
