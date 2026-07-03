/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! An opaque façade over the crate-internal query pipeline, for the RA-layer
//! criterion benches (`benches/ra_exec.rs`) and the iterator-vs-batched
//! equivalence campaign.
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
use crate::query::compile::{
    CompiledProgram, ExecMode, NoFixedRules, bind_for_eval, stratified_magic_compile,
};
use crate::query::eval::{Budget, RowLimit, stratified_evaluate};
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

/// Which RA execution strategy the run drives. The batched path is
/// selected here and threaded through `bind_for_eval` to `RelAlgebra`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exec {
    /// The classic tuple-at-a-time iterator tree.
    Iterator,
    /// The batched (vectorized) pipeline, where implemented; falls back to
    /// the iterator path per-operator otherwise.
    Batched,
}

impl Exec {
    fn to_mode(self) -> ExecMode {
        match self {
            Exec::Iterator => ExecMode::Iterator,
            Exec::Batched => ExecMode::Batched,
        }
    }
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
        valid_at: None,
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
    pub fn run(&self, exec: Exec) -> usize {
        match &self.backend {
            BackendStore::Mem(s) => self.run_on(s, exec),
            BackendStore::Fjall(s) => self.run_on(s, exec),
        }
    }

    /// Evaluate to the entry store and return its rows, sorted. Used by the
    /// equivalence campaign (iterator vs batched must be byte-identical).
    pub fn collect(&self, exec: Exec) -> Vec<Tuple> {
        match &self.backend {
            BackendStore::Mem(s) => self.collect_on(s, exec),
            BackendStore::Fjall(s) => self.collect_on(s, exec),
        }
    }

    fn run_on<S: Storage>(&self, store: &S, exec: Exec) -> usize {
        let rtx = store.read_tx().expect("read tx");
        let program =
            bind_for_eval::<_, NoFixedRules>(&self.compiled, &rtx, exec.to_mode(), &mut |_| {
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

    fn collect_on<S: Storage>(&self, store: &S, exec: Exec) -> Vec<Tuple> {
        let rtx = store.read_tx().expect("read tx");
        let program =
            bind_for_eval::<_, NoFixedRules>(&self.compiled, &rtx, exec.to_mode(), &mut |_| {
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
        let handle = create_relation(&mut tx, input).expect("create relation");
        for row in &rel.rows {
            let key = handle.encode_key_for_store(row, sp()).expect("encode key");
            let val = handle.encode_val_for_store(row, sp()).expect("encode val");
            tx.put(&key, &val).expect("put row");
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
