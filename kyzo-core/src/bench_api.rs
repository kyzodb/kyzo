/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! An opaque façade over the crate-internal query pipeline, for the RA-layer
//! criterion benches (`benches/ra_exec.rs`) and the determinism campaign —
//! all through the engine's one (vectorized) execution machine.
//!
//! The RA execution path — `stratified_magic_compile → bind_for_eval →
//! stratified_evaluate` — is entirely `pub(crate)`; a criterion bench is an
//! *external* target that can only see `pub` items. Rather than widen the
//! engine's public surface, this module (which has crate-internal access)
//! constructs the workloads here and hands the bench an opaque [`Workload`]
//! whose only public operations are "run it" and "collect its rows". No
//! crate-internal type crosses the boundary.
//!
//! The workloads mirror the shapes the design ascent must measure:
//! transitive closure (chain/dense/random graphs), a selective 3-way join,
//! a wide scan+filter, and an aggregation. Generators are seeded and
//! deterministic. Both the in-memory backend ([`Backend::Mem`], the
//! `SimStorage` MVCC double) and the on-disk backend ([`Backend::Fjall`])
//! are reachable.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::path::Path;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use smartstring::SmartString;

use crate::DataValue;
use crate::data::aggr::parse_aggr;
use crate::data::expr::Expr;
use crate::data::functions::OP_GT;
use crate::data::program::{
    InputRelationHandle, MagicAtom, MagicInlineRule, MagicProgram, MagicRelationApplyAtom,
    MagicRuleApplyAtom, MagicRulesOrFixed, MagicSymbol, StoreLifetimes, StratifiedMagicProgram,
};
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::engines::segments::Segments;
use crate::query::compile::{
    CompiledProgram, NoFixedRules, bind_for_eval, stratified_magic_compile,
};
use crate::query::eval::{Budget, RowLimit, stratified_evaluate};
use crate::runtime::relation::KeyspaceKind;
use crate::runtime::relation::create_relation;
use crate::storage::fjall::{FjallStorage, new_fjall_storage};
use crate::storage::sim::SimStorage;
use crate::storage::{Storage, WriteTx};

/// Which storage backend a workload is materialized on.
#[derive(Clone, Copy, Debug)]
pub enum Backend {
    /// In-memory MVCC double (`SimStorage`). No disk, no serialization tax —
    /// isolates the query engine's own cost.
    Mem,
    /// On-disk LSM (`fjall`). The real substrate; measures scan cost through
    /// the storage contract.
    Fjall,
}

// ── span / symbol plumbing (mirrors compile.rs's test helpers) ───────────

fn sp() -> SourceSpan {
    SourceSpan(0, 0)
}
fn sym(name: &str) -> Symbol {
    Symbol::new(name, sp())
}
fn muggle(rel: &str) -> MagicSymbol {
    MagicSymbol::Muggle { inner: sym(rel) }
}
fn entry_symbol() -> MagicSymbol {
    MagicSymbol::Muggle {
        inner: Symbol::prog_entry(sp()),
    }
}
fn v(i: i64) -> DataValue {
    DataValue::from(i)
}

fn col(name: &str) -> ColumnDef {
    ColumnDef {
        name: SmartString::from(name),
        typing: NullableColType {
            coltype: ColType::Any,
            nullable: false,
        },
        default_gen: None,
    }
}

fn rule_atom(name: &str, args: &[Symbol]) -> MagicAtom {
    MagicAtom::Rule(MagicRuleApplyAtom {
        name: muggle(name),
        args: args.to_vec(),
        span: sp(),
    })
}
fn rel_atom(name: &str, args: &[Symbol]) -> MagicAtom {
    MagicAtom::Relation(MagicRelationApplyAtom {
        name: sym(name),
        args: args.to_vec(),
        validity: None,
        span: sp(),
    })
}
fn plain_rule(head: &[Symbol], body: Vec<MagicAtom>) -> MagicInlineRule {
    MagicInlineRule {
        head: head.to_vec(),
        aggr: vec![None; head.len()],
        body,
    }
}

