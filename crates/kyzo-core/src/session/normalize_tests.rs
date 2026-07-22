/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use miette::{Result, miette};
use std::collections::BTreeMap;

use kyzo_model::value::{DataValue, ValidityTs};

use crate::data::json::NamedRows;
use crate::parse::{Script, parse_script};
use crate::rules::contract::CancelFlag;
use crate::session::catalog::Catalog;
use crate::session::db::{Engine, ScriptOptions, SessionTx, SessionView};
use crate::store::Storage;
use crate::store::fjall::new_fjall_storage;
use crate::store::sim::SimStorage;

use super::SessionNormalizer;

fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

fn test_vld() -> ValidityTs {
    ValidityTs::from_raw(0)
}

/// Test-local composition: Store + fresh Catalog. Not the deleted fused
/// public `Db::new(storage)` constructor — production callers use
/// [`Engine::compose`].
fn open_engine<S: Storage>(store: S) -> Result<Engine<S>>  {
    Ok(Engine::compose(store, Catalog::new())?)
}

/// Result rows as sorted `i64` vectors, for order-independent assertions.
fn int_rows(nr: &NamedRows) -> Result<Vec<Vec<i64>>> {
    let mut out: Vec<Vec<i64>> = nr
        .rows()
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| v.get_int().ok_or_else(|| miette!("int")))
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    out.sort();
    Ok(out)
}

/// The guard idiom is a language guarantee: `&&`, `||`, and `~`
/// short-circuit, so a deciding left side protects the right side
/// from ever evaluating — through the whole engine, not just the
/// expression unit.
#[test]
fn guard_idiom_short_circuits_through_scripts() -> Result<()>  {
    let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
    let db = open_engine(new_fjall_storage(dir.path())?)?;
    db.run_script(
        "?[k, v] <- [[0, 10], [2, 20]] :create t {k => v}",
        no_params(),
    )
    .map_err(|e| miette!("create: {e}"))?;
    // `%` errors on a zero divisor (`/` does NOT — it yields inf —
    // so a division guard cannot discriminate lazy from strict; the
    // hostile review caught the original test passing vacuously).
    let rows = db
        .run_script("?[k] := *t[k, v], k != 0 && v % k == 0", no_params())
        .map_err(|e| miette!("guarded modulo must not error on the zero row: {e}"))?;
    assert_eq!(int_rows(&rows)?, vec![vec![2]]);
    // Same law when the connective is nested inside another expression.
    let rows = db
        .run_script(
            "?[k] := *t[k, v], w = if(k != 0 && v % k == 0, 1, 0), w == 1",
            no_params(),
        )
        .map_err(|e| miette!("nested guard must not error: {e}"))?;
    assert_eq!(int_rows(&rows)?, vec![vec![2]]);
    // The mirror proves the pin has teeth: unguarded, the zero row
    // DOES error.
    db.run_script("?[k] := *t[k, v], v % k == 0", no_params())
        .expect_err("unguarded modulo must error on the zero row");
    // Coalesce guards the same way.
    let rows = db
        .run_script("?[x] := x = null ~ 7", no_params())
        .map_err(|e| miette!("coalesce: {e}"))?;
    assert_eq!(int_rows(&rows)?, vec![vec![7]]);
    Ok(())
}

/// The reviewers' pushdown hazard, pinned: `to_conjunction` splits a
/// top-level guard conjunction across join sides, and the split must
/// never let the guarded expression evaluate on rows its guard would
/// have excluded — in any atom order, stored or derived.
#[test]
fn guard_survives_conjunction_pushdown_across_joins() -> Result<()>  {
    let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
    let db = open_engine(new_fjall_storage(dir.path())?)?;
    db.run_script("?[k] <- [[1], [2]] :create a {k}", no_params())
        .map_err(|e| miette!("create a: {e}"))?;
    db.run_script(
        "?[k, v] <- [[0, 5], [1, 20], [2, 30]] :create b {k => v}",
        no_params(),
    )
    .map_err(|e| miette!("create b: {e}"))?;
    for (name, script) in [
        (
            "stored join",
            "?[k, v] := *a[k], *b[k, v], k != 0 && v % k == 0",
        ),
        (
            "reordered",
            "?[k, v] := *b[k, v], *a[k], k != 0 && v % k == 0",
        ),
        (
            "derived sides",
            "aa[k] := *a[k]\nbb[k, v] := *b[k, v]\n?[k, v] := aa[k], bb[k, v], k != 0 && v % k == 0",
        ),
    ] {
        let rows = match db.run_script(script, no_params()) {
            Ok(r) => r,
            Err(e) => {
                assert!(false, "{name}: guard must survive pushdown: {e}");
                return;
            }
        };
        assert_eq!(
            int_rows(&rows)?,
            vec![vec![1, 20], vec![2, 30]],
            "{name}: wrong rows"
        );
    }
    Ok(())
}

