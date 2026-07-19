/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The fixed-rule contract surface: order-preserving parallelism and the
//! session-backed [`SessionFixedRule`] evaluation adapter.
//!
//! Several fixed rules fan out an independent, side-effect-free computation
//! per node / per start / per node-pair, then fold the results. Upstream ran
//! those fan-outs under `rayon`; the port left them sequential behind
//! `SEAM(parallelism)` markers while the workspace carried no `rayon`. The
//! query engine has since taken a direct `rayon` dependency, so the seam
//! can close.
//!
//! The one law here is **determinism**: parallel execution must produce
//! byte-identical output to the sequential path. [`par_try_map`] is the only
//! tool the algorithms use, and it is order-preserving by construction — so
//! the axis it parallelizes never reaches the output as scheduling order.
//! Cross-item float reductions (which are *not* order-independent) stay in a
//! sequential fold the caller runs over the returned, canonically ordered
//! `Vec`; they are never handed to a parallel reduction.
//!
//! [`SessionFixedRule`] bridges one `MagicFixedRuleApply` to `FixedRule::run`
//! at evaluation time. Output is branded with the manifest arity (never a
//! caller-supplied one); the budget's cancel poll is shared so a cancelled
//! query stops the rule; budgeted output is armed with the true global
//! admitted total.

use std::collections::BTreeMap;

use miette::Result;
#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;

use crate::data::program::{MagicFixedRuleApply, MagicSymbol};
use crate::exec::fixpoint::delta_store::{EpochStore, RegularTempStore};
use crate::exec::fixpoint::eval::{Budget, FixedRuleEval};
use crate::fixed_rule::{
    CancelFlag, FixedRuleOutput, FixedRulePayload, StoredInputSource,
};

/// Order-preserving fallible parallel map: apply `f` to every item, collect
/// the results into a `Vec` **in the same order as `items`**, and
/// short-circuit on the first `Err`.
///
/// On native targets the map runs on `rayon`'s thread pool; on `wasm32`
/// (no threads) it degrades to a sequential map, matching how
/// `query/eval.rs` gates its per-epoch batch. `rayon`'s `collect` into a
/// `Vec` is index-preserving, so the output order equals the input order
/// regardless of how work is scheduled across threads — that is the
/// property callers rely on for determinism.
///
/// This parallelizes only the per-item compute. Any reduction *across*
/// items whose result depends on evaluation order (a float sum, say) must be
/// performed by the caller as a sequential fold over the returned `Vec`,
/// never smuggled into a parallel reduction — see the algorithm call sites.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn par_try_map<T, R, F>(items: Vec<T>, f: F) -> Result<Vec<R>>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Result<R> + Send + Sync,
{
    items.into_par_iter().map(f).collect()
}

/// `wasm32` has no threads; run the same fallible map sequentially. The
/// output is identical to the native path (both preserve input order), so
/// callers need not know which one they got.
#[cfg(target_arch = "wasm32")]
pub(crate) fn par_try_map<T, R, F>(items: Vec<T>, f: F) -> Result<Vec<R>>
where
    F: Fn(T) -> Result<R>,
{
    items.into_iter().map(f).collect()
}

// ─────────────────────────────────────────────────────────────────────────
// The fixed-rule evaluation adapter
// ─────────────────────────────────────────────────────────────────────────

/// Bridges one `MagicFixedRuleApply` to `FixedRule::run` at evaluation time.
/// It assembles the payload (in-memory rule inputs from the epoch stores,
/// stored-relation inputs through a [`StoredInputSource`]), brands the output
/// store with the manifest arity (never a caller-supplied one), and shares the
/// budget's cancel poll as the rule's [`CancelFlag`] so a cancelled query stops
/// the rule too. This is the concrete `F` that `bind_for_eval`'s `make_fixed`
/// factory produces — the seam that lets a stored/derived query APPLY a fixed
/// rule (including the `Constant` rule behind every `<- [[…]]` inline datum).
///
/// `S` is the session read surface (production: `SessionView`); rules never
/// import the concrete session type — only the [`StoredInputSource`] seam.
pub(crate) struct SessionFixedRule<'a, S> {
    apply: &'a MagicFixedRuleApply,
    view: S,
    cancel: CancelFlag,
}

impl<'a, S> SessionFixedRule<'a, S> {
    pub(crate) fn new(
        apply: &'a MagicFixedRuleApply,
        view: S,
        cancel: CancelFlag,
    ) -> Self {
        Self {
            apply,
            view,
            cancel,
        }
    }
}

impl<S: StoredInputSource + Send + Sync> FixedRuleEval for SessionFixedRule<'_, S> {
    fn run(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        out: &mut RegularTempStore,
        budget: &Budget,
        baseline: u64,
    ) -> Result<()> {
        let payload = FixedRulePayload {
            manifest: self.apply,
            stores,
            stored: &self.view,
        };
        // Armed with the query's derived-tuple ceiling and the true global
        // admitted total as of this stratum's epoch-0 barrier, so a
        // row-amplifying algorithm refuses mid-run — counting every prior
        // admission, not just this writer's own rows — instead of
        // materializing unbounded output.
        let mut output = FixedRuleOutput::new_budgeted(
            self.apply.arity,
            self.apply.span,
            baseline,
            budget.derived_tuple_ceiling(),
        );
        self.apply
            .fixed_impl
            .clone()
            .run(payload, &mut output, self.cancel.clone())?;
        // Replace eval's fresh epoch-0 store with the branded output wholesale.
        *out = output.into_store();
        Ok(())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// The load-bearing property: results come back in input order, not
    /// completion order. A body that sleeps longer for earlier items would
    /// reorder a naive `par_iter().map().collect_into_unordered()`; this
    /// must not.
    #[test]
    fn preserves_input_order() {
        let got = par_try_map((0u32..1000).collect(), |i| Ok::<_, miette::Report>(i * 2));
        assert_eq!(
            got.unwrap(),
            (0u32..1000).map(|i| i * 2).collect::<Vec<_>>()
        );
    }

    /// A single-thread pool and the default pool agree byte-for-byte.
    #[test]
    fn single_thread_matches_default_pool() {
        // INVARIANT(test_hash_mix): golden-hash mul in a unit test; wrap is intentional.
        let f = |i: u32| Ok::<_, miette::Report>(i.wrapping_mul(2_654_435_761));
        let default = par_try_map((0u32..2000).collect(), f).unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let single = pool.install(|| par_try_map((0u32..2000).collect(), f).unwrap());
        assert_eq!(default, single);
    }

    /// A raised error short-circuits the collect.
    #[test]
    fn propagates_error() {
        let got: Result<Vec<u32>> = par_try_map((0u32..100).collect(), |i| {
            if i == 42 {
                Err(miette::miette!("boom"))
            } else {
                Ok(i)
            }
        });
        assert!(got.is_err());
    }
}