fn program_of(strata: Vec<Vec<(MagicSymbol, Vec<MagicInlineRule>)>>) -> StratifiedMagicProgram {
    let strata = strata
        .into_iter()
        .map(|defs| {
            let mut prog = MagicProgram::default();
            for (name, rules) in defs {
                prog.prog.insert(name, MagicRulesOrFixed::Rules { rules });
            }
            prog
        })
        .collect();
    StratifiedMagicProgram::from_execution_order(strata).expect("entry in final stratum")
}

// ── the opaque workload ──────────────────────────────────────────────────

/// A compiled, ready-to-run query over a materialized backend. The compile
/// step (which reads stored-relation metadata) happens once, at build time;
/// each [`Workload::run`] opens a fresh read snapshot and drives semi-naive
/// evaluation, so the timed region is evaluation, not compilation.
pub struct Workload {
    backend: BackendStore,
    compiled: Vec<CompiledProgram>,
    lifetimes: StoreLifetimes,
    label: String,
}

enum BackendStore {
    Mem(SimStorage),
    Fjall(FjallStorage),
}

impl Workload {
    /// A human-readable label for criterion's benchmark id.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Evaluate to the entry store and return its row count. The whole
    /// point of the bench: this is the region criterion times.
    pub fn run(&self) -> usize {
        match &self.backend {
            BackendStore::Mem(s) => self.run_on(s),
            BackendStore::Fjall(s) => self.run_on(s),
        }
    }

    /// Evaluate to the entry store and return its rows, sorted. Used by the
    /// equivalence campaign (iterator vs batched must be byte-identical).
    pub fn collect(&self) -> Vec<Tuple> {
        match &self.backend {
            BackendStore::Mem(s) => self.collect_on(s),
            BackendStore::Fjall(s) => self.collect_on(s),
        }
    }

    fn run_on<S: Storage>(&self, store: &S) -> usize {
        let rtx = store.read_tx().expect("read tx");
        let program =
            bind_for_eval::<_, NoFixedRules>(&self.compiled, &rtx, Segments::OFF, &mut |_| {
                panic!("bench workloads have no fixed rules")
            })
            .expect("binds");
        let outcome = stratified_evaluate(
            &program,
            &self.lifetimes,
            RowLimit::default(),
            &generous_budget(),
            None,
        )
        .expect("evaluates");
        outcome.store.all_iter().count()
    }

    fn collect_on<S: Storage>(&self, store: &S) -> Vec<Tuple> {
        let rtx = store.read_tx().expect("read tx");
        let program =
            bind_for_eval::<_, NoFixedRules>(&self.compiled, &rtx, Segments::OFF, &mut |_| {
                panic!("bench workloads have no fixed rules")
            })
            .expect("binds");
        let outcome = stratified_evaluate(
            &program,
            &self.lifetimes,
            RowLimit::default(),
            &generous_budget(),
            None,
        )
        .expect("evaluates");
        let mut rows: Vec<Tuple> = outcome.store.all_iter().map(|t| t.into_tuple()).collect();
        rows.sort();
        rows
    }
}

fn generous_budget() -> Budget {
    Budget::new(NonZeroU32::new(100_000).expect("nonzero")).with_derived_tuple_ceiling(50_000_000)
}

/// Every store lives to the end (fine for a single-shot bench workload).
fn immortal_lifetimes(compiled: &[CompiledProgram]) -> StoreLifetimes {
    let mut lifetimes = StoreLifetimes::default();
    let last = compiled.len().saturating_sub(1);
    for stratum in compiled {
        for name in stratum.keys() {
            lifetimes.note_use(name.clone(), last);
        }
    }
    lifetimes
}

// ── backend materialization ──────────────────────────────────────────────

/// A stored relation to seed the backend with, all-key-columns.
struct SeedRelation {
    name: String,
    arity: usize,
    rows: Vec<Tuple>,
}