/// Exercises the normalizer paths the recursive-join test does not: a
/// stratified negation (`not *edge[b, a]`), which drives negation-normal
/// form and the binding-safety well-ordering, and a named-field relation
/// read (`*edge{a: x}`), which drives catalog-schema field resolution.
#[test]
fn negation_and_named_field_through_public_api() -> Result<()>  {
    let db = open_engine(SimStorage::new(13))?;
    db.run_script(
        "?[a, b] <- [[1, 2], [2, 1], [2, 3], [3, 4], [4, 2]] :create edge {a, b}",
        no_params(),
    )
    .map_err(|e| miette!("create: {e}"))?;

    // Sources of edges whose reverse is absent: 1↔2 is symmetric (both
    // excluded); 2→3, 3→4, 4→2 have no reverse, so their sources qualify.
    let neg = db
        .run_script("?[a] := *edge[a, b], not *edge[b, a]", no_params())
        .map_err(|e| miette!("negation query: {e}"))?;
    assert_eq!(int_rows(&neg)?, vec![vec![2], vec![3], vec![4]]);

    // Named-field read binds the `a` column by name; the result is every
    // distinct source vertex.
    let named = db
        .run_script("?[x] := *edge{a: x}", no_params())
        .map_err(|e| miette!("named-field query: {e}"))?;
    assert_eq!(int_rows(&named)?, vec![vec![1], vec![2], vec![3], vec![4]]);
    Ok(())
}

// ── obligation 11: the magic-sets end-to-end differential ────────────────

/// The compiled plan's symbols, so a test can prove the magic-sets
/// rewrite actually fired (a non-`Muggle` symbol) rather than trusting a
/// bound-recursive query to have triggered it.
fn compiled_magic_symbols<S: Storage>(db: &Engine<S>, script: &str) -> Vec<String> {
    match db { // fixed rules bind later at session normalize; parse is params-only
        value => core::mem::drop(value),
    }
    let prog = match parse_script(script, &no_params(), test_vld())? {
        Script::Query(p) => p,
        Script::Imperative(_) | Script::Sys(_) => panic!("expected a single query"),
    };
    let tx = SessionTx::new_read(db.store.read_tx()?, ScriptOptions::default());
    let view = SessionView {
        store: &tx.store,
        temp: &tx.temp,
    };
    let mut normalizer = SessionNormalizer::new(view, CancelFlag::default());
    let (nf, _) =
        crate::exec::plan::program::into_normalized_program(prog, &mut normalizer)?;
    let (strat, _lifetimes) = nf.into_stratified_program()?;
    let magic = strat.magic_sets_rewrite(&view)?;
    magic
        .into_strata()
        .into_iter()
        .flat_map(|m| m.prog.into_keys())
        .map(|sym| format!("{sym:?}"))
        .collect()
}

