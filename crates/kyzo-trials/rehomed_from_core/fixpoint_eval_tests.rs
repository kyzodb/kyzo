
#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    use itertools::Itertools;
    use proptest::prelude::*;

    use super::*;
    use kyzo_model::program::aggregate::parse_aggr;
    use crate::exec::plan::program::StoreLifetimes;
    use kyzo_model::SourceSpan;
    use kyzo_model::program::symbol::Symbol;
    use crate::exec::provenance::eval::Witness;
    use kyzo_oracle::eval::{FixedRule, Literal, Program, Rel, Rule, Term, naive_eval};

    // ── plumbing ─────────────────────────────────────────────────────────

    fn v(i: i64) -> DataValue {
        DataValue::from(i)
    }
    fn muggle(rel: &str) -> MagicSymbol {
        MagicSymbol::Muggle {
            inner: Symbol::new(rel, SourceSpan(0, 0)),
        }
    }
    fn entry_symbol() -> MagicSymbol {
        MagicSymbol::Muggle {
            inner: Symbol::prog_entry(SourceSpan(0, 0)),
        }
    }
    fn generous_budget() -> Budget {
        Budget::new(NonZeroU32::new(10_000).unwrap())
    }
    fn no_limit() -> RowLimit {
        RowLimit::default()
    }
    fn lit(rel: Rel, args: Vec<Term>, negated: bool) -> Literal {
        if negated {
            Literal::neg(rel, args)
        } else {
            Literal::pos(rel, args)
        }
    }
    fn x() -> Term {
        Term::Var("X")
    }
    fn y() -> Term {
        Term::Var("Y")
    }
    fn z() -> Term {
        Term::Var("Z")
    }
    fn named(name: &str) -> HeadAggr {
        HeadAggrSlot::Aggregated {
            aggr: parse_aggr(name)
                .ok()
                .flatten()
                .unwrap_or_else(|| panic!("real aggregation exists: {name}")),
            args: vec![],
        }
    }

    // ── the differential harness: the oracle's rule model as a RuleBody ──
    //
    // This is the first of the two RuleBody implementations the seam was
    // designed for: it evaluates one `laws::Rule` body against the live
    // EpochStore map by naive nested-loop unification, positives (in
    // order) before negatives, mirroring `laws::body_bindings` — except
    // that IDB literals read the stores (total, or the delta of
    // `delta_from`), which is exactly the context the compile tier's RA
    // plans will receive.

    type Bindings = HashMap<&'static str, DataValue>;

    fn unify(args: &[Term], tuple: &[DataValue], bound: &Bindings) -> Option<Bindings> {
        if args.len() != tuple.len() {
            return None;
        }
        let mut out = bound.clone();
        for (t, val) in args.iter().zip(tuple) {
            match t {
                Term::Const(c) => {
                    if c != val {
                        return None;
                    }
                }
                Term::Var(name) => match out.get(name) {
                    Some(existing) if existing != val => return None,
                    Some(_) => {}
                    None => {
                        out.insert(name, val.clone());
                    }
                },
            }
        }
        Some(out)
    }

    fn ground(args: &[Term], bound: &Bindings) -> Tuple {
        args.iter()
            .map(|t| match t {
                Term::Const(c) => c.clone(),
                Term::Var(var) => bound[var].clone(),
            })
            .collect()
    }

    #[derive(Debug)]
    struct ModelBody {
        head: Vec<Term>,
        body: Vec<Literal>,
        /// The EDB: base facts, read-only, shared across the program.
        facts: Arc<BTreeMap<Rel, BTreeSet<Tuple>>>,
        /// Which relations are rule/fixed heads (stores), not EDB.
        idb: Arc<BTreeSet<Rel>>,
        /// Occurrence key = this literal's position in `body` (stable
        /// across the positive/negated reordering `stream_join` uses for
        /// evaluation order) — one entry per idb literal, positive OR
        /// negated: this map is also the lifetime-tracking dependency
        /// source (`note_use`, below), and a store read only inside a
        /// negation is used just as much as one read positively. A
        /// relation mentioned twice gets two independent occurrences, each
        /// independently delta-selectable (matches the real engine's
        /// `compile.rs::contained_rules`, which numbers the same way over
        /// `MagicInlineRule::body`) — though `stream_join` never selects a
        /// negated occurrence's delta (negation always reads totals).
        contained: BTreeMap<AtomOccurrence, MagicSymbol>,
    }

    impl ModelBody {
        fn new(
            head: Vec<Term>,
            body: Vec<Literal>,
            facts: Arc<BTreeMap<Rel, BTreeSet<Tuple>>>,
            idb: Arc<BTreeSet<Rel>>,
        ) -> Self {
            let mut contained: BTreeMap<AtomOccurrence, MagicSymbol> = BTreeMap::new();
            for (i, l) in body.iter().enumerate() {
                if idb.contains(l.rel) {
                    contained.insert(AtomOccurrence(i), muggle(l.rel));
                }
            }
            Self {
                head,
                body,
                facts,
                idb,
                contained,
            }
        }

        fn rows_of(
            &self,
            stores: &BTreeMap<MagicSymbol, EpochStore>,
            rel: Rel,
            use_delta: bool,
        ) -> Result<Vec<Tuple>> {
            if self.idb.contains(rel) {
                let store = store_of(stores, &muggle(rel))?;
                Ok(if use_delta {
                    collect_materialized(store.delta_all_iter()?)
                } else {
                    collect_materialized(store.all_iter()?)
                })
            } else {
                Ok(self
                    .facts
                    .get(rel)
                    .map(|set| set.iter().cloned().collect())
                    .unwrap_or_default())
            }
        }

        fn negated_probe_hits(
            &self,
            stores: &BTreeMap<MagicSymbol, EpochStore>,
            rel: Rel,
            probe: &Tuple,
        ) -> Result<bool> {
            if self.idb.contains(rel) {
                // Full-tuple probe via prefix_iter: exact at full-tuple
                // granularity for meet stores too (EpochStore::exists
                // truncates meet probes to the group key, which is the
                // wrong question for negation).
                let store = store_of(stores, &muggle(rel))?;
                Ok(store
                    .prefix_iter(probe)?
                    .next()
                    .is_some_and(|t| t == probe.as_slice()[..]))
            } else {
                Ok(self.facts.get(rel).is_some_and(|set| set.contains(probe)))
            }
        }
    }

    impl ModelBody {
        /// Depth-first STREAMING join: recurse literal by literal, calling
        /// `f` at each fully-bound leaf. This never materializes the join
        /// frontier — the old frontier `Vec` grew to the whole cross product
        /// *below* the budget's tick seam (`f`), so a near-cross-product OOMed
        /// the harness before the guard could fire (reviewer finding F3).
        /// Streaming pushes every derivation through `f` as it is found, so
        /// the mid-epoch guard bounds the harness exactly as it bounds the
        /// compiled production path. The nested-loop recursion visits leaves
        /// in the SAME order as the old frontier expansion (outer literal
        /// slowest), so derivation order, premises, and admission order are
        /// byte-identical to before — only the memory profile changes.
        #[allow(clippy::too_many_arguments)]
        fn stream_join(
            &self,
            stores: &BTreeMap<MagicSymbol, EpochStore>,
            delta_from: Option<AtomOccurrence>,
            want_premises: bool,
            ordered: &[(usize, &Literal)],
            idx: usize,
            bound: &Bindings,
            premises: &mut Vec<Tuple>,
            f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
        ) -> Result<ControlFlow<()>> {
            if idx == ordered.len() {
                let head = ground(&self.head, bound);
                let arg = if want_premises {
                    Premises::Rows(premises)
                } else {
                    Premises::NotRequested
                };
                return f(Cow::Owned(head.into_vec()), arg);
            }
            let (body_pos, l) = ordered[idx];
            if l.is_negated() {
                let probe = ground(&l.args, bound);
                if self.negated_probe_hits(stores, l.rel, &probe)? {
                    return Ok(ControlFlow::Continue(()));
                }
                return self.stream_join(
                    stores,
                    delta_from,
                    want_premises,
                    ordered,
                    idx + 1,
                    bound,
                    premises,
                    f,
                );
            }
            // This literal's OWN occurrence — its position in the original
            // body, stable across the positive/negated reordering above —
            // must match `delta_from` exactly; a different occurrence of
            // the SAME relation reads the total, per the seam contract.
            let is_delta = delta_from == Some(AtomOccurrence(body_pos));
            let rows = self.rows_of(stores, l.rel, is_delta)?;
            for row in &rows {
                if let Some(b) = unify(&l.args, row.as_slice(), bound) {
                    if want_premises {
                        premises.push(row.clone());
                    }
                    let cf = self.stream_join(
                        stores,
                        delta_from,
                        want_premises,
                        ordered,
                        idx + 1,
                        &b,
                        premises,
                        f,
                    )?;
                    if want_premises {
                        premises.pop();
                    }
                    if cf.is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
            Ok(ControlFlow::Continue(()))
        }
    }

    impl crate::exec::fixpoint::eval::seal::Sealed for ModelBody {}

    impl RuleBody for ModelBody {
        fn for_each_derivation(
            &self,
            stores: &BTreeMap<MagicSymbol, EpochStore>,
            delta_from: Option<AtomOccurrence>,
            want_premises: bool,
            f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
        ) -> Result<()> {
            let mut ordered: Vec<(usize, &Literal)> = self
                .body
                .iter()
                .enumerate()
                .filter(|(_, l)| !l.is_negated())
                .collect();
            ordered.extend(self.body.iter().enumerate().filter(|(_, l)| l.is_negated()));
            let mut premises: Vec<Tuple> = Vec::new();
            // The driver ignores the break/continue verdict: a break here
            // just means the visitor stopped early, which is fine at the top.
            let _ = self.stream_join(
                stores,
                delta_from,
                want_premises,
                &ordered,
                0,
                &Bindings::new(),
                &mut premises,
                f,
            )?;
            Ok(())
        }

        fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
            &self.contained
        }
    }

    struct ModelFixed {
        inputs: Vec<Rel>,
        eval: fn(&[BTreeSet<Tuple>]) -> BTreeSet<Tuple>,
        facts: Arc<BTreeMap<Rel, BTreeSet<Tuple>>>,
        idb: Arc<BTreeSet<Rel>>,
    }

    impl FixedRuleEval for ModelFixed {
        fn run(
            &self,
            stores: &BTreeMap<MagicSymbol, EpochStore>,
            out: &mut RegularTempStore,
            _budget: &Budget,
            _baseline: u64,
        ) -> Result<()> {
            let inputs: Vec<BTreeSet<Tuple>> = self
                .inputs
                .iter()
                .map(|rel| -> Result<BTreeSet<Tuple>> {
                    if self.idb.contains(rel) {
                        Ok(collect_materialized(
                            store_of(stores, &muggle(rel))?.all_iter()?,
                        ))
                    } else {
                        Ok(self.facts.get(rel).cloned().unwrap_or_default())
                    }
                })
                .collect::<Result<_>>()?;
            for row in (self.eval)(&inputs) {
                out.put(row);
            }
            Ok(())
        }
    }

    // ── the harness compiler: oracle model → EvalProgram ────────────────
    //
    // Stratum assignment duplicates the oracle's Bellman-Ford (the oracle
    // is sealed; its `strata` is private, and this scaffolding must not
    // depend on the judge's internals anyway). An extra final stratum
    // holds the entry `?[vars…] := target[vars…]`.

    struct HeadClass {
        has_aggr: bool,
        is_meet: bool,
    }

    fn head_classes(program: &Program) -> HashMap<Rel, HeadClass> {
        let mut per_head: HashMap<Rel, Vec<&Rule>> = HashMap::new();
        for rule in &program.rules {
            per_head.entry(rule.head_rel).or_default().push(rule);
        }
        per_head
            .into_iter()
            .map(|(rel, rules)| {
                let has_aggr = rules.iter().any(|r| r.aggr.iter().any(|a| a.is_aggregated()));
                let is_meet = has_aggr
                    && rules.iter().all(|r| {
                        r.aggr.iter().all(|a| match a.as_aggregated() {
                            None => true,
                            Some((aggregation, _)) => aggregation.is_meet(),
                        })
                    });
                (rel, HeadClass { has_aggr, is_meet })
            })
            .collect()
    }

    /// head → dependency, `forcing` when the dependency must be complete
    /// strictly below (transcribed from the oracle's edge rules).
    fn dependency_edges(program: &Program) -> Vec<(Rel, Rel, bool)> {
        let classes = head_classes(program);
        let fixed_heads: BTreeSet<Rel> = program.fixed.iter().map(|f| f.head_rel).collect();
        let is_meet = |rel: Rel| classes.get(rel).is_some_and(|c| c.is_meet);
        let mut edges = Vec::new();
        for rule in &program.rules {
            let head = rule.head_rel;
            let class = &classes[&head];
            for l in &rule.body {
                let forcing = if class.has_aggr {
                    if class.is_meet && l.rel == head {
                        l.is_negated()
                    } else {
                        true
                    }
                } else {
                    l.is_negated() || fixed_heads.contains(l.rel) || is_meet(l.rel)
                };
                edges.push((head, l.rel, forcing));
            }
        }
        for f in &program.fixed {
            for dep in &f.inputs {
                edges.push((f.head_rel, *dep, true));
            }
        }
        edges
    }

    fn strata_of(program: &Program) -> HashMap<Rel, usize> {
        let edges = dependency_edges(program);
        let mut s: HashMap<Rel, usize> = HashMap::new();
        for rule in &program.rules {
            s.insert(rule.head_rel, 0);
            for l in &rule.body {
                s.insert(l.rel, 0);
            }
        }
        for f in &program.fixed {
            s.insert(f.head_rel, 0);
            for i in &f.inputs {
                s.insert(i, 0);
            }
        }
        for rel in program.facts.keys() {
            s.insert(rel, 0);
        }
        let bound = s.len() + 1;
        for _ in 0..bound {
            let mut changed = false;
            for (head, dep, forcing) in &edges {
                let need = s[dep] + usize::from(*forcing);
                if s[head] < need {
                    s.insert(head, need);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        s
    }

    const ENTRY_VARS: [&str; 8] = ["v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7"];

    struct Compiled {
        program: EvalProgram<ModelBody, ModelFixed>,
        lifetimes: StoreLifetimes,
    }

    /// Compile the oracle model for evaluation with `target` as the entry
    /// (`?[vars…] := target[vars…]`). `fixed_arities` declares the output
    /// arity of each fixed head (opaque to the model).
    fn compile_for(
        model: &Program,
        target: Rel,
        target_arity: usize,
        fixed_arities: &BTreeMap<Rel, usize>,
    ) -> Compiled {
        let idb: Arc<BTreeSet<Rel>> = Arc::new(
            model
                .rules
                .iter()
                .map(|r| r.head_rel)
                .chain(model.fixed.iter().map(|f| f.head_rel))
                .collect(),
        );
        for rel in idb.iter() {
            assert!(
                !model.facts.contains_key(rel),
                "harness limitation: facts under rule head {rel} (the real \
                 engine separates stored relations from rules)"
            );
        }
        let facts = Arc::new(model.facts.clone());
        let strata_map = strata_of(model);
        // The entry sits strictly above the whole program, in its own final
        // stratum (EvalProgram's construction proof demands it).
        let entry_stratum = strata_map.values().copied().max().unwrap_or(0) + 1;

        let mut strata: Vec<EvalStratum<ModelBody, ModelFixed>> = (0..=entry_stratum)
            .map(|_| EvalStratum::default())
            .collect();
        let mut lifetimes = StoreLifetimes::default();

        // Group inline rules per head, preserving rule order within a head.
        let mut heads_in_order: Vec<Rel> = Vec::new();
        let mut per_head: BTreeMap<Rel, Vec<&Rule>> = BTreeMap::new();
        for rule in &model.rules {
            if !per_head.contains_key(rule.head_rel) {
                heads_in_order.push(rule.head_rel);
            }
            per_head.entry(rule.head_rel).or_default().push(rule);
        }
        for head in heads_in_order {
            let rules = &per_head[head];
            let stratum = strata_map[head];
            let bodies: Vec<ModelBody> = rules
                .iter()
                .map(|r| {
                    ModelBody::new(
                        r.head_args.clone(),
                        r.body.clone(),
                        facts.clone(),
                        idb.clone(),
                    )
                })
                .collect();
            for body in &bodies {
                for dep in body.contained_rules().values() {
                    lifetimes.note_use(dep.clone(), stratum);
                }
            }
            let rule_set =
                EvalRuleSet::new(rules[0].aggr.clone(), bodies).expect("well-shaped rule set");
            strata[stratum]
                .defs
                .insert(muggle(head), EvalDefinition::Rules(rule_set));
        }
        for f in &model.fixed {
            let stratum = strata_map[f.head_rel];
            for input in &f.inputs {
                if idb.contains(input) {
                    lifetimes.note_use(muggle(input), stratum);
                }
            }
            strata[stratum].defs.insert(
                muggle(f.head_rel),
                EvalDefinition::Fixed {
                    // No silent arity default: a wrong arity makes the entry
                    // copy unify nothing, masking disagreements (see
                    // `model_arities`).
                    arity: fixed_arities.get(f.head_rel).copied().unwrap_or_else(|| {
                        panic!("fixed head {} missing from fixed_arities", f.head_rel)
                    }),
                    rule: ModelFixed {
                        inputs: f.inputs.clone(),
                        eval: f.eval,
                        facts: facts.clone(),
                        idb: idb.clone(),
                    },
                },
            );
        }
        // The entry: ?[v0..vn] := target[v0..vn].
        let vars: Vec<Term> = ENTRY_VARS[..target_arity]
            .iter()
            .copied()
            .map(Term::Var)
            .collect();
        let entry_body = ModelBody::new(
            vars.clone(),
            vec![lit(target, vars, false)],
            facts.clone(),
            idb.clone(),
        );
        lifetimes.note_use(muggle(target), entry_stratum);
        let entry_set = EvalRuleSet::new(
            std::iter::repeat_n(None, target_arity).collect(),
            vec![entry_body],
        )
        .expect("entry rule set");
        strata[entry_stratum]
            .defs
            .insert(entry_symbol(), EvalDefinition::Rules(entry_set));

        let program = EvalProgram::from_execution_order(strata).expect("entry in final stratum");
        Compiled { program, lifetimes }
    }

    fn real_eval(
        model: &Program,
        target: Rel,
        target_arity: usize,
        fixed_arities: &BTreeMap<Rel, usize>,
        budget: &Budget,
    ) -> Result<BTreeSet<Tuple>> {
        let compiled = compile_for(model, target, target_arity, fixed_arities);
        let outcome = stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            no_limit(),
            budget,
            None,
        )?;
        Ok(collect_materialized(outcome.store.all_iter()?))
    }

    /// Relation arities derived from the MODEL alone (rule heads and
    /// literal usage), never from oracle output. This is what keeps the
    /// judge sound: an oracle-empty relation used to default to arity 1,
    /// making the entry copy silently unify nothing — any over-derivation
    /// into such a relation was invisible (a vacuous pass).
    fn model_arities(model: &Program) -> BTreeMap<Rel, usize> {
        fn note(arities: &mut BTreeMap<Rel, usize>, rel: Rel, n: usize) {
            match arities.entry(rel) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(n);
                }
                std::collections::btree_map::Entry::Occupied(o) => {
                    assert_eq!(*o.get(), n, "model uses '{rel}' at two arities");
                }
            }
        }
        let mut arities = BTreeMap::new();
        for r in &model.rules {
            note(&mut arities, r.head_rel, r.head_args.len());
            for l in &r.body {
                note(&mut arities, l.rel, l.args.len());
            }
        }
        for (rel, rows) in &model.facts {
            if let Some(t) = rows.first() {
                note(&mut arities, rel, t.len());
            }
        }
        arities
    }

    /// THE differential: every IDB relation of the model, evaluated by the
    /// real semi-naive engine, must equal the sealed oracle's answer.
    fn assert_matches_oracle(model: &Program) {
        let oracle_db = naive_eval(model).expect("oracle accepts the program");
        let idb: BTreeSet<Rel> = model
            .rules
            .iter()
            .map(|r| r.head_rel)
            .chain(model.fixed.iter().map(|f| f.head_rel))
            .collect();
        let arities = model_arities(model);
        let arity_of = |rel: Rel| -> usize {
            arities.get(rel).copied().unwrap_or_else(|| {
                panic!(
                    "harness limitation: '{rel}' is never used at a known arity \
                     in the model (an unreferenced fixed head?) — reference it \
                     in a rule so the judge knows its shape"
                )
            })
        };
        let fixed_arities: BTreeMap<Rel, usize> = model
            .fixed
            .iter()
            .map(|f| (f.head_rel, arity_of(f.head_rel)))
            .collect();
        for rel in idb {
            let oracle_rows = oracle_db.get(rel).cloned().unwrap_or_default();
            let arity = arity_of(rel);
            if let Some(first) = oracle_rows.iter().next() {
                assert_eq!(first.len(), arity, "oracle/model arity drift on '{rel}'");
            }
            let real_rows = real_eval(model, rel, arity, &fixed_arities, &generous_budget())
                .unwrap_or_else(|e| panic!("real eval failed for {rel}: {e:?}"));
            assert_eq!(
                real_rows, oracle_rows,
                "FINDING: real eval disagrees with the oracle on '{rel}'"
            );
        }
    }

    // ── shared program shapes ────────────────────────────────────────────

    fn edge_facts(edges: &[(i64, i64)]) -> BTreeMap<Rel, BTreeSet<Tuple>> {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = Default::default();
        facts.insert(
            "edge",
            edges
                .iter()
                .map(|(a, b)| vec![v(*a), v(*b)])
                .map(Tuple::from_vec)
                .collect(),
        );
        facts
    }

    fn transitive_closure() -> Vec<Rule> {
        vec![
            Rule::plain(
                "path",
                vec![x(), y()],
                vec![lit("edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "path",
                vec![x(), y()],
                vec![
                    lit("edge", vec![x(), z()], false),
                    lit("path", vec![z(), y()], false),
                ],
            ),
        ]
    }

    /// TC by self-join: `path` appears twice in the recursive body, so its
    /// multiplicity is Many and every changed epoch forces a complete run.
    fn transitive_closure_self_join() -> Vec<Rule> {
        vec![
            Rule::plain(
                "path",
                vec![x(), y()],
                vec![lit("edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "path",
                vec![x(), z()],
                vec![
                    lit("path", vec![x(), y()], false),
                    lit("path", vec![y(), z()], false),
                ],
            ),
        ]
    }

    fn meet_reach_rules(aggr_name: &str) -> Vec<Rule> {
        vec![
            Rule::aggregated(
                "m",
                vec![x(), y()],
                vec![HeadAggrSlot::Plain, named(aggr_name)],
                vec![lit("seed", vec![x(), y()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y(), z()],
                vec![HeadAggrSlot::Plain, named(aggr_name)],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![x(), z()], false),
                ],
            ),
        ]
    }

    /// The mirror image of [`meet_reach_rules`]: the exact same meet
    /// recursion, but with the meet column at position **0** and the
    /// grouping node at position 1 — a non-suffix layout
    /// [`MeetAggrStore`] represents via positional [`MeetLayout`].
    /// `m[val, node]` reads back as "node → folded value", so the recursive
    /// body reads `m[z, x]` (value first, node second). The oracle groups
    /// by position, so `assert_matches_oracle` judges this against the same
    /// fixpoint as the suffix form.
    fn meet_reach_rules_pos0(aggr_name: &str) -> Vec<Rule> {
        vec![
            Rule::aggregated(
                "m",
                vec![y(), x()],
                vec![named(aggr_name), HeadAggrSlot::Plain],
                vec![lit("seed", vec![x(), y()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![z(), y()],
                vec![named(aggr_name), HeadAggrSlot::Plain],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![z(), x()], false),
                ],
            ),
        ]
    }

    // ── fixed-case differentials ─────────────────────────────────────────

    #[test]
    fn differential_transitive_closure() {
        assert_matches_oracle(&Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4), (4, 2)]),
            ..Program::default()
        });
    }

    #[test]
    fn differential_self_join_many_multiplicity() {
        assert_matches_oracle(&Program {
            rules: transitive_closure_self_join(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4), (5, 6)]),
            ..Program::default()
        });
    }

    #[test]
    fn differential_stratified_negation() {
        let mut facts = edge_facts(&[(1, 2), (2, 3)]);
        facts.insert(
            "node",
            (1..=3).map(|i| vec![v(i)]).map(Tuple::from_vec).collect(),
        );
        let mut rules = transitive_closure();
        rules.push(Rule::plain(
            "unreachable",
            vec![x(), y()],
            vec![
                lit("node", vec![x()], false),
                lit("node", vec![y()], false),
                lit("path", vec![x(), y()], true),
            ],
        ));
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    #[test]
    fn differential_normal_aggregation_over_recursion() {
        let mut rules = transitive_closure();
        rules.push(Rule::aggregated(
            "reach_count",
            vec![x(), y()],
            vec![HeadAggrSlot::Plain, named("count")],
            vec![lit("path", vec![x(), y()], false)],
        ));
        assert_matches_oracle(&Program {
            rules,
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4)]),
            ..Program::default()
        });
    }

    #[test]
    fn differential_normal_aggregation_empty_fold() {
        // Every position aggregated over no rows: the single empty-fold row.
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert("nothing", BTreeSet::new());
        assert_matches_oracle(&Program {
            rules: vec![Rule::aggregated(
                "c",
                vec![x(), x()],
                vec![named("count"), named("sum")],
                vec![lit("nothing", vec![x()], false)],
            )],
            facts,
            ..Program::default()
        });
    }

    #[test]
    fn differential_meet_recursion_min_on_cycle() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1), (3, 4)]);
        facts.insert(
            "seed",
            [(1, 5), (4, 1)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .map(Tuple::from_vec)
                .collect(),
        );
        assert_matches_oracle(&Program {
            rules: meet_reach_rules("min"),
            facts,
            ..Program::default()
        });
    }

    /// The and/or END-TO-END differential: the exact propagation shape on
    /// which the original's inverted changed-flag reached a premature
    /// fixpoint one hop short (laws.rs pins the store-level half; this
    /// runs the real evaluator through the landed stores and must reach
    /// the oracle's full fixpoint).
    #[test]
    fn differential_and_or_propagation_end_to_end() {
        for (name, seed_of) in [("or", [true, false, false]), ("and", [false, true, true])] {
            let mut facts = edge_facts(&[(1, 2), (2, 3)]);
            facts.insert(
                "seed",
                (1..=3)
                    .map(|k| vec![v(k), DataValue::from(seed_of[(k - 1) as usize])])
                    .map(Tuple::from_vec)
                    .collect(),
            );
            let model = Program {
                rules: meet_reach_rules(name),
                facts,
                ..Program::default()
            };
            assert_matches_oracle(&model);
            // And explicitly: node 3 must have flipped (the premature
            // fixpoint stranded it at its seed).
            let real = real_eval(&model, "m", 2, &BTreeMap::new(), &generous_budget()).unwrap();
            let fixpoint = name == "or";
            assert!(
                real.contains(&Tuple::from_vec(vec![v(3), DataValue::from(fixpoint)])),
                "{name}: node 3 must reach the fixpoint value"
            );
        }
    }

    // ── non-suffix meet layouts: the capability the refusal used to deny ──

    /// The `min` recursion on a cycle, but with the meet column at position
    /// 0 (grouping node at position 1). Same fixpoint as
    /// `differential_meet_recursion_min_on_cycle`, judged positionally.
    #[test]
    fn differential_meet_pos0_recursion_min_on_cycle() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1), (3, 4)]);
        facts.insert(
            "seed",
            [(1, 5), (4, 1)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .map(Tuple::from_vec)
                .collect(),
        );
        assert_matches_oracle(&Program {
            rules: meet_reach_rules_pos0("min"),
            facts,
            ..Program::default()
        });
    }

    /// The and/or premature-fixpoint case (the inverted changed-flag class)
    /// at a **non-suffix** position: the meet column is position 0, so a
    /// changed-flag bug or a mis-projected group key would strand node 3 at
    /// its seed exactly as the suffix form did. Mirrors
    /// `differential_and_or_propagation_end_to_end`.
    #[test]
    fn differential_and_or_pos0_propagation_end_to_end() {
        for (name, seed_of) in [("or", [true, false, false]), ("and", [false, true, true])] {
            let mut facts = edge_facts(&[(1, 2), (2, 3)]);
            facts.insert(
                "seed",
                (1..=3)
                    .map(|k| vec![v(k), DataValue::from(seed_of[(k - 1) as usize])])
                    .map(Tuple::from_vec)
                    .collect(),
            );
            let model = Program {
                rules: meet_reach_rules_pos0(name),
                facts,
                ..Program::default()
            };
            assert_matches_oracle(&model);
            // Node 3 (now at head position 1) must have flipped to the
            // fixpoint value carried in head position 0.
            let real = real_eval(&model, "m", 2, &BTreeMap::new(), &generous_budget()).unwrap();
            let fixpoint = name == "or";
            assert!(
                real.contains(&Tuple::from_vec(vec![DataValue::from(fixpoint), v(3)])),
                "{name}: node 3 must reach the fixpoint value at a non-suffix position"
            );
        }
    }

    /// Two meet columns split apart by a grouping column (val positions
    /// [0, 2], key position [1]): for each group `K`, position 0 folds the
    /// minimum and position 2 the maximum of the observed values. Exercises
    /// the store's interleave rebuilding a 3-tuple from split projections.
    #[test]
    fn differential_meet_interleaved_split_columns() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "obs",
            [(1, 5), (1, 2), (1, 8), (2, 4), (2, 7), (3, 3)]
                .iter()
                .map(|(k, val)| vec![v(*k), v(*val)])
                .map(Tuple::from_vec)
                .collect(),
        );
        // g[min(V), K, max(V)] :- obs[K, V].
        let rules = vec![Rule::aggregated(
            "g",
            vec![Term::Var("V"), Term::Var("K"), Term::Var("V")],
            vec![named("min"), HeadAggrSlot::Plain, named("max")],
            vec![lit("obs", vec![Term::Var("K"), Term::Var("V")], false)],
        )];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    /// A recursive meet with the grouping column between two meet columns
    /// (key position [1], val positions [0, 2]) — meet-in-recursion at a
    /// genuinely interleaved layout, not merely a swapped pair.
    #[test]
    fn differential_meet_interleaved_recursion() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1)]);
        facts.insert(
            "seed",
            [(1, 5, 5), (2, 1, 1)]
                .iter()
                .map(|(k, lo, hi)| vec![v(*k), v(*lo), v(*hi)])
                .map(Tuple::from_vec)
                .collect(),
        );
        // m[min(Lo), K, max(Hi)] seeded, then relaxed along edges: each hop
        // carries the source group's folded (min, max) to the target node.
        let rules = vec![
            Rule::aggregated(
                "m",
                vec![Term::Var("Lo"), Term::Var("K"), Term::Var("Hi")],
                vec![named("min"), HeadAggrSlot::Plain, named("max")],
                vec![lit(
                    "seed",
                    vec![Term::Var("K"), Term::Var("Lo"), Term::Var("Hi")],
                    false,
                )],
            ),
            Rule::aggregated(
                "m",
                vec![Term::Var("Lo"), Term::Var("T"), Term::Var("Hi")],
                vec![named("min"), HeadAggrSlot::Plain, named("max")],
                vec![
                    lit("edge", vec![Term::Var("S"), Term::Var("T")], false),
                    lit(
                        "m",
                        vec![Term::Var("Lo"), Term::Var("S"), Term::Var("Hi")],
                        false,
                    ),
                ],
            ),
        ];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    #[test]
    fn differential_meet_identity_row_feeds_recursion() {
        // No seeds: the identity `false` matches edge(false, true) and the
        // recursion derives true (laws::meet_identity_row_feeds_recursion).
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "edge",
            [vec![DataValue::from(false), DataValue::from(true)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect(),
        );
        facts.insert("seed", BTreeSet::new());
        let rules = vec![
            Rule::aggregated(
                "m",
                vec![x()],
                vec![named("or")],
                vec![lit("seed", vec![x()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y()],
                vec![named("or")],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![x()], false),
                ],
            ),
        ];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    #[test]
    fn differential_negation_reads_completed_meet_relation() {
        let mut facts = edge_facts(&[(1, 2)]);
        facts.insert(
            "seed",
            [vec![v(1), DataValue::from(true)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect(),
        );
        facts.insert(
            "node",
            (1..=3).map(|i| vec![v(i)]).map(Tuple::from_vec).collect(),
        );
        let mut rules = meet_reach_rules("or");
        rules.push(Rule::plain(
            "unseeded",
            vec![x()],
            vec![
                lit("node", vec![x()], false),
                lit("m", vec![x(), Term::Const(DataValue::from(true))], true),
            ],
        ));
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    #[test]
    fn differential_fixed_rules_on_stratum_boundaries() {
        let constant_edges = FixedRule {
            head_rel: "gen_edge",
            inputs: vec![],
            eval: |_| {
                [(1, 2), (2, 3)]
                    .iter()
                    .map(|(a, b)| vec![v(*a), v(*b)])
                    .map(Tuple::from_vec)
                    .collect()
            },
        };
        let path_sources = FixedRule {
            head_rel: "sources",
            inputs: vec!["path"],
            eval: |inputs| {
                inputs[0]
                    .iter()
                    .map(|t| vec![t[0].clone()])
                    .map(Tuple::from_vec)
                    .collect()
            },
        };
        let rules = vec![
            Rule::plain(
                "path",
                vec![x(), y()],
                vec![lit("gen_edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "path",
                vec![x(), y()],
                vec![
                    lit("gen_edge", vec![x(), z()], false),
                    lit("path", vec![z(), y()], false),
                ],
            ),
            Rule::plain("out", vec![x()], vec![lit("sources", vec![x()], false)]),
        ];
        assert_matches_oracle(&Program {
            rules,
            fixed: vec![constant_edges, path_sources],
            ..Program::default()
        });
    }

    /// Mutual recursion: p and q derive each other inside one stratum —
    /// a shape neither the fixed suite nor the generator produced before
    /// this review.
    #[test]
    fn differential_mutual_recursion() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 4)]);
        facts.insert(
            "edge2",
            [(2, 5), (5, 3), (4, 1)]
                .iter()
                .map(|(a, b)| vec![v(*a), v(*b)])
                .map(Tuple::from_vec)
                .collect(),
        );
        let rules = vec![
            Rule::plain(
                "p",
                vec![x(), y()],
                vec![lit("edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "p",
                vec![x(), z()],
                vec![
                    lit("q", vec![x(), y()], false),
                    lit("edge", vec![y(), z()], false),
                ],
            ),
            Rule::plain(
                "q",
                vec![x(), z()],
                vec![
                    lit("p", vec![x(), y()], false),
                    lit("edge2", vec![y(), z()], false),
                ],
            ),
        ];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    /// One body joining TWO recursive stores that both carry deltas in the
    /// same epochs: r(x,z) :- path(x,y), path2(y,z), with r recursive too.
    /// Kills any truncation of the per-delta iteration (each contained key
    /// must contribute its delta×total combinations).
    #[test]
    fn differential_two_delta_carrying_deps_in_one_body() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 4)]);
        facts.insert(
            "edge2",
            [(4, 5), (5, 6), (6, 7)]
                .iter()
                .map(|(a, b)| vec![v(*a), v(*b)])
                .map(Tuple::from_vec)
                .collect(),
        );
        let mut rules = transitive_closure(); // path = TC(edge)
        rules.push(Rule::plain(
            "path2",
            vec![x(), y()],
            vec![lit("edge2", vec![x(), y()], false)],
        ));
        rules.push(Rule::plain(
            "path2",
            vec![x(), y()],
            vec![
                lit("edge2", vec![x(), z()], false),
                lit("path2", vec![z(), y()], false),
            ],
        ));
        rules.push(Rule::plain(
            "r",
            vec![x(), z()],
            vec![
                lit("path", vec![x(), y()], false),
                lit("path2", vec![y(), z()], false),
            ],
        ));
        rules.push(Rule::plain(
            "r",
            vec![x(), z()],
            vec![
                lit("r", vec![x(), y()], false),
                lit("path2", vec![y(), z()], false),
            ],
        ));
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    /// A meet head whose body mentions its own store TWICE positively:
    /// multiplicity Many, so every changed epoch takes
    /// `incremental_meet_eval`'s complete-run branch — dead code under the
    /// previous suite (the review's surviving mutant M6). Values propagate
    /// around the cycle only through complete re-runs; gutting the branch
    /// freezes every node at its seed.
    #[test]
    fn differential_meet_self_join_many_multiplicity() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1)]);
        facts.insert(
            "seed",
            [(1, 5), (2, 7), (3, 9)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .map(Tuple::from_vec)
                .collect(),
        );
        let rules = vec![
            Rule::aggregated(
                "m",
                vec![x(), y()],
                vec![HeadAggrSlot::Plain, named("min")],
                vec![lit("seed", vec![x(), y()], false)],
            ),
            // m(x, min w) :- m(x, _), m(w', w), edge(w', x): node x adopts
            // any predecessor's value; m appears twice → Many.
            Rule::aggregated(
                "m",
                vec![x(), z()],
                vec![HeadAggrSlot::Plain, named("min")],
                vec![
                    lit("m", vec![x(), y()], false),
                    lit("m", vec![Term::Var("W"), z()], false),
                    lit("edge", vec![Term::Var("W"), x()], false),
                ],
            ),
        ];
        let model = Program {
            rules,
            facts,
            ..Program::default()
        };
        assert_matches_oracle(&model);
        // And explicitly: the cycle must drain every node to the global
        // minimum (a frozen incremental path strands nodes at their seed).
        let real = real_eval(&model, "m", 2, &BTreeMap::new(), &generous_budget()).unwrap();
        for node in 1..=3 {
            assert!(
                real.contains(&Tuple::from_vec(vec![v(node), v(5)])),
                "node {node} must reach the cycle minimum 5, got {real:?}"
            );
        }
    }

    /// Two recursions that converge at different epochs inside ONE
    /// stratum: `a_long` (8-hop chain, ~8 epochs) and `z_short` (2-hop
    /// chain, done by epoch 2), named so the early converger merges LAST
    /// at the barrier. Pins fixpoint detection as the accumulation over
    /// every store's delta — `changed = has_delta()` of the last store
    /// (instead of `|=`) exits the stratum epochs early and truncates the
    /// long closure. Previously only the randomized differential could
    /// catch that mutation.
    #[test]
    fn differential_two_recursions_converge_independently() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "long_edge",
            (0..8i64)
                .map(|i| vec![v(i), v(i + 1)])
                .map(Tuple::from_vec)
                .collect(),
        );
        facts.insert(
            "short_edge",
            [(100, 101), (101, 102)]
                .iter()
                .map(|(a, b)| vec![v(*a), v(*b)])
                .map(Tuple::from_vec)
                .collect(),
        );
        let rules = vec![
            Rule::plain(
                "a_long",
                vec![x(), y()],
                vec![lit("long_edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "a_long",
                vec![x(), z()],
                vec![
                    lit("long_edge", vec![x(), y()], false),
                    lit("a_long", vec![y(), z()], false),
                ],
            ),
            Rule::plain(
                "z_short",
                vec![x(), y()],
                vec![lit("short_edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "z_short",
                vec![x(), z()],
                vec![
                    lit("short_edge", vec![x(), y()], false),
                    lit("z_short", vec![y(), z()], false),
                ],
            ),
        ];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    // ── the randomized differential ──────────────────────────────────────

    // Shapes the generator still cannot produce, each pinned by a fixed
    // differential where one exists:
    // - meet self-join / Many-multiplicity meet heads
    //   (differential_meet_self_join_many_multiplicity);
    // - a recursive entry under `:limit`
    //   (limiter_incremental_entry_recursion_dedups_and_overshoots);
    // - meet heads with ≥2 grouping or ≥2 aggregated positions inside
    //   recursion (identity-row shape tested non-recursively only);
    // - aggregations with arguments (`named` always passes empty args);
    // - fixed rules (differential_fixed_rules_on_stratum_boundaries only);
    // - negation over meet stores
    //   (differential_negation_reads_completed_meet_relation only);
    // - witness recording during differentials (witness paths are
    //   exercised by the dedicated provenance and determinism tests only).
    #[derive(Debug, Clone)]
    struct GenCase {
        n: i64,
        edges: BTreeSet<(i64, i64)>,
        seeds: BTreeMap<i64, DataValue>,
        aggr_name: &'static str,
        self_join: bool,
        negation: bool,
        normal_aggr: bool,
        /// Add a mutually recursive pair qa/qb (same stratum as path).
        mutual: bool,
        /// Add pj, whose body joins TWO delta-carrying stores (path, qa);
        /// implies the qa/qb pair.
        two_dep: bool,
    }

    fn arb_case() -> BoxedStrategy<GenCase> {
        let aggr = prop_oneof![
            Just("or"),
            Just("and"),
            Just("min"),
            Just("max"),
            Just("union"),
        ];
        (
            2i64..=5,
            aggr,
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_flat_map(
                |(n, aggr_name, self_join, negation, normal_aggr, mutual, two_dep)| {
                    let value: BoxedStrategy<DataValue> = match aggr_name {
                        "or" | "and" => any::<bool>().prop_map(DataValue::from).boxed(),
                        "union" => {
                            prop::collection::btree_set((0i64..4).prop_map(DataValue::from), 0..3)
                                .prop_map(DataValue::Set)
                                .boxed()
                        }
                        _ => (-10i64..10).prop_map(DataValue::from).boxed(),
                    };
                    (
                        prop::collection::btree_set((0..n, 0..n), 0..10),
                        prop::collection::btree_map(0..n, value, 0..=(n as usize)),
                    )
                        .prop_map(move |(edges, seeds)| GenCase {
                            n,
                            edges,
                            seeds,
                            aggr_name,
                            self_join,
                            negation,
                            normal_aggr,
                            mutual,
                            two_dep,
                        })
                },
            )
            .boxed()
    }

    fn build_case(case: &GenCase) -> Program {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "edge",
            case.edges
                .iter()
                .map(|(a, b)| vec![v(*a), v(*b)])
                .map(Tuple::from_vec)
                .collect(),
        );
        facts.insert(
            "seed",
            case.seeds
                .iter()
                .map(|(k, val)| vec![v(*k), val.clone()])
                .map(Tuple::from_vec)
                .collect(),
        );
        facts.insert(
            "node",
            (0..case.n)
                .map(|i| vec![v(i)])
                .map(Tuple::from_vec)
                .collect(),
        );
        let mut rules = if case.self_join {
            transitive_closure_self_join()
        } else {
            transitive_closure()
        };
        rules.extend(meet_reach_rules(case.aggr_name));
        rules.push(Rule::plain(
            "out",
            vec![x(), y()],
            vec![lit("m", vec![x(), y()], false)],
        ));
        if case.negation {
            rules.push(Rule::plain(
                "unreachable",
                vec![x(), y()],
                vec![
                    lit("node", vec![x()], false),
                    lit("node", vec![y()], false),
                    lit("path", vec![x(), y()], true),
                ],
            ));
        }
        if case.normal_aggr {
            rules.push(Rule::aggregated(
                "reach_count",
                vec![x(), y()],
                vec![HeadAggrSlot::Plain, named("count")],
                vec![lit("path", vec![x(), y()], false)],
            ));
        }
        if case.mutual || case.two_dep {
            // Mutual recursion: qa and qb derive each other, sharing
            // stratum 0 with path.
            rules.push(Rule::plain(
                "qa",
                vec![x(), y()],
                vec![lit("edge", vec![x(), y()], false)],
            ));
            rules.push(Rule::plain(
                "qa",
                vec![x(), z()],
                vec![
                    lit("qb", vec![x(), y()], false),
                    lit("edge", vec![y(), z()], false),
                ],
            ));
            rules.push(Rule::plain(
                "qb",
                vec![x(), z()],
                vec![
                    lit("qa", vec![x(), y()], false),
                    lit("edge", vec![y(), z()], false),
                ],
            ));
        }
        if case.two_dep {
            // One body joining two delta-carrying stores (path and qa both
            // change while pj is being derived), plus pj-recursion.
            rules.push(Rule::plain(
                "pj",
                vec![x(), z()],
                vec![
                    lit("path", vec![x(), y()], false),
                    lit("qa", vec![y(), z()], false),
                ],
            ));
            rules.push(Rule::plain(
                "pj",
                vec![x(), z()],
                vec![
                    lit("pj", vec![x(), y()], false),
                    lit("qa", vec![y(), z()], false),
                ],
            ));
        }
        Program {
            rules,
            facts,
            ..Program::default()
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        /// The moment of truth: randomized stratified programs — plain and
        /// self-join recursion, meet recursion over five lattices,
        /// negation, normal aggregation — through the real semi-naive
        /// evaluator and the sealed oracle, relation by relation.
        #[test]
        fn differential_randomized_stratified_programs(case in arb_case()) {
            assert_matches_oracle(&build_case(&case));
        }
    }

    // ── the determinism law ──────────────────────────────────────────────

    fn determinism_case() -> Program {
        let edges: Vec<(i64, i64)> = (0..12).map(|i| (i, (i * 7 + 3) % 12)).collect();
        let mut facts = edge_facts(&edges);
        facts.insert(
            "seed",
            [(0, 9), (5, 2), (11, 4)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .map(Tuple::from_vec)
                .collect(),
        );
        let mut rules = transitive_closure_self_join();
        rules.extend(meet_reach_rules("min"));
        rules.push(Rule::plain(
            "out",
            vec![x(), y()],
            vec![lit("m", vec![x(), y()], false)],
        ));
        Program {
            rules,
            facts,
            ..Program::default()
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn at_thread_count<T: Send>(threads: usize, f: impl FnOnce() -> T + Send) -> T {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("thread pool")
            .install(f)
    }

    /// Same program + facts + budget ⇒ identical result sets AND identical
    /// witness tables at 1/2/4/8 rayon threads.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn determinism_results_and_witnesses_across_thread_counts() {
        let model = determinism_case();
        let run = |threads: usize| -> (BTreeSet<Tuple>, Vec<String>) {
            at_thread_count(threads, || {
                let compiled = compile_for(&model, "path", 2, &BTreeMap::new());
                let mut table = WitnessTable::default();
                let outcome = stratified_evaluate(
                    &compiled.program,
                    &compiled.lifetimes,
                    no_limit(),
                    &generous_budget(),
                    Some(&mut table),
                )
                .expect("evaluates");
                let rows = collect_materialized(outcome.store.all_iter()?)?;
                let witnesses = table
                    .entries()
                    .iter()
                    .map(|w| format!("{w:?}"))
                    .collect_vec();
                (rows, witnesses)
            })
        };
        let baseline = run(1);
        for threads in [2, 4, 8] {
            let got = run(threads);
            assert_eq!(got.0, baseline.0, "result set differs at {threads} threads");
            assert_eq!(
                got.1, baseline.1,
                "witness table differs at {threads} threads"
            );
        }
    }

    /// A meet recursion whose meet column sits at head position 0 (a
    /// non-suffix layout), where group-key order and head-tuple order
    /// diverge. Admissions are reported in group-key order and the store's
    /// two views (`by_group`, `by_row`) must stay in lockstep regardless of
    /// how the parallel epoch schedules its rules — so results AND the
    /// per-group witness table stay byte-identical at 1/2/4/8 threads.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn determinism_nonsuffix_meet_across_thread_counts() {
        let edges: Vec<(i64, i64)> = (0..12).map(|i| (i, (i * 7 + 3) % 12)).collect();
        let mut facts = edge_facts(&edges);
        facts.insert(
            "seed",
            [(0, 9), (5, 2), (11, 4)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .map(Tuple::from_vec)
                .collect(),
        );
        let model = Program {
            rules: meet_reach_rules_pos0("min"),
            facts,
            ..Program::default()
        };
        let run = |threads: usize| -> (BTreeSet<Tuple>, Vec<String>) {
            at_thread_count(threads, || {
                let compiled = compile_for(&model, "m", 2, &BTreeMap::new());
                let mut table = WitnessTable::default();
                let outcome = stratified_evaluate(
                    &compiled.program,
                    &compiled.lifetimes,
                    no_limit(),
                    &generous_budget(),
                    Some(&mut table),
                )
                .expect("evaluates");
                let rows = collect_materialized(outcome.store.all_iter()?)?;
                let witnesses = table
                    .entries()
                    .iter()
                    .map(|w| format!("{w:?}"))
                    .collect_vec();
                (rows, witnesses)
            })
        };
        let baseline = run(1);
        for threads in [2, 4, 8] {
            let got = run(threads);
            assert_eq!(
                got.0, baseline.0,
                "non-suffix meet result set differs at {threads} threads"
            );
            assert_eq!(
                got.1, baseline.1,
                "non-suffix meet witness table differs at {threads} threads"
            );
        }
    }

    /// The refusal half of the law: a budget-exceeding case refuses
    /// byte-identically at every thread count (deterministic dimensions
    /// are checked at the barrier only, so the spend is exact).
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn determinism_budget_refusal_is_byte_identical_across_thread_counts() {
        let model = determinism_case();
        let run = |threads: usize| -> (String, BudgetDimension, u64, u64) {
            at_thread_count(threads, || {
                let compiled = compile_for(&model, "path", 2, &BTreeMap::new());
                let budget = generous_budget().with_derived_tuple_ceiling(20);
                let err = stratified_evaluate(
                    &compiled.program,
                    &compiled.lifetimes,
                    no_limit(),
                    &budget,
                    None,
                )
                .expect_err("must refuse");
                let refusal: &LimitExceeded =
                    err.downcast_ref().expect("typed LimitExceeded refusal");
                (
                    err.to_string(),
                    refusal.dimension,
                    refusal.spent,
                    refusal.ceiling,
                )
            })
        };
        let baseline = run(1);
        assert_eq!(baseline.1, BudgetDimension::DerivedTuples);
        for threads in [2, 4, 8] {
            assert_eq!(
                run(threads),
                baseline,
                "refusal differs at {threads} threads"
            );
        }
    }

    // ── budget refusals ──────────────────────────────────────────────────

    #[test]
    fn epoch_ceiling_refuses_deterministically() {
        // A 30-hop chain needs many epochs; a ceiling of 4 must refuse
        // with the exact typed spend.
        let edges: Vec<(i64, i64)> = (0..30).map(|i| (i, i + 1)).collect();
        let model = Program {
            rules: transitive_closure(),
            facts: edge_facts(&edges),
            ..Program::default()
        };
        let budget = Budget::new(NonZeroU32::new(4).unwrap());
        let err = real_eval(&model, "path", 2, &BTreeMap::new(), &budget).expect_err("refuses");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(
            *refusal,
            LimitExceeded {
                dimension: BudgetDimension::Epochs,
                spent: 4,
                ceiling: 4,
                rule: None,
                span: None,
            }
        );
    }

    #[test]
    fn derived_tuple_ceiling_refuses_with_exact_spend() {
        let model = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4)]),
            ..Program::default()
        };
        // The full closure has 6 tuples; with the entry rule copying it
        // and the base facts admitted too, a ceiling of 3 refuses at the
        // first barrier that crosses it — always the same barrier.
        let budget = generous_budget().with_derived_tuple_ceiling(3);
        let err = real_eval(&model, "path", 2, &BTreeMap::new(), &budget).expect_err("refuses");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(refusal.dimension, BudgetDimension::DerivedTuples);
        assert_eq!(refusal.ceiling, 3);
        assert!(refusal.spent > 3);
        // Deterministic: the same refusal again.
        let err2 = real_eval(&model, "path", 2, &BTreeMap::new(), &budget).expect_err("refuses");
        assert_eq!(err.to_string(), err2.to_string());
    }

    // ── the mid-epoch in-flight ceiling ──────────────────────────────────
    //
    // A rule body whose output stream is a near-cross-product: `a × b`
    // distinct rows in ONE epoch. This is the incident's shape — a single
    // legitimate join that materializes an unbounded intermediate before any
    // epoch barrier can check the derived-tuple ceiling. The `emitted`
    // counter is the materialization high-water mark (it upper-bounds the
    // out-store's size, since every emission is distinct here).
    struct CrossProduct {
        a: i64,
        b: i64,
        emitted: Arc<AtomicUsize>,
        contained: BTreeMap<AtomOccurrence, MagicSymbol>,
    }
    impl CrossProduct {
        fn new(a: i64, b: i64, emitted: Arc<AtomicUsize>) -> Self {
            Self {
                a,
                b,
                emitted,
                contained: BTreeMap::new(),
            }
        }
    }
    impl crate::exec::fixpoint::eval::seal::Sealed for CrossProduct {}

    impl RuleBody for CrossProduct {
        fn for_each_derivation(
            &self,
            _stores: &BTreeMap<MagicSymbol, EpochStore>,
            _delta_from: Option<AtomOccurrence>,
            _want_premises: bool,
            f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
        ) -> Result<()> {
            for i in 0..self.a {
                for j in 0..self.b {
                    self.emitted.fetch_add(1, Ordering::Relaxed);
                    if f(Cow::Owned(vec![v(i), v(j)]), Premises::NotRequested)?.is_break() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        }
        fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
            &self.contained
        }
    }

    fn cross_product_program(
        symb: MagicSymbol,
        a: i64,
        b: i64,
        emitted: Arc<AtomicUsize>,
    ) -> EvalProgram<CrossProduct, NoFixed> {
        let body = CrossProduct::new(a, b, emitted);
        let rule_set = EvalRuleSet::new(vec![HeadAggrSlot::Plain, HeadAggrSlot::Plain], vec![body]).unwrap();
        let mut stratum: EvalStratum<CrossProduct, NoFixed> = EvalStratum::default();
        stratum.defs.insert(symb, EvalDefinition::Rules(rule_set));
        EvalProgram::from_execution_order(vec![stratum]).unwrap()
    }

    /// The core guarantee: a near-cross-product with a small derived-tuple
    /// ceiling refuses **mid-epoch**, before the barrier — and its
    /// materialization never exceeds `ceiling + INTERRUPT_STRIDE`. This is
    /// the hole the incident fell through: without the mid-epoch check the
    /// whole `a × b` intermediate materializes before any barrier fires.
    #[test]
    fn mid_epoch_in_flight_ceiling_refuses_before_barrier() {
        const CEILING: u64 = 100;
        let emitted = Arc::new(AtomicUsize::new(0));
        // 400 × 400 = 160_000 candidate rows if left unchecked.
        let program = cross_product_program(entry_symbol(), 400, 400, emitted.clone());
        let budget = generous_budget().with_derived_tuple_ceiling(CEILING);
        let err = stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            no_limit(),
            &budget,
            None,
        )
        .expect_err("must refuse mid-epoch");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed LimitExceeded");

        // It is the MID-EPOCH dimension, not the barrier's DerivedTuples.
        assert_eq!(refusal.dimension, BudgetDimension::InFlightDerivations);
        assert_eq!(refusal.ceiling, CEILING);
        // The refusal names the offending rule and labels its span.
        assert_eq!(refusal.rule.as_deref(), Some("?"));
        assert_eq!(refusal.span, Some(SourceSpan(0, 0)));
        // Spend crossed the ceiling but only within one stride of slack.
        assert!(refusal.spent > CEILING, "spent {} > ceiling", refusal.spent);
        assert!(
            refusal.spent <= CEILING + INTERRUPT_STRIDE as u64,
            "spend {} must be within a stride of the ceiling",
            refusal.spent
        );

        // THE BOUNDEDNESS PROOF: materialization never exceeded
        // ceiling + stride, though the full product is 160_000. This is the
        // assertion the mutation campaign shows *biting* — remove the
        // mid-epoch check and `emitted` becomes 160_000.
        let emitted = emitted.load(Ordering::Relaxed);
        assert!(
            (emitted as u64) <= CEILING + INTERRUPT_STRIDE as u64 + 1,
            "materialization {emitted} must be bounded by ceiling + stride, \
             not the {} of the full product",
            400 * 400
        );
    }

    /// Requirement 2: the mid-epoch refusal is byte-identical at 1/2/4/8
    /// rayon threads — same message, dimension, spend, ceiling, rule name,
    /// and span. Both terms of the check (barrier baseline; this rule's own
    /// sequential in-flight count) are deterministic, so the refusal is too.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn mid_epoch_refusal_is_byte_identical_across_thread_counts() {
        type Refusal = (
            String,
            BudgetDimension,
            u64,
            u64,
            Option<String>,
            Option<SourceSpan>,
        );
        let run = |threads: usize| -> Refusal {
            at_thread_count(threads, || {
                let emitted = Arc::new(AtomicUsize::new(0));
                let program = cross_product_program(entry_symbol(), 400, 400, emitted);
                let budget = generous_budget().with_derived_tuple_ceiling(100);
                let err = stratified_evaluate(
                    &program,
                    &StoreLifetimes::default(),
                    no_limit(),
                    &budget,
                    None,
                )
                .expect_err("must refuse");
                let r: &LimitExceeded = err.downcast_ref().expect("typed refusal");
                (
                    err.to_string(),
                    r.dimension,
                    r.spent,
                    r.ceiling,
                    r.rule.clone(),
                    r.span,
                )
            })
        };
        let baseline = run(1);
        assert_eq!(baseline.1, BudgetDimension::InFlightDerivations);
        assert_eq!(baseline.4.as_deref(), Some("?"));
        for threads in [2, 4, 8] {
            assert_eq!(
                run(threads),
                baseline,
                "refusal differs at {threads} threads"
            );
        }
    }

    /// When several rules of one stratum each cross the ceiling in parallel,
    /// the reported rule is the canonically-first among them — deterministic
    /// across thread counts, because we never read another in-flight rule's
    /// count. Two non-entry flooders `aaa` and `bbb` both blow the ceiling;
    /// `aaa` (canonically first) is always the one named.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn mid_epoch_refusal_names_canonically_first_tripping_rule() {
        let build = || -> EvalProgram<CrossProduct, NoFixed> {
            let mut s0: EvalStratum<CrossProduct, NoFixed> = EvalStratum::default();
            s0.defs.insert(
                muggle("aaa"),
                EvalDefinition::Rules(
                    EvalRuleSet::new(
                        vec![HeadAggrSlot::Plain, HeadAggrSlot::Plain],
                        vec![CrossProduct::new(400, 400, Arc::new(AtomicUsize::new(0)))],
                    )
                    .unwrap(),
                ),
            );
            s0.defs.insert(
                muggle("bbb"),
                EvalDefinition::Rules(
                    EvalRuleSet::new(
                        vec![HeadAggrSlot::Plain, HeadAggrSlot::Plain],
                        vec![CrossProduct::new(400, 400, Arc::new(AtomicUsize::new(0)))],
                    )
                    .unwrap(),
                ),
            );
            // The entry sits in a later stratum, never reached (stratum 0
            // refuses first), but from_execution_order requires it to exist.
            let mut s1: EvalStratum<CrossProduct, NoFixed> = EvalStratum::default();
            s1.defs.insert(
                entry_symbol(),
                EvalDefinition::Rules(
                    EvalRuleSet::new(
                        vec![HeadAggrSlot::Plain, HeadAggrSlot::Plain],
                        vec![CrossProduct::new(0, 0, Arc::new(AtomicUsize::new(0)))],
                    )
                    .unwrap(),
                ),
            );
            EvalProgram::from_execution_order(vec![s0, s1]).unwrap()
        };
        let run = |threads: usize| -> Option<String> {
            at_thread_count(threads, || {
                let program = build();
                let budget = generous_budget().with_derived_tuple_ceiling(100);
                let err = stratified_evaluate(
                    &program,
                    &StoreLifetimes::default(),
                    no_limit(),
                    &budget,
                    None,
                )
                .expect_err("must refuse");
                let r: &LimitExceeded = err.downcast_ref().expect("typed refusal");
                r.rule.clone()
            })
        };
        for threads in [1, 2, 4, 8] {
            assert_eq!(
                run(threads).as_deref(),
                Some("aaa"),
                "canonically-first tripping rule at {threads} threads"
            );
        }
    }

    // ── the refuted-theorem counterexample, landed as a differential ─────
    //
    // The hostile reviewer refuted the non-perturbation theorem on the MEET
    // path: a min-fold meet recursion over an N-node cycle with all seeds
    // EQUAL re-derives every group unchanged in epoch 1. The old guard ticked
    // `out.len()` (the fresh out-store's resident group count = N), while the
    // barrier admits ZERO of them — so the guard spuriously refused a program
    // the barrier completes, at every ceiling in `[true_spend, baseline+N]`.
    // Fix 1 counts admissions (`meet_put_admission_faithful`), so the guard
    // now fires only where the barrier would. This test sweeps that whole old
    // divergence window and demands byte-identical completion, plus an honest
    // (admitted, not in-flight) `spent` on the one barrier refusal below the
    // true spend. It FAILS on the pre-fix `out.len()` count.

    /// An N-node directed cycle `0→1→…→(N-1)→0`, every node seeded with the
    /// SAME value. `min` propagation never lowers anything, so epoch 1
    /// re-derives all N groups unchanged: resident `out.len() == N`, admitted
    /// `== 0`. This is the reviewer's `hostile_probe_meet_tightslack_low`.
    fn equal_seed_cycle_facts(n: i64, seed_val: i64) -> BTreeMap<Rel, BTreeSet<Tuple>> {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "edge",
            (0..n)
                .map(|i| vec![v(i), v((i + 1) % n)])
                .map(Tuple::from_vec)
                .collect(),
        );
        facts.insert(
            "seed",
            (0..n)
                .map(|i| vec![v(i), v(seed_val)])
                .map(Tuple::from_vec)
                .collect(),
        );
        facts
    }

    /// The meet recursion plus a single-row `count` on top — the reviewer's
    /// exact shape (tiny post-stratum footprint). Target relation is `cnt`.
    fn meet_tightslack_model(n: i64) -> Program {
        let mut rules = meet_reach_rules("min");
        // cnt[count(X)] :- m[X, Y] — all-aggregated, one output row.
        rules.push(Rule::aggregated(
            "cnt",
            vec![x()],
            vec![named("count")],
            vec![lit("m", vec![x(), y()], false)],
        ));
        Program {
            rules,
            facts: equal_seed_cycle_facts(n, 7),
            ..Program::default()
        }
    }

    #[test]
    fn meet_rerederivation_does_not_perturb_completing_program() {
        const N: i64 = 500;
        let model = meet_tightslack_model(N);
        let cnt = |c: u64| {
            real_eval(
                &model,
                "cnt",
                1,
                &BTreeMap::new(),
                &generous_budget().with_derived_tuple_ceiling(c),
            )
        };
        // Reference: the unbudgeted answer (no ceiling armed at all).
        let reference = real_eval(&model, "cnt", 1, &BTreeMap::new(), &generous_budget())
            .expect("unbudgeted meet recursion completes");

        // True admitted spend = the minimal ceiling at which it completes
        // (binary search; monotone in the ceiling). This is the barrier's
        // honest cost, independent of the guard.
        let (mut lo, mut hi) = (1u64, 4_000u64);
        assert!(cnt(hi).is_ok(), "completes at a generous ceiling");
        while lo < hi {
            let mid = (lo + hi) / 2;
            if cnt(mid).is_ok() {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let true_spend = lo;
        // 500 seed groups admitted in epoch 0, plus the count stratum — the
        // reviewer's measured 502 for this exact shape.
        assert_eq!(true_spend, 502, "true admitted spend of the 500-cycle");

        // Just BELOW the true spend: the only refusal is the BARRIER
        // (DerivedTuples), and its `spent` is the true admitted spend — NOT
        // an in-flight overcount. (Pre-fix this was InFlightDerivations with
        // an inflated spend.)
        let err = cnt(true_spend - 1).expect_err("one under true spend must refuse");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(
            refusal.dimension,
            BudgetDimension::DerivedTuples,
            "below true spend the honest refusal is the barrier, not the mid-epoch guard"
        );
        assert_eq!(
            refusal.spent, true_spend,
            "refusal spend is true admitted spend, not in-flight volume"
        );

        // THE SWEEP: every ceiling from the true spend up through the whole
        // old divergence window (`baseline + N` and beyond) must COMPLETE and
        // return the byte-identical reference answer. The pre-fix guard
        // refused the entire `[502, ~1000]` band here.
        for c in true_spend..=(true_spend + N as u64 + 40) {
            let got = cnt(c).unwrap_or_else(|e| {
                panic!("ceiling {c} ≥ true spend {true_spend} must complete, refused: {e:?}")
            });
            assert_eq!(
                got, reference,
                "ceiling {c}: guarded answer must be byte-identical to the unbudgeted answer"
            );
        }
    }

    // ── mutation-hardening at the boundaries (kills the 3 survivors) ──────

    /// Emits `distinct` distinct rows, then `dups` copies of row 0. The plain
    /// out-store dedups, so `out.len()` plateaus at `distinct` while the
    /// ticker keeps firing on the duplicate tail — this puts a stride check
    /// squarely on `out.len() == distinct`, the exact-at-ceiling boundary.
    struct DistinctThenDup {
        distinct: i64,
        dups: i64,
        contained: BTreeMap<AtomOccurrence, MagicSymbol>,
    }
    impl DistinctThenDup {
        fn new(distinct: i64, dups: i64) -> Self {
            Self {
                distinct,
                dups,
                contained: BTreeMap::new(),
            }
        }
    }
    impl crate::exec::fixpoint::eval::seal::Sealed for DistinctThenDup {}

    impl RuleBody for DistinctThenDup {
        fn for_each_derivation(
            &self,
            _stores: &BTreeMap<MagicSymbol, EpochStore>,
            _delta_from: Option<AtomOccurrence>,
            _want_premises: bool,
            f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
        ) -> Result<()> {
            for i in 0..self.distinct {
                if f(Cow::Owned(vec![v(i), v(0)]), Premises::NotRequested)?.is_break() {
                    return Ok(());
                }
            }
            for _ in 0..self.dups {
                if f(Cow::Owned(vec![v(0), v(0)]), Premises::NotRequested)?.is_break() {
                    return Ok(());
                }
            }
            Ok(())
        }
        fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
            &self.contained
        }
    }

    fn single_stratum_program<B: RuleBody>(symb: MagicSymbol, body: B) -> EvalProgram<B, NoFixed> {
        let rule_set = EvalRuleSet::new(vec![HeadAggrSlot::Plain, HeadAggrSlot::Plain], vec![body]).unwrap();
        let mut stratum: EvalStratum<B, NoFixed> = EvalStratum::default();
        stratum.defs.insert(symb, EvalDefinition::Rules(rule_set));
        EvalProgram::from_execution_order(vec![stratum]).unwrap()
    }

    /// Kills M3 (`spent > ceiling` → `>=`, the off-by-one). A rule that
    /// admits EXACTLY `ceiling` distinct rows then re-derives dominates:
    /// `out.len()` plateaus at the ceiling and a stride check lands on
    /// `spent == ceiling`. Exact-at-ceiling must COMPLETE (`>`), never refuse
    /// (`>=`). The barrier admits exactly `ceiling ≤ ceiling`, so the answer
    /// is the full `ceiling` distinct rows.
    #[test]
    fn exact_at_ceiling_completes_not_refused() {
        const CEILING: u64 = 128; // a stride multiple, so a check lands on it
        let emitted_distinct = CEILING as i64;
        // dups long enough that a stride check fires while out.len() == CEILING.
        let program =
            single_stratum_program(entry_symbol(), DistinctThenDup::new(emitted_distinct, 128));
        let budget = generous_budget().with_derived_tuple_ceiling(CEILING);
        let outcome = stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            no_limit(),
            &budget,
            None,
        )
        .expect("exact-at-ceiling spend must COMPLETE, not refuse (kills `>=`)");
        let rows = outcome.store.all_iter()?.count();
        assert_eq!(rows, CEILING as usize, "all exactly-ceiling rows survive");
    }

    /// Kills M2a (INTERRUPT_STRIDE ×64 weakening). The boundedness law is
    /// stride-linear, so the stride is load-bearing and pinned by a LITERAL
    /// — not by the `INTERRUPT_STRIDE` symbol (a bound written in terms of
    /// the symbol moves with the mutant and cannot detect it). A hostile
    /// near-cross-product must refuse having materialized no more than
    /// `ceiling + 64` rows; a 64× wider stride would let it reach ~4096.
    #[test]
    fn stride_pinned_at_64_bounds_materialization() {
        assert_eq!(
            INTERRUPT_STRIDE, 64,
            "the boundedness bound is O(ceiling + STRIDE); changing STRIDE is a \
             data-safety change — re-derive the bound and this pin deliberately"
        );
        const CEILING: u64 = 100;
        let emitted = Arc::new(AtomicUsize::new(0));
        let program = cross_product_program(entry_symbol(), 400, 400, emitted.clone());
        let budget = generous_budget().with_derived_tuple_ceiling(CEILING);
        stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            no_limit(),
            &budget,
            None,
        )
        .expect_err("must refuse mid-epoch");
        let emitted = emitted.load(Ordering::Relaxed) as u64;
        // Literal bound, NOT `CEILING + INTERRUPT_STRIDE`: with stride 64 the
        // guard trips by ~164 materialized; with the mutant's 4096 it would
        // reach ~4096, blowing this hard ceiling.
        assert!(
            emitted <= 100 + 64 + 1,
            "materialization {emitted} must stay within one 64-stride of the ceiling"
        );
    }

    /// Kills M4 (`epoch_baseline` zeroed). A completing stratum admits a
    /// NONZERO baseline; a later stratum's flooder must count it. With the
    /// real baseline the refusal spend is `baseline + in_flight`; zero it and
    /// the reported spend (and the trip point) shift. Pin the exact spend so
    /// the baseline term is load-bearing.
    #[test]
    fn nonzero_baseline_mid_epoch_refusal_counts_baseline() {
        // Stratum 0 admits exactly 100 distinct rows and COMPLETES (100 ≤
        // ceiling 101); it never trips (its only stride check sees
        // out.len()=63 < 101). Baseline for stratum 1 is therefore 100.
        let mut s0: EvalStratum<CrossProduct, NoFixed> = EvalStratum::default();
        s0.defs.insert(
            muggle("s0"),
            EvalDefinition::Rules(
                EvalRuleSet::new(
                    vec![HeadAggrSlot::Plain, HeadAggrSlot::Plain],
                    vec![CrossProduct::new(100, 1, Arc::new(AtomicUsize::new(0)))],
                )
                .unwrap(),
            ),
        );
        // Stratum 1 (the entry) floods; its FIRST stride check sees
        // out.len()=63, so spent = baseline(100) + 63 = 163 > ceiling 101.
        let mut s1: EvalStratum<CrossProduct, NoFixed> = EvalStratum::default();
        s1.defs.insert(
            entry_symbol(),
            EvalDefinition::Rules(
                EvalRuleSet::new(
                    vec![HeadAggrSlot::Plain, HeadAggrSlot::Plain],
                    vec![CrossProduct::new(400, 400, Arc::new(AtomicUsize::new(0)))],
                )
                .unwrap(),
            ),
        );
        let program = EvalProgram::from_execution_order(vec![s0, s1]).unwrap();
        let budget = generous_budget().with_derived_tuple_ceiling(101);
        let err = stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            no_limit(),
            &budget,
            None,
        )
        .expect_err("stratum 1 floods past baseline+ceiling");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(refusal.dimension, BudgetDimension::InFlightDerivations);
        assert_eq!(refusal.ceiling, 101);
        assert_eq!(
            refusal.spent, 163,
            "spend must be baseline(100) + in_flight(63); zeroing the baseline changes it"
        );
    }

    /// A fixed rule that `put`s `rows` distinct tuples, ticking the ordinary
    /// per-rule mid-run guard ([`Budget::ticker`]) as it goes — exercising
    /// the exact `baseline` [`FixedRuleEval::run`] receives, the same way
    /// [`crate::rules::contract::FixedRuleOutput`]'s own guard does in
    /// production.
    struct BaselineCheckingFixed {
        rows: i64,
        symb: MagicSymbol,
    }
    impl FixedRuleEval for BaselineCheckingFixed {
        fn run(
            &self,
            _stores: &BTreeMap<MagicSymbol, EpochStore>,
            out: &mut RegularTempStore,
            budget: &Budget,
            baseline: u64,
        ) -> Result<()> {
            let mut ticker = budget.ticker(baseline, &self.symb);
            for i in 0..self.rows {
                ticker.tick(out.len())?;
                out.put(Tuple::from_vec(vec![v(i)]));
            }
            Ok(())
        }
    }

    /// Fixed-rule twin of [`nonzero_baseline_mid_epoch_refusal_counts_baseline`]:
    /// proves the baseline `FixedRuleEval::run` now receives is the true
    /// global admitted spend, not the fixed baseline-0 compromise. Stratum 0
    /// admits exactly 100 rows and completes; stratum 1 (the entry) is a
    /// FIXED rule that puts up to 400 rows, ticking the same mid-run guard
    /// ordinary rules use.
    ///
    /// With ceiling 101: the fixed rule's first stride check lands at
    /// `out.len() == 63`, so `spent` must be `baseline(100) + 63 == 163 >
    /// 101` — refusing. Sabotage check: if the baseline were zeroed (the old
    /// compromise), `0 + 63 == 63 ≤ 101` would NOT trip at that check; the
    /// rule would keep materializing and only refuse later, at a different
    /// (lower) `spent` value — so pinning `spent == 163` exactly fails under
    /// that reversion.
    ///
    /// With ceiling 1000 (accommodates the true total of 100 + 400 = 500):
    /// the same program must COMPLETE, proving the plumbing doesn't
    /// over-refuse when the global total fits.
    #[test]
    fn fixed_rule_budget_counts_global_baseline() {
        fn program(ceiling: u64) -> (EvalProgram<CrossProduct, BaselineCheckingFixed>, Budget) {
            let mut s0: EvalStratum<CrossProduct, BaselineCheckingFixed> = EvalStratum::default();
            s0.defs.insert(
                muggle("s0"),
                EvalDefinition::Rules(
                    EvalRuleSet::new(
                        vec![HeadAggrSlot::Plain, HeadAggrSlot::Plain],
                        vec![CrossProduct::new(100, 1, Arc::new(AtomicUsize::new(0)))],
                    )
                    .unwrap(),
                ),
            );
            let mut s1: EvalStratum<CrossProduct, BaselineCheckingFixed> = EvalStratum::default();
            s1.defs.insert(
                entry_symbol(),
                EvalDefinition::Fixed {
                    arity: 1,
                    rule: BaselineCheckingFixed {
                        rows: 400,
                        symb: entry_symbol(),
                    },
                },
            );
            let program = EvalProgram::from_execution_order(vec![s0, s1]).unwrap();
            let budget = generous_budget().with_derived_tuple_ceiling(ceiling);
            (program, budget)
        }

        // Refuses: the fixed rule's own spend, uncounted, would never cross
        // 101; only the true global baseline (100 from stratum 0) does.
        let (prog, budget) = program(101);
        let err = stratified_evaluate(&prog, &StoreLifetimes::default(), no_limit(), &budget, None)
            .expect_err("the fixed rule must refuse because the global baseline is counted");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(refusal.dimension, BudgetDimension::InFlightDerivations);
        assert_eq!(refusal.ceiling, 101);
        assert_eq!(
            refusal.spent, 163,
            "spend must be baseline(100) + in_flight(63); a zeroed baseline changes both \
             the trip point and this value"
        );

        // Completes: a ceiling that accommodates the true total (100 + 400)
        // must not refuse.
        let (prog, budget) = program(1000);
        let outcome =
            stratified_evaluate(&prog, &StoreLifetimes::default(), no_limit(), &budget, None)
                .expect("a ceiling covering the true total must not refuse");
        assert_eq!(outcome.store.all_iter().unwrap().count(), 400);
    }

    /// F3 pin: the STREAMING harness bounds the killer shape. A 10_000×10_000
    /// cross product (100M candidate rows) through the `ModelBody` oracle
    /// harness with a small ceiling must refuse — typed, fast, bounded — NOT
    /// OOM below the tick seam as the pre-fix frontier-materializing harness
    /// did (reviewer finding F3). This is the harness twin of the
    /// compiled-path killer pin (`compile.rs` cross-product test). Its mere
    /// completion under the 12G cap is the boundedness proof; the assertions
    /// pin the typed, stride-bounded refusal.
    #[test]
    fn harness_killer_cross_product_streams_through_the_guard() {
        let n = 10_000i64;
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "a",
            (0..n).map(|i| vec![v(i)]).map(Tuple::from_vec).collect(),
        );
        facts.insert(
            "b",
            (0..n).map(|i| vec![v(i)]).map(Tuple::from_vec).collect(),
        );
        let model = Program {
            rules: vec![Rule::plain(
                "out",
                vec![x(), y()],
                vec![lit("a", vec![x()], false), lit("b", vec![y()], false)],
            )],
            facts,
            ..Program::default()
        };
        let budget = generous_budget().with_derived_tuple_ceiling(1_000);
        let err = real_eval(&model, "out", 2, &BTreeMap::new(), &budget)
            .expect_err("a 100M-row cross product must refuse, not OOM");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal, not an abort");
        assert_eq!(refusal.dimension, BudgetDimension::InFlightDerivations);
        assert_eq!(refusal.ceiling, 1_000);
        assert!(refusal.spent > 1_000);
        assert!(
            refusal.spent <= 1_000 + INTERRUPT_STRIDE as u64,
            "materialization bounded within a stride of the ceiling: {}",
            refusal.spent
        );
    }

    #[test]
    fn deadline_zero_refuses() {
        let model = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3)]),
            ..Program::default()
        };
        let budget = generous_budget().with_timeout(Duration::ZERO);
        let err = real_eval(&model, "path", 2, &BTreeMap::new(), &budget).expect_err("refuses");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(refusal.dimension, BudgetDimension::Deadline);
    }

    /// The unkillable-scan gap is closed: a rule mid-iteration observes
    /// a spent [`CancelAuthority`] and stops, long before its scan would
    /// finish. The original checked poison once per rule, *after* the full
    /// scan.
    #[test]
    fn kill_flag_interrupts_inside_rule_iteration() {
        use crate::rules::contract::{CancelAuthority, Cancelled};
        use std::sync::Mutex;

        struct FloodBody {
            contained: BTreeMap<AtomOccurrence, MagicSymbol>,
            auth: Mutex<Option<CancelAuthority>>,
            emitted: Arc<AtomicUsize>,
        }
        impl crate::exec::fixpoint::eval::seal::Sealed for FloodBody {}

        impl RuleBody for FloodBody {
            fn for_each_derivation(
                &self,
                _stores: &BTreeMap<MagicSymbol, EpochStore>,
                _delta_from: Option<AtomOccurrence>,
                _want_premises: bool,
                f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
            ) -> Result<()> {
                for i in 0..1_000_000i64 {
                    if i == 10
                        && let Ok(mut slot) = self.auth.lock()
                        && let Some(auth) = slot.take()
                    {
                        let _ = auth.cancel();
                    }
                    self.emitted.fetch_add(1, Ordering::Relaxed);
                    if f(Cow::Owned(vec![v(i)]), Premises::NotRequested)?.is_break() {
                        return Ok(());
                    }
                }
                Ok(())
            }
            fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
                &self.contained
            }
        }
        let (auth, cancel) = CancelAuthority::arm();
        let emitted = Arc::new(AtomicUsize::new(0));
        let body = FloodBody {
            contained: BTreeMap::new(),
            auth: Mutex::new(Some(auth)),
            emitted: emitted.clone(),
        };
        let rule_set = EvalRuleSet::new(vec![HeadAggrSlot::Plain], vec![body]).unwrap();
        let mut stratum: EvalStratum<FloodBody, NoFixed> = EvalStratum::default();
        stratum
            .defs
            .insert(entry_symbol(), EvalDefinition::Rules(rule_set));
        let program = EvalProgram::from_execution_order(vec![stratum]).unwrap();
        let budget = generous_budget().with_cancel(cancel);
        let err = stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            no_limit(),
            &budget,
            None,
        )
        .expect_err("killed");
        assert!(
            err.downcast_ref::<Cancelled>().is_some(),
            "typed Cancelled refusal"
        );
        let count = emitted.load(Ordering::Relaxed);
        assert!(
            count < 10_000,
            "the scan must stop promptly after the kill (emitted {count})"
        );
    }

    /// A fixed-rule stand-in for programs that have none.
    #[derive(Debug)]
    struct NoFixed;
    impl FixedRuleEval for NoFixed {
        fn run(
            &self,
            _stores: &BTreeMap<MagicSymbol, EpochStore>,
            _out: &mut RegularTempStore,
            _budget: &Budget,
            _baseline: u64,
        ) -> Result<()> {
            Ok(())
        }
    }

    // ── limiter (early return) ───────────────────────────────────────────

    #[test]
    fn limiter_early_returns_take_minus_skip_rows() {
        let edges: Vec<(i64, i64)> = (0..10).map(|i| (i, i + 1)).collect();
        let model = Program {
            rules: transitive_closure(),
            facts: edge_facts(&edges),
            ..Program::default()
        };
        let oracle_db = naive_eval(&model).unwrap();
        let compiled = compile_for(&model, "path", 2, &BTreeMap::new());
        // :limit 2 :offset 1 → take 3, skip 1.
        let limit = RowLimit {
            num_to_take: Some(3),
            num_to_skip: Some(1),
        };
        let outcome = stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            limit,
            &generous_budget(),
            None,
        )
        .expect("evaluates");
        assert!(outcome.limited, "the limiter engaged");
        let returned: Vec<Tuple> = collect_materialized(outcome.store.early_returned_iter()?)?;
        assert_eq!(returned.len(), 2, "limit rows, offset excluded");
        let taken: Vec<Tuple> = collect_materialized(outcome.store.all_iter()?)?;
        assert_eq!(taken.len(), 3, "take = limit + offset rows produced");
        for row in taken {
            assert!(
                oracle_db["path"].contains(&row),
                "every row is a real answer"
            );
        }
    }

    /// The incremental limiter path (D1/D2 and the N2 overshoot), executed:
    /// the ENTRY rule itself is recursive (TC computed in the entry store),
    /// so `incremental_plain_eval` runs with the limiter engaged — dead
    /// code under the previous suite (the review's surviving mutant M5).
    ///
    /// Diamond + tail: edge (0,1),(0,2),(1,3),(2,3),(3,4); take = 7.
    /// Traced epochs (ModelBody iterates stores/facts in sorted order):
    ///   epoch 0: base rows (0,1),(0,2),(1,3),(2,3),(3,4)   — counter 5
    ///   epoch 1: (0,3) [count 6], (0,3) again — the D2 dedup point: the
    ///            re-derivation within the epoch must NOT count (upstream
    ///            double-counted here and stopped one row short), then
    ///            (1,4) [count 7 → stop; (2,4) never derived]
    ///   epoch 2: (0,4) put-then-counted [count 8] — the N2 overshoot row
    ///   epoch 3: nothing new → fixpoint
    /// Final store: exactly take + 1 rows, every one a real answer.
    #[test]
    fn limiter_incremental_entry_recursion_dedups_and_overshoots() {
        let edges = [(0, 1), (0, 2), (1, 3), (2, 3), (3, 4)];
        let rules = vec![
            Rule::plain(
                "?",
                vec![x(), y()],
                vec![lit("edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "?",
                vec![x(), z()],
                vec![
                    lit("?", vec![x(), y()], false),
                    lit("edge", vec![y(), z()], false),
                ],
            ),
        ];
        let oracle_model = Program {
            rules: rules.clone(),
            facts: edge_facts(&edges),
            ..Program::default()
        };
        let oracle_closure = naive_eval(&oracle_model).unwrap().remove("?").unwrap();

        let facts = Arc::new(edge_facts(&edges));
        let idb: Arc<BTreeSet<Rel>> = Arc::new(["?"].into_iter().collect());
        let bodies: Vec<ModelBody> = rules
            .iter()
            .map(|r| {
                ModelBody::new(
                    r.head_args.clone(),
                    r.body.clone(),
                    facts.clone(),
                    idb.clone(),
                )
            })
            .collect();
        let rule_set = EvalRuleSet::new(vec![HeadAggrSlot::Plain, HeadAggrSlot::Plain], bodies).unwrap();
        let mut stratum: EvalStratum<ModelBody, NoFixed> = EvalStratum::default();
        stratum
            .defs
            .insert(entry_symbol(), EvalDefinition::Rules(rule_set));
        let program = EvalProgram::from_execution_order(vec![stratum]).unwrap();

        let limit = RowLimit {
            num_to_take: Some(7),
            num_to_skip: None,
        };
        let outcome = stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            limit,
            &generous_budget(),
            None,
        )
        .expect("evaluates");
        assert!(outcome.limited, "the limiter engaged");
        let rows: BTreeSet<Tuple> = collect_materialized(outcome.store.all_iter()?)?
            .into_iter()
            .collect();
        for row in &rows {
            assert!(oracle_closure.contains(row), "every row is a real answer");
        }
        let expected: BTreeSet<Tuple> = [
            (0, 1),
            (0, 2),
            (1, 3),
            (2, 3),
            (3, 4),
            (0, 3),
            (1, 4),
            (0, 4),
        ]
        .iter()
        .map(|(a, b)| vec![v(*a), v(*b)])
        .map(Tuple::from_vec)
        .collect();
        assert_eq!(
            rows, expected,
            "the traced limited set: D2 dedup keeps (1,4); N2 overshoot admits (0,4)"
        );
        assert_eq!(rows.len(), 8, "take + 1 rows: the documented N2 overshoot");
    }

    #[test]
    fn without_limit_the_outcome_is_not_limited() {
        let model = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2)]),
            ..Program::default()
        };
        let compiled = compile_for(&model, "path", 2, &BTreeMap::new());
        let outcome = stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            no_limit(),
            &generous_budget(),
            None,
        )
        .expect("evaluates");
        assert!(!outcome.limited);
    }

    // ── provenance hooks ─────────────────────────────────────────────────

    #[test]
    fn witnesses_record_first_derivations_in_canonical_order() {
        let model = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3)]),
            ..Program::default()
        };
        let compiled = compile_for(&model, "path", 2, &BTreeMap::new());
        let mut table = WitnessTable::default();
        stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            no_limit(),
            &generous_budget(),
            Some(&mut table),
        )
        .expect("evaluates");

        let path_store = muggle("path");
        let path_witnesses: Vec<&Witness> = table
            .entries()
            .iter()
            .filter(|w| w.store == path_store)
            .collect();
        // The closure of 1→2→3 is {(1,2),(2,3),(1,3)}: each admitted once.
        assert_eq!(path_witnesses.len(), 3);
        // Epoch 0 admits the base tuples in canonical order.
        assert_eq!(path_witnesses[0].tuple, Tuple::from_vec(vec![v(1), v(2)]));
        assert_eq!(path_witnesses[1].tuple, Tuple::from_vec(vec![v(2), v(3)]));
        // Base tuples: rule 0, premise = the edge row.
        assert_eq!(
            path_witnesses[0].derivation,
            Some((0, vec![Tuple::from_vec(vec![v(1), v(2)])]))
        );
        // The recursive tuple: rule 1, premises = edge(1,2) then path(2,3).
        assert_eq!(path_witnesses[2].tuple, Tuple::from_vec(vec![v(1), v(3)]));
        assert_eq!(
            path_witnesses[2].derivation,
            Some((
                1,
                vec![
                    Tuple::from_vec(vec![v(1), v(2)]),
                    Tuple::from_vec(vec![v(2), v(3)])
                ]
            ))
        );
    }

    #[test]
    fn meet_identity_row_witness_has_no_derivation() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert("nothing", BTreeSet::new());
        let model = Program {
            rules: vec![Rule::aggregated(
                "g",
                vec![x(), y()],
                vec![named("min"), named("or")],
                vec![lit("nothing", vec![x(), y()], false)],
            )],
            facts,
            ..Program::default()
        };
        let compiled = compile_for(&model, "g", 2, &BTreeMap::new());
        let mut table = WitnessTable::default();
        stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            no_limit(),
            &generous_budget(),
            Some(&mut table),
        )
        .expect("evaluates");
        let g_store = muggle("g");
        let identity: Vec<&Witness> = table
            .entries()
            .iter()
            .filter(|w| w.store == g_store)
            .collect();
        assert_eq!(identity.len(), 1);
        assert_eq!(
            identity[0].tuple,
            Tuple::from_vec(vec![DataValue::Null, DataValue::from(false)])
        );
        assert_eq!(
            identity[0].derivation, None,
            "identity row has no derivation"
        );
    }

    // ── constructor refusals and typed invariants ────────────────────────

    #[test]
    fn empty_rule_set_is_refused_at_construction() {
        let refused = EvalRuleSet::<ModelBody>::new(vec![HeadAggrSlot::Plain], vec![]);
        assert!(matches!(refused, Err(RuleSetShapeError::Empty)));
    }

    /// The retired deviation D3: a non-suffix all-meet head (here the meet
    /// column sits at position 0, ahead of its grouping position) is no
    /// longer a constructor refusal. The landed [`MeetAggrStore`] groups by
    /// position, so the shape the original silently demoted to a frozen
    /// normal aggregation (wrong answers) now constructs cleanly and its
    /// grouping positions are recorded exactly where they sit.
    #[test]
    fn non_suffix_meet_head_constructs_with_positional_grouping() {
        let facts = Arc::new(BTreeMap::new());
        let idb = Arc::new(BTreeSet::new());
        let body = ModelBody::new(
            vec![y(), x()],
            vec![lit("d", vec![x(), y()], false)],
            facts,
            idb,
        );
        // Meet at position 0, grouping at position 1 — the exact shape D3
        // used to reject.
        let rule_set =
            EvalRuleSet::new(vec![named("min"), HeadAggrSlot::Plain], vec![body]).expect("no longer refused");
        assert_eq!(
            rule_set.kind,
            HeadAggrKind::Meet {
                key_positions: vec![HeadPos::from_index(1)]
            },
            "the grouping position is position 1, wherever the meet column sits"
        );
    }

    /// The end-to-end companion to the retired refusal: the same non-suffix
    /// shape (meet at position 0) does not merely construct — it *answers*,
    /// folding each group's meet exactly as the sealed positional oracle
    /// does, instead of the original's frozen demotion.
    #[test]
    fn non_suffix_meet_head_answers_matching_oracle() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "obs",
            [(1, 5), (1, 3), (2, 9)]
                .iter()
                .map(|(k, val)| vec![v(*k), v(*val)])
                .map(Tuple::from_vec)
                .collect(),
        );
        // m[min(V), K] :- obs[K, V].  Grouping position is 1 (K), the meet
        // column is position 0 — a non-suffix layout the old store refused.
        let rules = vec![Rule::aggregated(
            "m",
            vec![y(), x()],
            vec![named("min"), HeadAggrSlot::Plain],
            vec![lit("obs", vec![x(), y()], false)],
        )];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    // ── adversarial reviewer attacks (adopted from the hostile pass) ──────
    // Adopted verbatim from the reviewer's deliverables; only imports/naming
    // match house style. These pin the witness-by-grouping-projection
    // correctness the frozen diff left unpinned (the surviving M6 mutant),
    // plus Null / shared-var / all-aggregated / negation-below coverage.

    /// ATTACK 1a: Null values in the grouping column AND in the meet column
    /// at a non-suffix layout. Null's position in DataValue's total order is
    /// load-bearing for both by_group and by_row ordering.
    #[test]
    fn rev_differential_meet_pos0_nulls_in_group_and_value() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "obs",
            vec![
                vec![DataValue::Null, v(5)],
                vec![DataValue::Null, v(2)],
                vec![v(1), DataValue::Null],
                vec![v(1), v(7)],
                vec![v(2), v(3)],
            ]
            .into_iter()
            .map(Tuple::from_vec)
            .collect(),
        );
        // m[min(V), K] :- obs[K, V]  — Null group key, Null meet value.
        let rules = vec![Rule::aggregated(
            "m",
            vec![y(), x()],
            vec![named("min"), HeadAggrSlot::Plain],
            vec![lit("obs", vec![x(), y()], false)],
        )];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    /// ATTACK 1b: the same variable at a grouping position AND a meet
    /// position (m[min(V), V]): every group folds itself.
    #[test]
    fn rev_differential_meet_var_shared_by_key_and_val() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "obs",
            [(1, 5), (1, 3), (2, 3), (2, 9)]
                .iter()
                .map(|(k, val)| vec![v(*k), v(*val)])
                .map(Tuple::from_vec)
                .collect(),
        );
        let rules = vec![Rule::aggregated(
            "m",
            vec![y(), y()],
            vec![named("min"), HeadAggrSlot::Plain],
            vec![lit("obs", vec![x(), y()], false)],
        )];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    /// ATTACK 1c: all-aggregated multi-column meet head (empty group key —
    /// one group, keyed by the empty tuple) inside recursion, WITH real
    /// derivations so the identity row must never appear.
    #[test]
    fn rev_differential_meet_all_aggregated_recursive() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 5), (5, 1)]);
        facts.insert(
            "start",
            [vec![v(3), v(3)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect(),
        );
        // m[min(A), max(B)] :- start[A, B]
        // m[min(Y), max(Y)] :- m[X, _ignored], edge[X, Y]
        let rules = vec![
            Rule::aggregated(
                "m",
                vec![x(), y()],
                vec![named("min"), named("max")],
                vec![lit("start", vec![x(), y()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y(), y()],
                vec![named("min"), named("max")],
                vec![
                    lit("m", vec![x(), z()], false),
                    lit("edge", vec![x(), y()], false),
                ],
            ),
        ];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    /// ATTACK 3/6: a nastier determinism program — a non-suffix meet
    /// recursion whose seed relation is derived THROUGH NEGATION in a lower
    /// stratum, on a bigger denser graph, plus an interleaved 3-column meet
    /// head in the same program. Results and witness tables must be
    /// byte-identical at 1/2/4/8 threads.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn rev_determinism_nonsuffix_meet_negation_below() {
        let edges: Vec<(i64, i64)> = (0..24)
            .flat_map(|i| vec![(i, (i * 5 + 7) % 24), (i, (i * 11 + 3) % 24)])
            .collect();
        let mut facts = edge_facts(&edges);
        facts.insert(
            "node",
            (0..24).map(|i| vec![v(i)]).map(Tuple::from_vec).collect(),
        );
        facts.insert(
            "special",
            [0i64, 7, 13, 21]
                .iter()
                .map(|i| vec![v(*i)])
                .map(Tuple::from_vec)
                .collect(),
        );
        let mut rules = vec![
            // Stratum below: nonspecial via negation.
            Rule::plain(
                "nonspecial",
                vec![x()],
                vec![
                    lit("node", vec![x()], false),
                    lit("special", vec![x()], true),
                ],
            ),
            // Seed: every nonspecial node seeds with its own id.
            Rule::plain(
                "seed",
                vec![x(), x()],
                vec![lit("nonspecial", vec![x()], false)],
            ),
        ];
        rules.extend(meet_reach_rules_pos0("min"));
        // A second, interleaved meet head in the same program:
        // w[min(V), K, max(V)] :- m[V, K].
        rules.push(Rule::aggregated(
            "w",
            vec![y(), x(), y()],
            vec![named("min"), HeadAggrSlot::Plain, named("max")],
            vec![lit("m", vec![y(), x()], false)],
        ));
        rules.push(Rule::plain(
            "out",
            vec![x(), y(), z()],
            vec![lit("w", vec![x(), y(), z()], false)],
        ));
        let model = Program {
            rules,
            facts,
            ..Program::default()
        };
        assert_matches_oracle(&model);
        let run = |threads: usize| -> (BTreeSet<Tuple>, Vec<String>) {
            at_thread_count(threads, || {
                let compiled = compile_for(&model, "out", 3, &BTreeMap::new());
                let mut table = WitnessTable::default();
                let outcome = stratified_evaluate(
                    &compiled.program,
                    &compiled.lifetimes,
                    no_limit(),
                    &generous_budget(),
                    Some(&mut table),
                )
                .expect("evaluates");
                let rows = collect_materialized(outcome.store.all_iter()?)?;
                let witnesses = table
                    .entries()
                    .iter()
                    .map(|w| format!("{w:?}"))
                    .collect_vec();
                (rows, witnesses)
            })
        };
        let baseline = run(1);
        for threads in [2, 4, 8] {
            let got = run(threads);
            assert_eq!(got.0, baseline.0, "results differ at {threads} threads");
            assert_eq!(got.1, baseline.1, "witnesses differ at {threads} threads");
        }
    }

    /// ATTACK: positive witness binding for a NON-SUFFIX meet head — the
    /// admitted group's witness must carry Some(derivation) recovered
    /// through the grouping projection (this is the assertion the frozen
    /// diff's own tests never make; a consistently mis-keyed projection
    /// passes every thread-count comparison).
    #[test]
    fn rev_nonsuffix_meet_witness_binds_derivation() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "obs",
            [(1, 5), (1, 3), (2, 9)]
                .iter()
                .map(|(k, val)| vec![v(*k), v(*val)])
                .map(Tuple::from_vec)
                .collect(),
        );
        // m[min(V), K] :- obs[K, V]
        let model = Program {
            rules: vec![Rule::aggregated(
                "m",
                vec![y(), x()],
                vec![named("min"), HeadAggrSlot::Plain],
                vec![lit("obs", vec![x(), y()], false)],
            )],
            facts,
            ..Program::default()
        };
        let compiled = compile_for(&model, "m", 2, &BTreeMap::new());
        let mut table = WitnessTable::default();
        stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            no_limit(),
            &generous_budget(),
            Some(&mut table),
        )
        .expect("evaluates");
        let m_store = muggle("m");
        let ws: Vec<&Witness> = table
            .entries()
            .iter()
            .filter(|w| w.store == m_store)
            .collect();
        assert_eq!(ws.len(), 2, "one witness per admitted group");
        for w in &ws {
            assert!(
                w.derivation.is_some(),
                "a non-suffix meet admission must bind its pending derivation: {w:?}"
            );
        }
        // Group 1 folded to min 3; its witness is the FIRST derivation seen
        // for the group, whose premise row comes from obs.
        assert_eq!(ws[0].tuple, Tuple::from_vec(vec![v(3), v(1)]));
        let (_, premises) = ws[0].derivation.as_ref().unwrap();
        assert_eq!(premises.len(), 1);
        assert!(
            premises[0] == Tuple::from_vec(vec![v(1), v(3)])
                || premises[0] == Tuple::from_vec(vec![v(1), v(5)]),
            "premise must be a real obs row for group 1: {:?}",
            premises[0]
        );
    }

    /// ATTACK (killer for prefix-keyed witness regressions): two groups
    /// fold to the SAME meet value at a non-suffix layout. Witness keying
    /// that collapses to the value prefix cannot tell the groups apart and
    /// binds group 2's witness to group 1's derivation. Each group's
    /// premises must come from its OWN obs rows.
    #[test]
    fn rev_nonsuffix_meet_witness_premises_are_per_group() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "obs",
            [(1, 3), (1, 5), (2, 3)]
                .iter()
                .map(|(k, val)| vec![v(*k), v(*val)])
                .map(Tuple::from_vec)
                .collect(),
        );
        // m[min(V), K] :- obs[K, V]; groups 1 and 2 both fold to min 3.
        let model = Program {
            rules: vec![Rule::aggregated(
                "m",
                vec![y(), x()],
                vec![named("min"), HeadAggrSlot::Plain],
                vec![lit("obs", vec![x(), y()], false)],
            )],
            facts,
            ..Program::default()
        };
        let compiled = compile_for(&model, "m", 2, &BTreeMap::new());
        let mut table = WitnessTable::default();
        stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            no_limit(),
            &generous_budget(),
            Some(&mut table),
        )
        .expect("evaluates");
        let m_store = muggle("m");
        let ws: Vec<&Witness> = table
            .entries()
            .iter()
            .filter(|w| w.store == m_store)
            .collect();
        assert_eq!(ws.len(), 2);
        for w in &ws {
            let group = w.tuple[1].clone();
            let (_, premises) = w
                .derivation
                .as_ref()
                .unwrap_or_else(|| panic!("unbound witness for group {group:?}"));
            assert_eq!(
                premises[0][0], group,
                "witness for group {group:?} bound a premise from another group: {premises:?}"
            );
        }
    }

    // ATTACK 1d (randomized): the full randomized stratified differential,
    // but with the meet column at position 0 — the frozen diff's proptest
    // only ever generates suffix layouts.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn rev_differential_randomized_nonsuffix_meet(case in arb_case()) {
            let mut model = build_case(&case);
            // Swap the suffix meet rules for the pos0 form and re-point the
            // reader at the swapped columns.
            model.rules.retain(|r| r.head_rel != "m" && r.head_rel != "out");
            model.rules.extend(meet_reach_rules_pos0(case.aggr_name));
            model.rules.push(Rule::plain(
                "out",
                vec![x(), y()],
                vec![lit("m", vec![y(), x()], false)],
            ));
            assert_matches_oracle(&model);
        }
    }

    #[test]
    fn missing_store_is_a_typed_error_not_a_panic() {
        // A rule whose contained-rules map names a store no stratum
        // defines: epoch 1's delta discipline must surface the invariant
        // as an error.
        struct GhostBody {
            contained: BTreeMap<AtomOccurrence, MagicSymbol>,
        }
        impl crate::exec::fixpoint::eval::seal::Sealed for GhostBody {}

        impl RuleBody for GhostBody {
            fn for_each_derivation(
                &self,
                _stores: &BTreeMap<MagicSymbol, EpochStore>,
                delta_from: Option<AtomOccurrence>,
                _want_premises: bool,
                f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
            ) -> Result<()> {
                if delta_from.is_none() {
                    let _ = f(Cow::Owned(vec![v(1)]), Premises::NotRequested)?;
                }
                Ok(())
            }
            fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
                &self.contained
            }
        }
        let mut contained = BTreeMap::new();
        contained.insert(AtomOccurrence(0), muggle("ghost"));
        let rule_set = EvalRuleSet::new(vec![HeadAggrSlot::Plain], vec![GhostBody { contained }]).unwrap();
        let mut stratum: EvalStratum<GhostBody, NoFixed> = EvalStratum::default();
        stratum
            .defs
            .insert(entry_symbol(), EvalDefinition::Rules(rule_set));
        let program = EvalProgram::from_execution_order(vec![stratum]).unwrap();
        let err = stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            no_limit(),
            &generous_budget(),
            None,
        )
        .expect_err("typed invariant error");
        assert!(err.to_string().contains("invariant"), "got: {err}");
    }

    #[test]
    fn entry_less_program_is_refused_at_construction() {
        let mut stratum: EvalStratum<ModelBody, NoFixed> = EvalStratum::default();
        let facts = Arc::new(BTreeMap::new());
        let idb = Arc::new(BTreeSet::new());
        let body = ModelBody::new(vec![x()], vec![lit("d", vec![x()], false)], facts, idb);
        stratum.defs.insert(
            muggle("r"),
            EvalDefinition::Rules(EvalRuleSet::new(vec![HeadAggrSlot::Plain], vec![body]).unwrap()),
        );
        let err = EvalProgram::from_execution_order(vec![stratum]).expect_err("no entry");
        assert!(err.to_string().contains("no entry"), "got: {err}");
    }

    #[test]
    fn epoch_ceiling_of_one_refuses_any_deriving_program() {
        // Even a settled derivation needs a second epoch to certify the
        // fixpoint: the minimum viable ceiling is 2, deterministically.
        let model = Program {
            rules: vec![Rule::plain(
                "p",
                vec![x()],
                vec![lit("d", vec![x()], false)],
            )],
            facts: {
                let mut f: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
                f.insert("d", [vec![v(1)]].into_iter().map(Tuple::from_vec).collect());
                f
            },
            ..Program::default()
        };
        let budget = Budget::new(NonZeroU32::new(1).unwrap());
        let err = real_eval(&model, "p", 1, &BTreeMap::new(), &budget).expect_err("refuses");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(refusal.dimension, BudgetDimension::Epochs);
        let ok = real_eval(
            &model,
            "p",
            1,
            &BTreeMap::new(),
            &Budget::new(NonZeroU32::new(2).unwrap()),
        );
        assert!(ok.is_ok(), "two epochs suffice for a settled derivation");
    }
}