fn build(
    backend: Backend,
    store_dir: &Path,
    seeds: Vec<SeedRelation>,
    program: StratifiedMagicProgram,
    label: String,
) -> Workload {
    let backend = match backend {
        Backend::Mem => {
            let store = SimStorage::new(0xB0FF_0000);
            seed_backend(&store, &seeds);
            BackendStore::Mem(store)
        }
        Backend::Fjall => {
            // `store_dir` is a fresh directory the caller (a dev bench/test
            // target) owns and cleans up; the on-disk store lives inside it.
            let store = new_fjall_storage(store_dir).expect("fjall");
            seed_backend(&store, &seeds);
            BackendStore::Fjall(store)
        }
    };
    let (compiled, lifetimes) = match &backend {
        BackendStore::Mem(s) => compile(s, program),
        BackendStore::Fjall(s) => compile(s, program),
    };
    Workload {
        backend,
        compiled,
        lifetimes,
        label,
    }
}

fn seed_backend<S: Storage>(store: &S, seeds: &[SeedRelation]) {
    let mut tx = store.write_tx().expect("write tx");
    for rel in seeds {
        let keys: Vec<ColumnDef> = (0..rel.arity).map(|i| col(&format!("c{i}"))).collect();
        let key_bindings = keys.iter().map(|c| sym(&c.name)).collect();
        let input = InputRelationHandle {
            name: sym(&rel.name),
            metadata: StoredRelationMetadata {
                keys,
                non_keys: vec![],
            },
            key_bindings,
            dep_bindings: vec![],
            span: sp(),
        };
        let handle = create_relation(&mut tx, input, KeyspaceKind::Facts).expect("create relation");
        for row in &rel.rows {
            handle
                .put_fact(
                    &mut tx,
                    row,
                    crate::data::value::ValidityTs::from_raw(0),
                    sp(),
                )
                .expect("put row");
        }
    }
    tx.commit().expect("commit");
}

fn compile<S: Storage>(
    store: &S,
    program: StratifiedMagicProgram,
) -> (Vec<CompiledProgram>, StoreLifetimes) {
    let rtx = store.read_tx().expect("read tx");
    let compiled = stratified_magic_compile(&rtx, program).expect("compiles");
    let lifetimes = immortal_lifetimes(&compiled);
    (compiled, lifetimes)
}

// ── graph generators (seeded, deterministic) ─────────────────────────────

/// Shape of the generated edge relation for transitive closure.
#[derive(Clone, Copy, Debug)]
pub enum Graph {
    /// A single path `0→1→2→…→n-1`. TC is O(n²) pairs — the pathological
    /// recursion depth case.
    Chain,
    /// `k` disjoint cliques of size `n/k`-ish. Dense local reachability.
    Dense,
    /// Erdős–Rényi-ish random edges at a fixed out-degree.
    Random,
}

fn gen_edges(shape: Graph, n: usize, seed: u64) -> Vec<Tuple> {
    let mut edges: Vec<Tuple> = Vec::new();
    match shape {
        Graph::Chain => {
            for i in 0..n.saturating_sub(1) {
                edges.push(vec![v(i as i64), v((i + 1) as i64)]);
            }
        }
        Graph::Dense => {
            // Cliques of ~sqrt(n) vertices so TC stays bounded but dense.
            let clique = ((n as f64).sqrt().ceil() as usize).max(2);
            let mut base = 0usize;
            while base < n {
                let end = (base + clique).min(n);
                for a in base..end {
                    for b in base..end {
                        if a != b {
                            edges.push(vec![v(a as i64), v(b as i64)]);
                        }
                    }
                }
                base = end;
            }
        }
        Graph::Random => {
            let mut rng = StdRng::seed_from_u64(seed);
            let out_degree = 3;
            for a in 0..n {
                for _ in 0..out_degree {
                    let b = rng.random_range(0..n);
                    if a != b {
                        edges.push(vec![v(a as i64), v(b as i64)]);
                    }
                }
            }
        }
    }
    edges.sort();
    edges.dedup();
    edges
}