/// The last unexercised engine law (query/mod.rs #1, magic-sets half):
/// **the demand transform changes which rows are computed, never the
/// result semantics.** Two bound-argument queries against a recursive
/// rule — the shape where magic rewriting fires — are each asserted equal
/// to the reference `laws::naive_eval` (which computes the full fixpoint,
/// no demand restriction) on the same program and facts. The disconnected
/// `5→6` component makes the demand selective: a rewriter that lost or
/// leaked demand returns the wrong rows, not merely a slower plan.
#[test]
fn magic_sets_demand_matches_naive_oracle_end_to_end() -> Result<()>  {
    // Hand-checked fixpoint on edges 1→2→3→4 plus disconnected 5→6
    // (oracle differential lives in kyzo-trials; crate wall forbids kyzo_oracle here).
    let db = open_engine(SimStorage::new(17))?;
    db.run_script(
        "?[a, b] <- [[1, 2], [2, 3], [3, 4], [5, 6]] :create edge {a, b}",
        no_params(),
    )
    .map_err(|e| miette!("create edges: {e}"))?;
    let recursive_rule = "
        path[a, b] := *edge[a, b]
        path[a, b] := *edge[a, c], path[c, b]
    ";

    // Demand pattern 1: first argument bound (forward reachability from 1).
    // A rewritten plan carries adorned symbols (`path|Mbf` magic, `path|Ibf`
    // input, `path|S…` supplementary); a Muggle symbol has no `|adornment`.
    let q1 = format!("{recursive_rule}\n?[d] := path[1, d]");
    let syms1 = compiled_magic_symbols(&db, &q1);
    assert!(
        syms1.iter().any(|s| s.contains('|')),
        "the bound-first-arg query must trigger the magic-sets rewrite; symbols were {syms1:?}"
    );
    let got1 = int_rows(&db.run_script(&q1, no_params()).map_err(|e| miette!("bound-first query: {e}"))?)?;
    assert_eq!(got1, vec![vec![2], vec![3], vec![4]]); // excludes the 5→6 component

    // Demand pattern 2: second argument bound (who reaches 4).
    let q2 = format!("{recursive_rule}\n?[a] := path[a, 4]");
    let syms2 = compiled_magic_symbols(&db, &q2);
    assert!(
        syms2.iter().any(|s| s.contains('|')),
        "the bound-second-arg query must trigger the magic-sets rewrite; symbols were {syms2:?}"
    );
    let got2 = int_rows(&db.run_script(&q2, no_params()).map_err(|e| miette!("bound-second query: {e}"))?)?;
    assert_eq!(
        got2,
        vec![vec![1], vec![2], vec![3]],
        "backward-demand who reaches 4"
    );
    assert_eq!(got2, vec![vec![1], vec![2], vec![3]]);
    Ok(())
}

/// Issue #68 reopened, diagnostic: does the PUBLIC path's magic-sets
/// rewrite actually stay identity for `pointsto.kz`'s fully-unbound
/// entry (`?[y, x] := pt[y, x]`), as the closing comment's "unlikely"
/// assessment assumed (`strange_case_with_disabled_rewrite_is_identity`
/// pins the unbound case in isolation, but never against this specific
/// 4-rule, two-self-reference-occurrence program end to end)? Prints
/// the compiled symbol names — a `|`-adorned name would mean the
/// rewrite fired and pt's rules stopped being the same Muggle rules
/// `bench_api::points_to` hand-builds.
#[test]
fn pointsto_magic_symbols_are_unadorned() -> Result<()>  {
    let db = open_engine(SimStorage::new(6800))?;
    db.run_script("?[a, b] <- [] :create addr_of {a, b}", no_params())
        .map_err(|e| miette!("create addr_of: {e}"))?;
    db.run_script("?[a, b] <- [] :create assign {a, b}", no_params())
        .map_err(|e| miette!("create assign: {e}"))?;
    db.run_script("?[a, b] <- [] :create load {a, b}", no_params())
        .map_err(|e| miette!("create load: {e}"))?;
    db.run_script("?[a, b] <- [] :create store {a, b}", no_params())
        .map_err(|e| miette!("create store: {e}"))?;
    let script = "
        pt[y, x] := *addr_of[y, x]
        pt[y, x] := *assign[y, z], pt[z, x]
        pt[y, w] := *load[y, x], pt[x, z], pt[z, w]
        pt[z, w] := *store[y, x], pt[y, z], pt[x, w]
        ?[y, x] := pt[y, x]
    ";
    let syms = compiled_magic_symbols(&db, script);
    eprintln!("pointsto compiled symbols: {syms:?}");
    // The fully-unbound entry demands only `pt`'s ff (fully-free)
    // variant — issue #68's fix (`AdornedProgram::collapse_ff_redundant_variants`)
    // collapses what sideways information passing would otherwise
    // proliferate into Mff/Mbf/Mbb plus ~20 Input/supplementary
    // relations, all computing overlapping fragments of the same `pt`.
    assert_eq!(
        syms,
        vec!["?".to_string(), "pt|Mff".to_string()],
        "expected the fully-unbound entry to collapse pt to its one ff variant; got {syms:?}"
    );
    Ok(())
}