// ── workload constructors ────────────────────────────────────────────────

/// Transitive closure `path(x,y)` over a generated `edge` relation.
///
/// ```datalog
/// path[x, y] := edge[x, y]
/// path[x, y] := edge[x, z], path[z, y]
/// ?[x, y]    := path[x, y]
/// ```
pub fn transitive_closure(
    backend: Backend,
    shape: Graph,
    n: usize,
    seed: u64,
    tmp: &Path,
) -> Workload {
    let edges = gen_edges(shape, n, seed);
    let (x, y, z) = (sym("x"), sym("y"), sym("z"));
    let program = program_of(vec![
        vec![(
            muggle("path"),
            vec![
                plain_rule(
                    &[x.clone(), y.clone()],
                    vec![rel_atom("edge", &[x.clone(), y.clone()])],
                ),
                plain_rule(
                    &[x.clone(), y.clone()],
                    vec![
                        rel_atom("edge", &[x.clone(), z.clone()]),
                        rule_atom("path", &[z.clone(), y.clone()]),
                    ],
                ),
            ],
        )],
        vec![(
            entry_symbol(),
            vec![plain_rule(
                &[x.clone(), y.clone()],
                vec![rule_atom("path", &[x, y])],
            )],
        )],
    ]);
    build(
        backend,
        tmp,
        vec![SeedRelation {
            name: "edge".into(),
            arity: 2,
            rows: edges,
        }],
        program,
        format!("tc/{shape:?}/n{n}"),
    )
}

/// Andersen-style points-to over synthetic `addr_of`/`assign`/`load`/`store`
/// statements — mirrors `kyzo-bench`'s `pointsto.kz` exactly (issue #68's
/// memory-blowup workload). The `load`/`store` rules each mention `pt` at
/// TWO body positions — the self-join shape that used to collapse
/// `contained_rules()` to a single name-keyed entry (the retired
/// `ContainedRuleMultiplicity::Many`) and disable semi-naive delta
/// narrowing for that rule entirely. Fixed by keying `contained_rules()`
/// on [`crate::query::eval::AtomOccurrence`] (body position) instead of
/// store name, so each occurrence is independently delta-selectable.
///
/// ```datalog
/// pt[y, x] := *addr_of[y, x]
/// pt[y, x] := *assign[y, z], pt[z, x]
/// pt[y, w] := *load[y, x], pt[x, z], pt[z, w]
/// pt[z, w] := *store[y, x], pt[y, z], pt[x, w]
/// ?[y, x]  := pt[y, x]
/// ```
#[allow(clippy::too_many_arguments)]
pub fn points_to(
    backend: Backend,
    vars: u64,
    addrs: u64,
    assigns: u64,
    loads: u64,
    stores: u64,
    seed: u64,
    tmp: &Path,
) -> Workload {
    // Distinct sub-seeds per relation so the four generators don't retrace
    // each other's draws (mirrors kyzo-bench's `seed.derive(label)`).
    let gen_rel = |label: u64, count: u64| -> Vec<Tuple> {
        let mut rng = StdRng::seed_from_u64(seed ^ (label << 32));
        let mut rows: BTreeSet<(i64, i64)> = BTreeSet::new();
        while (rows.len() as u64) < count {
            let y = rng.random_range(0..vars as i64);
            let x = rng.random_range(0..vars as i64);
            if y != x {
                rows.insert((y, x));
            }
        }
        rows.into_iter().map(|(y, x)| vec![v(y), v(x)]).collect()
    };
    let addr_of = gen_rel(1, addrs);
    let assign = gen_rel(2, assigns);
    let load = gen_rel(3, loads);
    let store = gen_rel(4, stores);
    let (y, x, z, w) = (sym("y"), sym("x"), sym("z"), sym("w"));
    let program = program_of(vec![
        vec![(
            muggle("pt"),
            vec![
                plain_rule(
                    &[y.clone(), x.clone()],
                    vec![rel_atom("addr_of", &[y.clone(), x.clone()])],
                ),
                plain_rule(
                    &[y.clone(), x.clone()],
                    vec![
                        rel_atom("assign", &[y.clone(), z.clone()]),
                        rule_atom("pt", &[z.clone(), x.clone()]),
                    ],
                ),
                plain_rule(
                    &[y.clone(), w.clone()],
                    vec![
                        rel_atom("load", &[y.clone(), x.clone()]),
                        rule_atom("pt", &[x.clone(), z.clone()]),
                        rule_atom("pt", &[z.clone(), w.clone()]),
                    ],
                ),
                plain_rule(
                    &[z.clone(), w.clone()],
                    vec![
                        rel_atom("store", &[y.clone(), x.clone()]),
                        rule_atom("pt", &[y.clone(), z.clone()]),
                        rule_atom("pt", &[x.clone(), w.clone()]),
                    ],
                ),
            ],
        )],
        vec![(
            entry_symbol(),
            vec![plain_rule(
                &[y.clone(), x.clone()],
                vec![rule_atom("pt", &[y, x])],
            )],
        )],
    ]);
    build(
        backend,
        tmp,
        vec![
            SeedRelation {
                name: "addr_of".into(),
                arity: 2,
                rows: addr_of,
            },
            SeedRelation {
                name: "assign".into(),
                arity: 2,
                rows: assign,
            },
            SeedRelation {
                name: "load".into(),
                arity: 2,
                rows: load,
            },
            SeedRelation {
                name: "store".into(),
                arity: 2,
                rows: store,
            },
        ],
        program,
        format!("pointsto/v{vars}-a{addrs}-s{assigns}-l{loads}-t{stores}"),
    )
}

/// A selective 3-way join: `j(x,w) := a(x,y), b(y,z), c(z,w)`. Each relation
/// is a random bipartite mapping so the join fans in, not out. `n` rows per
/// relation over a key domain of `n/fan` — `fan` sets match multiplicity.
pub fn three_way_join(backend: Backend, n: usize, fan: usize, seed: u64, tmp: &Path) -> Workload {
    let mut rng = StdRng::seed_from_u64(seed);
    let domain = (n / fan).max(1) as i64;
    let mk = |rng: &mut StdRng| -> Vec<Tuple> {
        let mut rows: Vec<Tuple> = (0..n)
            .map(|_| {
                vec![
                    v(rng.random_range(0..domain)),
                    v(rng.random_range(0..domain)),
                ]
            })
            .collect();
        rows.sort();
        rows.dedup();
        rows
    };
    let a = mk(&mut rng);
    let b = mk(&mut rng);
    let c = mk(&mut rng);
    let (x, y, z, w) = (sym("x"), sym("y"), sym("z"), sym("w"));
    let program = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[x.clone(), w.clone()],
            vec![
                rel_atom("a", &[x.clone(), y.clone()]),
                rel_atom("b", &[y.clone(), z.clone()]),
                rel_atom("c", &[z.clone(), w.clone()]),
            ],
        )],
    )]]);
    build(
        backend,
        tmp,
        vec![
            SeedRelation {
                name: "a".into(),
                arity: 2,
                rows: a,
            },
            SeedRelation {
                name: "b".into(),
                arity: 2,
                rows: b,
            },
            SeedRelation {
                name: "c".into(),
                arity: 2,
                rows: c,
            },
        ],
        program,
        format!("join3/n{n}/fan{fan}"),
    )
}