/// Diagnostic companion to `pointsto_magic_symbols_are_unadorned`: is
/// the spurious-adornment mechanism specific to points-to's two-atom
/// self-join, or does ANY recursive rule with a base-relation atom
/// before its recursive call spuriously adorn under a fully-unbound
/// top query? (`path[a,b] := edge[a,c], path[c,b]` — the standard
/// transitive-closure shape used throughout this test module.)
#[test]
fn transitive_closure_magic_symbols_under_unbound_query() -> Result<()>  {
    let db = open_engine(SimStorage::new(6801))?;
    db.run_script(
        "?[a, b] <- [[1,2],[2,3],[3,4]] :create edge {a, b}",
        no_params(),
    )
    .map_err(|e| miette!("create edge: {e}"))?;
    let script = "
        path[a, b] := *edge[a, b]
        path[a, b] := *edge[a, c], path[c, b]
        ?[a, b] := path[a, b]
    ";
    let syms = compiled_magic_symbols(&db, script);
    eprintln!("tc compiled symbols (fully unbound query): {syms:?}");
    assert_eq!(
        syms,
        vec!["?".to_string(), "path|Mff".to_string()],
        "expected the fully-unbound entry to collapse path to its one ff variant; got {syms:?}"
    );
    Ok(())
}

// ── obligation 12: the standing magic-vs-bypass differential (#68) ───────
//
// The two diagnostic tests above pin the symbol shape for one program
// each, by hand. This is that check turned into a permanent, generic
// differential: a small recursive corpus, each program queried with NO
// bound arguments, run BOTH through the public `Engine::run_script` path
// (`magic.rs`'s rewrite included) and through the production bypass
// door `:disable_magic_rewrite true` on the same facts (every rule
// exempt → muggle symbols → `stratified_magic_compile` → `bind_for_eval`
// → `stratified_evaluate` with no adornment) — asserting byte-identical
// answers (magic-sets law 1) AND the adorned-symbol shape on the magic
// path, one variant per predicate with no `Input`/`Sup` (magic.rs's
// fully-free identity theorem). A regression in either direction —
// wrong answers, or the theorem's cost guarantee — fails here, for any
// future program added to the corpus, not just points-to.
//
// The deleted `bench_api` façade used to host the bypass; that sealed
// door stays cut. The language option is the honest production twin.
mod magic_bypass_differential {
    use miette::{Result, miette};
    use super::*;

    /// Every non-`?` symbol name, sorted — order-independent, so this
    /// doesn't couple to `BTreeMap` iteration order the way the
    /// hand-pinned tests above (deliberately) do.
    fn sorted_syms<S: Storage>(db: &Engine<S>, script: &str) -> Vec<String> {
        let mut syms = compiled_magic_symbols(db, script);
        syms.sort();
        syms
    }

    /// Same program + facts with magic rewrite forced off — the production
    /// bypass twin of a magic-rewritten plan.
    fn run_bypass<S: Storage>(db: &Engine<S>, script: &str) -> Vec<Vec<i64>> {
        int_rows(
            &db.run_script(
                &format!("{script}\n:disable_magic_rewrite true"),
                no_params(),
            )
            .map_err(|e| miette!("bypass query: {e}"))?,
        )?
    }