/// A wide scan + filter: a 5-column relation of `n` rows, filtered on the
/// first column. This is the batched scan→filter→project pipeline's home
/// turf. `select_frac` in [0,100] sets roughly what fraction passes.
pub fn scan_filter(
    backend: Backend,
    n: usize,
    select_frac: i64,
    seed: u64,
    tmp: &Path,
) -> Workload {
    let mut rng = StdRng::seed_from_u64(seed);
    let rows: Vec<Tuple> = (0..n)
        .map(|i| {
            vec![
                v(i as i64),
                v(rng.random_range(0..100)),
                v(rng.random_range(0..1_000)),
                v(rng.random_range(0..1_000)),
                v(rng.random_range(0..1_000)),
            ]
        })
        .collect();
    // Threshold on c1 (uniform in [0,100)): keep c1 > (100 - select_frac).
    let threshold = 100 - select_frac;
    let (c0, c1, c2, c3, c4) = (sym("c0"), sym("c1"), sym("c2"), sym("c3"), sym("c4"));
    let pred = Expr::Apply {
        op: &OP_GT,
        args: Box::new([
            Expr::Binding {
                var: c1.clone(),
                tuple_pos: None,
            },
            Expr::Const {
                val: v(threshold),
                span: sp(),
            },
        ]),
        span: sp(),
    };
    let program = program_of(vec![vec![(
        entry_symbol(),
        vec![plain_rule(
            &[c0.clone(), c1.clone(), c2.clone(), c3.clone(), c4.clone()],
            vec![
                rel_atom("wide", &[c0, c1, c2, c3, c4]),
                MagicAtom::Predicate(pred),
            ],
        )],
    )]]);
    build(
        backend,
        tmp,
        vec![SeedRelation {
            name: "wide".into(),
            arity: 5,
            rows,
        }],
        program,
        format!("scan_filter/n{n}/sel{select_frac}"),
    )
}

/// Aggregation-heavy: `count` grouped by the first column of an `n`-row,
/// `g`-group relation. A stratum-boundary normal aggregation.
///
/// ```datalog
/// agg[g, count(x)] := nums[g, x]
/// ?[g, c]          := agg[g, c]
/// ```
pub fn aggregation(backend: Backend, n: usize, groups: usize, seed: u64, tmp: &Path) -> Workload {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut rows: Vec<Tuple> = (0..n)
        .map(|i| vec![v(rng.random_range(0..groups as i64)), v(i as i64)])
        .collect();
    rows.sort();
    rows.dedup();
    let (g, x, c) = (sym("g"), sym("x"), sym("c"));
    let count = parse_aggr("count").expect("count aggr exists");
    let agg_rule = MagicInlineRule {
        head: vec![g.clone(), x.clone()],
        aggr: vec![None, Some((count, vec![]))],
        body: vec![rel_atom("nums", &[g.clone(), x.clone()])],
    };
    let program = program_of(vec![
        vec![(muggle("agg"), vec![agg_rule])],
        vec![(
            entry_symbol(),
            vec![plain_rule(
                &[g.clone(), c.clone()],
                vec![rule_atom("agg", &[g, c])],
            )],
        )],
    ]);
    build(
        backend,
        tmp,
        vec![SeedRelation {
            name: "nums".into(),
            arity: 2,
            rows,
        }],
        program,
        format!("aggregation/n{n}/g{groups}"),
    )
}

// ── bulk-ingest attribution (issue #74) ──────────────────────────────────
//
// Where a 1000-row `:put` batch's time goes: script parse (literal vs a
// `$param`-substituted body), the mutation pipeline's per-row work (extract
// + encode + the SSI current-row probe + the routed write), and the bare
// storage floor (raw fjall put+commit with no relation/session/catalog at
// all). Each function isolates exactly one of those so a caller can time
// them independently and subtract; nothing here changes what the real
// path does, it only calls the same crate-internal pieces the real path
// calls, in isolation.

/// A synthetic `n`-row batch: `[i, i*3]` starting at `start`, so successive
/// batches never collide on key `i` (a genuine bulk INSERT shape, not
/// repeated updates of the same keys).
fn synthetic_rows(start: i64, n: usize) -> Vec<(i64, i64)> {
    (0..n as i64)
        .map(|i| (start + i, (start + i) * 3))
        .collect()
}

fn put_literal_script(rows: &[(i64, i64)]) -> String {
    let mut s = String::from("?[k, v] <- [");
    for (k, v) in rows {
        s.push_str(&format!("[{k},{v}],"));
    }
    s.push_str("] :put w {k => v}");
    s
}

const PUT_PARAM_SCRIPT: &str = "?[k, v] <- $data :put w {k => v}";

fn param_pool_of(rows: &[(i64, i64)]) -> BTreeMap<String, DataValue> {
    let mut pool = BTreeMap::new();
    pool.insert(
        "data".to_string(),
        DataValue::List(
            rows.iter()
                .map(|(k, v)| DataValue::List(vec![DataValue::from(*k), DataValue::from(*v)]))
                .collect(),
        ),
    );
    pool
}

/// Parse an `n`-row LITERAL `:put` script (the row values spelled out in
/// the script text) and discard the result. Isolates the literal-shape
/// parse cost alone — no compile, no eval, no storage.
pub fn parse_put_literal(n: usize) -> miette::Result<()> {
    let script = put_literal_script(&synthetic_rows(0, n));
    let parsed = crate::parse::parse_script(
        &script,
        &BTreeMap::new(),
        &BTreeMap::new(),
        crate::data::value::ValidityTs::from_raw(0),
    )?;
    std::hint::black_box(parsed);
    Ok(())
}

/// Parse the PARAM-DRIVEN `:put` script (`?[k, v] <- $data :put …`) with an
/// `n`-row `$data` substituted from the param pool, and discard the result.
/// The script TEXT is `n`-independent; whatever cost scales with `n` here is
/// the cost of substituting (cloning) the param's `DataValue` into the AST
/// at parse time, not text parsing.
pub fn parse_put_param(n: usize) -> miette::Result<()> {
    let pool = param_pool_of(&synthetic_rows(0, n));
    let parsed = crate::parse::parse_script(
        PUT_PARAM_SCRIPT,
        &pool,
        &BTreeMap::new(),
        crate::data::value::ValidityTs::from_raw(0),
    )?;
    std::hint::black_box(parsed);
    Ok(())
}

/// Run `n_batches` of `batch_rows`-row `:put`s through the PUBLIC `Db` —
/// the full real path (parse, compile/bind, evaluate the `Constant` source,
/// the mutation pipeline's extract/encode/probe/write, commit) exactly as a
/// script-driving caller exercises it. Returns `(rows written, wall time
/// for all batches)`; the relation is created once, up front, and excluded
/// from the timed region. Each batch targets fresh keys (a bulk INSERT
/// shape).
pub fn run_put_batches(
    backend: Backend,
    batch_rows: usize,
    n_batches: usize,
    param_driven: bool,
    tmp: &Path,
) -> miette::Result<(usize, std::time::Duration)> {
    fn seed_and_run<S: Storage>(
        db: crate::runtime::db::Db<S>,
        batch_rows: usize,
        n_batches: usize,
        param_driven: bool,
    ) -> miette::Result<(usize, std::time::Duration)> {
        db.run_script("?[k, v] <- [] :create w {k => v}", BTreeMap::new())?;
        let t0 = std::time::Instant::now();
        let mut total = 0usize;
        for b in 0..n_batches {
            let rows = synthetic_rows((b * batch_rows) as i64, batch_rows);
            if param_driven {
                db.run_script(PUT_PARAM_SCRIPT, param_pool_of(&rows))?;
            } else {
                db.run_script(&put_literal_script(&rows), BTreeMap::new())?;
            }
            total += rows.len();
        }
        Ok((total, t0.elapsed()))
    }
    match backend {
        Backend::Mem => {
            let db = crate::runtime::db::Db::new(SimStorage::new(0xB0FF_0001))?;
            seed_and_run(db, batch_rows, n_batches, param_driven)
        }
        Backend::Fjall => {
            let db = crate::runtime::db::Db::new(new_fjall_storage(tmp)?)?;
            seed_and_run(db, batch_rows, n_batches, param_driven)
        }
    }
}