    /// Transitive closure over a tiny deterministic chain (`0→1→…→n-1`).
    #[test]
    fn tc_chain_public_matches_bypass_byte_identical_and_unadorned() -> Result<()>  {
        let n = 10usize;
        let db = open_engine(SimStorage::new(68_001))?;
        let edge_literal: String = (0..n as i64 - 1)
            .map(|i| format!("[{i},{}],", i + 1))
            .collect();
        db.run_script(
            &format!("?[a, b] <- [{edge_literal}] :create edge {{a, b}}"),
            no_params(),
        )
        .map_err(|e| miette!("create edge: {e}"))?;
        let script = "
            path[a, b] := *edge[a, b]
            path[a, b] := *edge[a, c], path[c, b]
            ?[a, b] := path[a, b]
        ";
        let public_rows = int_rows(&db.run_script(script, no_params()).map_err(|e| miette!("query: {e}"))?)?;
        let bypass_rows = run_bypass(&db, script);

        assert_eq!(
            public_rows, bypass_rows,
            "public path and bypass path must derive the identical answer"
        );
        assert_eq!(
            sorted_syms(&db, script),
            vec!["?".to_string(), "path|Mff".to_string()],
            "a fully-unbound entry must leave path as its one ff variant, matching the \
             bypass path's cost (no Input/Sup machinery)"
        );
        Ok(())
    }

    /// Andersen points-to's self-join shape (`pt` occurs twice in
    /// `load`/`store`'s bodies) — issue #68's actual corpus member.
    /// Facts are generated with the same seeded `StdRng` + `BTreeSet`
    /// dedup the retired bench façade used, so both paths still compute
    /// over a fixed deterministic input.
    #[test]
    fn pointsto_self_join_public_matches_bypass_byte_identical_and_unadorned() -> Result<()>  {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};
        use std::collections::BTreeSet;

        let (vars, addrs, assigns, loads, stores) = (12u64, 8u64, 10u64, 6u64, 6u64);
        let seed = 0x5EED_0068u64;

        let gen_rel = |label: u64, count: u64| -> Vec<(i64, i64)> {
            let mut rng = StdRng::seed_from_u64(seed ^ (label << 32));
            let mut rows: BTreeSet<(i64, i64)> = BTreeSet::new();
            while (rows.len() as u64) < count {
                let y = rng.random_range(0..vars as i64);
                let x = rng.random_range(0..vars as i64);
                if y != x {
                    rows.insert((y, x));
                }
            }
            rows.into_iter().collect()
        };

        let db = open_engine(SimStorage::new(68_002))?;
        let load_rel = |name: &str, rows: &[(i64, i64)]| {
            let literal: String = rows.iter().map(|(y, x)| format!("[{y},{x}],")).collect();
            db.run_script(
                &format!("?[a, b] <- [{literal}] :create {name} {{a, b}}"),
                no_params(),
            )
            .map_err(|e| miette!("create: {e}"))?;
        };
        load_rel("addr_of", &gen_rel(1, addrs));
        load_rel("assign", &gen_rel(2, assigns));
        load_rel("load", &gen_rel(3, loads));
        load_rel("store", &gen_rel(4, stores));
        let script = "
            pt[y, x] := *addr_of[y, x]
            pt[y, x] := *assign[y, z], pt[z, x]
            pt[y, w] := *load[y, x], pt[x, z], pt[z, w]
            pt[z, w] := *store[y, x], pt[y, z], pt[x, w]
            ?[y, x] := pt[y, x]
        ";
        let public_rows = int_rows(&db.run_script(script, no_params()).map_err(|e| miette!("query: {e}"))?)?;
        let bypass_rows = run_bypass(&db, script);

        assert_eq!(
            public_rows, bypass_rows,
            "public path and bypass path must derive the identical answer"
        );
        assert_eq!(
            sorted_syms(&db, script),
            vec!["?".to_string(), "pt|Mff".to_string()],
            "a fully-unbound entry must leave pt as its one ff variant, matching the bypass \
             path's cost (no Input/Sup machinery) — issue #68's regression shape"
        );
        Ok(())
    }

    /// Mutual recursion where only ONE of the two predicates (`p`) is
    /// demanded fully-free (from the entry); `r` is reached only through
    /// `p`'s own rule and never gains a free sibling of its own. `p`
    /// must collapse to its one ff variant; `r` must keep its genuinely-
    /// needed bound variant (with its Input/Sup chain) — the mutual
    /// reference back from `r` to `p` must redirect onto `p|Mff`.
    #[test]
    fn mutual_recursion_bf_and_ff_stays_correctly_reachable() -> Result<()>  {
        let expected: Vec<Vec<i64>> = vec![vec![1, 2], vec![2, 4]];

        let db = open_engine(SimStorage::new(68_101))?;
        for (name, rows) in [
            ("seedp", vec![(1i64, 2i64)]),
            ("linkp", vec![(2, 3)]),
            ("seedr", vec![(3, 4)]),
            ("linkr", vec![(4, 1)]),
        ] {
            let literal: String = rows.iter().map(|(a, b)| format!("[{a},{b}],")).collect();
            db.run_script(
                &format!("?[a, b] <- [{literal}] :create {name} {{a, b}}"),
                no_params(),
            )
            .map_err(|e| miette!("create: {e}"))?;
        }
        let script = "
            p[a, b] := *seedp[a, b]
            p[a, b] := *linkp[a, c], r[c, b]
            r[a, b] := *seedr[a, b]
            r[a, b] := *linkr[a, c], p[c, b]
            ?[a, b] := p[a, b]
        ";
        let got = int_rows(&db.run_script(script, no_params()).map_err(|e| miette!("query: {e}"))?)?;
        assert_eq!(got, expected, "mutual recursion answer");

        let syms = sorted_syms(&db, script);
        assert!(
            syms.iter()
                .filter(|s| s.starts_with("p|M"))
                .eq(["p|Mff"].iter()),
            "p is fully-unbound from the entry and must collapse to its one Magic variant \
             (a `p|S…` supplementary relation feeding r's bound join is fine); got {syms:?}"
        );
        assert!(
            syms.iter().any(|s| s == "r|Mbf" || s == "r|Mfb"),
            "r is never demanded unbound and must keep its genuinely-needed bound variant; got {syms:?}"
        );
        Ok(())
    }

    /// A predicate negated from a later stratum, alongside a SEPARATE
    /// predicate that gets an ff sibling and undergoes the collapse —
    /// negation always targets a Muggle (cross-stratum-exempt) name and
    /// must be completely inert to the redirect/sweep machinery.
    #[test]
    fn negation_with_ff_sibling_stays_correct() -> Result<()>  {
        let expected: Vec<Vec<i64>> = vec![vec![2, 3]];

        let db = open_engine(SimStorage::new(68_102))?;
        for (name, rows) in [
            ("addr_of", vec![(1i64, 2i64), (2, 3)]),
            ("assign", vec![(2, 3), (3, 4)]),
            ("blocked", vec![(1, 2)]),
        ] {
            let literal: String = rows.iter().map(|(a, b)| format!("[{a},{b}],")).collect();
            db.run_script(
                &format!("?[a, b] <- [{literal}] :create {name} {{a, b}}"),
                no_params(),
            )
            .map_err(|e| miette!("create: {e}"))?;
        }
        let script = "
            pt[y, x] := *addr_of[y, x]
            pt[y, x] := *assign[y, z], pt[z, x]
            excluded[y, x] := pt[y, x], not *blocked[y, x]
            ?[y, x] := excluded[y, x]
        ";
        let got = int_rows(&db.run_script(script, no_params()).map_err(|e| miette!("query: {e}"))?)?;
        assert_eq!(got, expected, "negation alongside ff-sibling answer");
        assert_eq!(
            sorted_syms(&db, script),
            vec![
                "?".to_string(),
                "excluded|Mff".to_string(),
                "pt|Mff".to_string()
            ],
            "pt (and the also-fully-unbound excluded) must collapse to their one ff variant \
             apiece, with negation elsewhere in the program"
        );
        Ok(())
    }

    /// Repeated-variable adornment (`r[v, y, y]`'s pinned quirk in
    /// `query::magic`'s own unit tests: the SECOND occurrence of a
    /// repeated variable adorns bound within the SAME atom application)
    /// combined with a fully-unbound entry elsewhere in the program —
    /// confirms the collapse/sweep pair doesn't interact badly with
    /// repeated-argument adornment.
    #[test]
    fn repeated_var_partial_adornment_matches_oracle() -> Result<()>  {
        let expected: Vec<Vec<i64>> = vec![vec![2]];

        let db = open_engine(SimStorage::new(68_103))?;
        db.run_script(
            "?[a, b, c] <- [[1,2,2],[1,3,4]] :create baseq {a, b, c}",
            no_params(),
        )
        .map_err(|e| miette!("create baseq: {e}"))?;
        db.run_script("?[a] <- [[1]] :create seedv {a}", no_params())
            .map_err(|e| miette!("create seedv: {e}"))?;
        let script = "
            q[a, b, c] := *baseq[a, b, c]
            dup[y] := *seedv[v], q[v, y, y]
            ?[y] := dup[y]
        ";
        let got = int_rows(&db.run_script(script, no_params()).map_err(|e| miette!("query: {e}"))?)?;
        assert_eq!(got, expected, "repeated-variable adornment answer");
        Ok(())
    }

    /// The reviewer's orphan shape, reconstructed to the closest
    /// adversarial approximation this investigation could derive from
    /// the review summary alone (the literal repro text was not
    /// available to reconstruct verbatim): `helper` is referenced from
    /// WITHIN `pt`'s own self-joining `load` rule, bound via `load`'s
    /// output rather than `pt`'s own head — an adornment-INVARIANT
    /// binding source, so `helper`'s demand is identical whether walked
    /// under `pt`'s surviving ff variant or its (redirected-away) bound
    /// ones. This construction keeps `helper` correctly reachable
    /// through `pt`'s surviving copy rather than orphaning it; it is
    /// included as a verified-correct adjacent case, not a positive
    /// reproduction of the reviewer's exact finding. The sweep's actual
    /// necessity is independently and unambiguously demonstrated by
    /// `pointsto_magic_symbols_are_unadorned` and
    /// `tc_chain_public_matches_bypass_byte_identical_and_unadorned`
    /// above: with `collapse_ff_redundant_variants` refactored to only
    /// redirect (its own drop step removed once the sweep landed),
    /// disabling `sweep_unreachable` leaves points-to's OWN redirected-
    /// away `pt|Mbf`/`pt|Mbb` uncollected in the base case, which both
    /// of those tests catch directly.
    #[test]
    fn helper_via_relation_bound_var_inside_self_join_survives_correctly() -> Result<()>  {
        let expected: Vec<Vec<i64>> = vec![vec![1, 2]];

        let db = open_engine(SimStorage::new(68_104))?;
        for (name, rows) in [
            ("addr_of", vec![(1i64, 2i64)]),
            ("assign", vec![(2, 3)]),
            ("load", vec![(3, 4)]),
            ("store", vec![(4, 1)]),
            ("baseh", vec![(4, 5)]),
            ("linkh", vec![(5, 6)]),
        ] {
            let literal: String = rows.iter().map(|(a, b)| format!("[{a},{b}],")).collect();
            db.run_script(
                &format!("?[a, b] <- [{literal}] :create {name} {{a, b}}"),
                no_params(),
            )
            .map_err(|e| miette!("create: {e}"))?;
        }
        let script = "
            pt[y, x] := *addr_of[y, x]
            pt[y, x] := *assign[y, z], pt[z, x]
            pt[y, w] := *load[y, x], helper[x, z], pt[z, w]
            pt[z, w] := *store[y, x], pt[y, z], pt[x, w]
            helper[a, b] := *baseh[a, b]
            helper[a, b] := *linkh[a, c], helper[c, b]
            ?[y, x] := pt[y, x]
        ";
        let got = int_rows(&db.run_script(script, no_params()).map_err(|e| miette!("query: {e}"))?)?;
        assert_eq!(got, expected, "helper-inside-self-join answer");

        let syms = sorted_syms(&db, script);
        assert!(
            syms.iter()
                .filter(|s| s.starts_with("pt|M"))
                .eq(["pt|Mff"].iter()),
            "pt must collapse to its one Magic variant (a `pt|S…` supplementary relation \
             feeding helper's bound join is fine); got {syms:?}"
        );
        assert!(
            syms.iter().any(|s| s.starts_with("helper|Mb")),
            "helper must keep its genuinely-needed bound variant, reachable through pt's \
             surviving ff copy of the load rule; got {syms:?}"
        );
        Ok(())
    }
}