/// The bare storage floor: `n_batches` transactions of `batch_rows` raw
/// `put`s each, straight through `Storage`/`WriteTx` — no relation catalog,
/// no session, no extraction, no bitemporal key/value encoding, no
/// current-row probe. Whatever fjall itself costs to accept the same
/// number of keys, nothing more.
pub fn bare_fjall_put_batches(
    batch_rows: usize,
    n_batches: usize,
    tmp: &Path,
) -> miette::Result<(usize, std::time::Duration)> {
    let storage = new_fjall_storage(tmp)?;
    let t0 = std::time::Instant::now();
    let mut total = 0usize;
    for b in 0..n_batches {
        let mut tx = storage.write_tx()?;
        for i in 0..batch_rows {
            let k = (b * batch_rows + i) as i64;
            let key = crate::data::tuple::encode_tuple_key(42, &[DataValue::from(k)]);
            tx.put(key.as_bytes(), &k.to_be_bytes())?;
        }
        tx.commit()?;
        total += batch_rows;
    }
    Ok((total, t0.elapsed()))
}

/// The bulk-write path's per-row key+value encode, alone: build a real
/// `RelationHandle` (arity 2, one key column) over an in-memory `SimStorage`
/// (no disk I/O to conflate with the encode cost itself), then encode `n`
/// distinct rows' bitemporal key and value — the exact calls
/// `put_into_relation` makes per row — and discard the bytes. No probe, no
/// write, no commit.
pub fn encode_only(n: usize) -> miette::Result<std::time::Duration> {
    let store = SimStorage::new(0xE1C0DE01);
    let mut tx = store.write_tx().expect("write tx");
    let input = InputRelationHandle {
        name: sym("w"),
        metadata: StoredRelationMetadata {
            keys: vec![col("k")],
            non_keys: vec![col("v")],
        },
        key_bindings: vec![sym("k")],
        dep_bindings: vec![sym("v")],
        span: sp(),
    };
    let handle = create_relation(&mut tx, input, KeyspaceKind::Facts).expect("create relation");
    let stamp = tx.system_stamp();
    let t0 = std::time::Instant::now();
    for i in 0..n as i64 {
        let row = [DataValue::from(i), DataValue::from(i * 3)];
        let key = handle
            .encode_bitemporal_key_for_store(&row, stamp, stamp, sp())
            .expect("encode key");
        let val = handle
            .encode_bitemporal_val_for_store(
                &row,
                crate::data::bitemporal::ClaimPolarity::Assert,
                sp(),
            )
            .expect("encode val");
        std::hint::black_box((key, val));
    }
    Ok(t0.elapsed())
}

/// The bulk-write path's per-row SSI current-row probe, alone, against an
/// otherwise-EMPTY relation (the genuine bulk-INSERT shape: every probed
/// key is absent) — isolates
/// [`RelationHandle::current_row`](crate::runtime::relation::RelationHandle)'s
/// cost (key+bound encoding plus the conflict-tracked range read) from
/// everything else `put_into_relation` does.
pub fn probe_only_not_found(n: usize) -> miette::Result<std::time::Duration> {
    let store = SimStorage::new(0xB0BE_0002);
    let mut tx = store.write_tx().expect("write tx");
    let input = InputRelationHandle {
        name: sym("w"),
        metadata: StoredRelationMetadata {
            keys: vec![col("k")],
            non_keys: vec![col("v")],
        },
        key_bindings: vec![sym("k")],
        dep_bindings: vec![sym("v")],
        span: sp(),
    };
    let handle = create_relation(&mut tx, input, KeyspaceKind::Facts).expect("create relation");
    let as_of = crate::data::value::AsOf::current(crate::data::value::MAX_VALIDITY_TS);
    let t0 = std::time::Instant::now();
    for i in 0..n as i64 {
        let row = [DataValue::from(i)];
        let found = handle.current_row(&tx, &row, as_of, sp()).expect("probe");
        std::hint::black_box(found);
    }
    Ok(t0.elapsed())
}
