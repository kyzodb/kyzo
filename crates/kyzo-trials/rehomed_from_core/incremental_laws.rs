    // ── The production-vs-oracle differential (issue #61's non-
    // negotiable gate): every case laws.rs's own generative campaign
    // proves against full recompute, converted into this module's real
    // types and run through THIS module's algorithm, must agree with
    // the oracle's `incremental_eval` byte-for-byte. ───────────────────

    fn conv_term(t: &laws::Term) -> Term {
        match t {
            laws::Term::Const(c) => Term::Const(c.clone()),
            laws::Term::Var(name) => Term::Var(sym(name)),
        }
    }
    fn conv_literal(l: &laws::Literal) -> Literal {
        Literal {
            rel: sym(l.rel),
            args: l.args.iter().map(conv_term).collect(),
            polarity: l.polarity,
        }
    }
    fn conv_rule(r: &laws::Rule) -> Rule {
        Rule {
            head_rel: sym(r.head_rel),
            head_args: r.head_args.iter().map(conv_term).collect(),
            body: r.body.iter().map(conv_literal).collect(),
            aggr: r.aggr.clone(),
        }
    }
    fn conv_program(p: &laws::Program) -> IncrementalProgram {
        IncrementalProgram {
            rules: p.rules.iter().map(conv_rule).collect(),
        }
    }
    fn conv_facts(facts: &BTreeMap<laws::Rel, BTreeSet<Tuple>>) -> MaintainedState {
        facts.iter().map(|(k, v)| (sym(k), v.clone())).collect()
    }
    fn conv_signed(fact: &laws::SignedFact) -> SignedFact {
        match fact {
            laws::SignedFact::Plus(t) => SignedFact::Plus(t.clone()),
            laws::SignedFact::Minus(t) => SignedFact::Minus(t.clone()),
        }
    }
    fn conv_patch(
        patch: &BTreeMap<laws::Rel, BTreeSet<laws::SignedFact>>,
    ) -> BTreeMap<Symbol, BTreeSet<SignedFact>> {
        patch
            .iter()
            .map(|(k, facts)| (sym(k), facts.iter().map(conv_signed).collect()))
            .collect()
    }

    /// One case: build the oracle `Program`/EDB/patch, run
    /// `laws::incremental_eval`, convert everything to this module's
    /// types, run THIS module's `incremental_eval`, and assert the two
    /// deltas agree relation-by-relation (a relation absent from one
    /// side means the same as an empty delta on the other).
    fn assert_matches_oracle(
        oracle_program: &laws::Program,
        oracle_facts: &BTreeMap<laws::Rel, BTreeSet<Tuple>>,
        oracle_patch: &BTreeMap<laws::Rel, BTreeSet<laws::SignedFact>>,
        ctx: &str,
    ) {
        let full_oracle_program = laws::Program::untimed(
            oracle_program.rules.clone(),
            oracle_program.fixed.clone(),
            oracle_facts.clone(),
        );
        let oracle_out = laws::incremental_eval(&full_oracle_program, oracle_patch)
            .expect("oracle incremental_eval succeeds");
        // `MaintainedState` must start as the FULL old total (every IDB
        // relation's own prior derivation, not just the raw EDB facts) —
        // a standing query maintains that state itself; it has no way to
        // re-derive it from scratch each round. `naive_eval` on the
        // OLD (pre-patch) program is exactly that full old total.
        let old_total = laws::naive_eval(&full_oracle_program).expect("old program evaluates");

        let production_program = conv_program(oracle_program);
        let production_state = conv_facts(&old_total);
        let production_patch = conv_patch(oracle_patch);
        let (production_out, _new_state) =
            incremental_eval(&production_program, &production_state, &production_patch)
                .expect("production incremental_eval succeeds");

        let rel_names: BTreeSet<&str> = oracle_out
            .keys()
            .copied()
            .chain(oracle_facts.keys().copied())
            .collect();
        for rel in rel_names {
            let expected: BTreeSet<SignedFact> = oracle_out
                .get(rel)
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(conv_signed)
                .collect();
            let got = production_out.get(&sym(rel)).cloned().unwrap_or_default();
            assert_eq!(expected, got, "{ctx}: mismatch on relation '{rel}'");
        }
    }

    #[test]
    fn production_matches_oracle_generatively() {
        fn shape_a() -> Vec<laws::Rule> {
            vec![laws::Rule::plain(
                "q",
                vec![laws::Term::Var("X")],
                vec![
                    laws::Literal::pos("p", vec![laws::Term::Var("X"), laws::Term::Var("Y")]),
                    laws::Literal::neg("r", vec![laws::Term::Var("X")]),
                ],
            )]
        }
        fn shape_b() -> Vec<laws::Rule> {
            vec![
                laws::Rule::plain(
                    "mid",
                    vec![laws::Term::Var("X")],
                    vec![
                        laws::Literal::pos("p", vec![laws::Term::Var("X"), laws::Term::Var("Y")]),
                        laws::Literal::neg("r", vec![laws::Term::Var("X")]),
                    ],
                ),
                laws::Rule::plain(
                    "q",
                    vec![laws::Term::Var("X")],
                    vec![
                        laws::Literal::pos("mid", vec![laws::Term::Var("X")]),
                        laws::Literal::neg("s", vec![laws::Term::Var("X")]),
                    ],
                ),
            ]
        }
        fn shape_c() -> Vec<laws::Rule> {
            vec![laws::Rule::plain(
                "q",
                vec![laws::Term::Var("X"), laws::Term::Var("Y")],
                vec![
                    laws::Literal::pos("p", vec![laws::Term::Var("X"), laws::Term::Var("Y")]),
                    laws::Literal::pos("r2", vec![laws::Term::Var("X"), laws::Term::Var("Y")]),
                ],
            )]
        }
        // Shape D: `q(x, min(y)) :- p(x, y)` — aggregation, `min`
        // deliberately (the hardest kind: no per-kind incremental
        // formula covers retracting the current min).
        fn shape_d() -> Vec<laws::Rule> {
            vec![laws::Rule::aggregated(
                "q",
                vec![laws::Term::Var("X"), laws::Term::Var("Y")],
                vec![
                    None,
                    Some((
                        kyzo_model::program::aggregate::parse_aggr("min")
                            .unwrap()
                            .expect("real aggregation exists"),
                        vec![],
                    )),
                ],
                vec![laws::Literal::pos(
                    "p",
                    vec![laws::Term::Var("X"), laws::Term::Var("Y")],
                )],
            )]
        }
        let shapes: [fn() -> Vec<laws::Rule>; 4] = [shape_a, shape_b, shape_c, shape_d];

        let mut state: u64 = 0xFEED_FACE_C0FF_EE01;
        let mut next_u64 = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut next_range = |n: u64| next_u64() % n;

        let mut cases = 0;
        for shape in shapes {
            for _ in 0..60 {
                let rules = shape();
                let mut facts: BTreeMap<laws::Rel, BTreeSet<Tuple>> = BTreeMap::new();
                for rel in ["p", "r", "r2", "s"] {
                    let n = next_range(6);
                    let mut set = BTreeSet::new();
                    for _ in 0..n {
                        let a = v(next_range(4) as i64);
                        if rel == "p" || rel == "r2" {
                            set.insert(Tuple::from_vec(vec![a, v(next_range(4) as i64)]));
                        } else {
                            set.insert(Tuple::from_vec(vec![a]));
                        }
                    }
                    facts.insert(rel, set);
                }

                let mut patch: BTreeMap<laws::Rel, BTreeSet<laws::SignedFact>> = BTreeMap::new();
                let all = ["p", "r", "r2", "s"];
                let k = 1 + next_range(2) as usize;
                let mut chosen = Vec::new();
                while chosen.len() < k {
                    let rel = all[next_range(4) as usize];
                    if !chosen.contains(&rel) {
                        chosen.push(rel);
                    }
                }
                for rel in chosen {
                    let existing: Vec<Tuple> = facts[rel].iter().cloned().collect();
                    if !existing.is_empty() && next_range(2) == 0 {
                        let victim = existing[next_range(existing.len() as u64) as usize].clone();
                        patch
                            .entry(rel)
                            .or_default()
                            .insert(laws::SignedFact::Minus(victim));
                    } else {
                        let a = v(next_range(4) as i64);
                        let t: Tuple = if rel == "p" || rel == "r2" {
                            Tuple::from_vec(vec![a, v(next_range(4) as i64)])
                        } else {
                            Tuple::from_vec(vec![a])
                        };
                        patch
                            .entry(rel)
                            .or_default()
                            .insert(laws::SignedFact::Plus(t));
                    }
                }
                if patch.values().all(BTreeSet::is_empty) {
                    continue;
                }

                let oracle_program = laws::Program::untimed(rules, vec![], BTreeMap::new());
                assert_matches_oracle(&oracle_program, &facts, &patch, &format!("case {cases}"));
                cases += 1;
            }
        }
        assert!(
            cases > 100,
            "expected a rich production-vs-oracle campaign, ran {cases}"
        );
    }

    // ── Translation: a real (hand-built, but exactly the compiler's own
    // magic-tier shape) StratifiedMagicProgram -> IncrementalProgram. ──

    use kyzo_model::program::rule::Unification;
    use crate::exec::plan::program::{MagicProgram, MagicRelationApplyAtom, MagicRuleApplyAtom};

    fn muggle(name: &str) -> MagicSymbol {
        MagicSymbol::Muggle { inner: sym(name) }
    }
    fn rel_atom(name: &str, args: Vec<&str>, negated: bool) -> MagicAtom {
        let atom = MagicRelationApplyAtom {
            name: sym(name),
            args: args.into_iter().map(sym).collect(),
            validity: None,
            span: SourceSpan::default(),
        };
        if negated {
            MagicAtom::NegatedRelation(atom)
        } else {
            MagicAtom::Relation(atom)
        }
    }
    fn rule_atom(name: &str, args: Vec<&str>, negated: bool) -> MagicAtom {
        let atom = MagicRuleApplyAtom {
            name: muggle(name),
            args: args.into_iter().map(sym).collect(),
            span: SourceSpan::default(),
        };
        if negated {
            MagicAtom::NegatedRule(atom)
        } else {
            MagicAtom::Rule(atom)
        }
    }
    fn const_unif(binding: &str, val: DataValue) -> MagicAtom {
        MagicAtom::Unification(Unification {
            binding: sym(binding),
            expr: kyzo_model::program::expr::Expr::Const {
                val,
                span: SourceSpan::default(),
            },
            one_many_unif: false,
            span: SourceSpan::default(),
        })
    }
    fn magic_inline(head: Vec<&str>, body: Vec<MagicAtom>) -> MagicInlineRule {
        let aggr = (0..head.len()).map(|_| HeadAggrSlot::Plain).collect();
        MagicInlineRule {
            head: head.into_iter().map(sym).collect(),
            aggr,
            body,
        }
    }
    fn one_stratum_program(defs: Vec<(&str, Vec<MagicInlineRule>)>) -> StratifiedMagicProgram {
        let prog = defs
            .into_iter()
            .map(|(head, rules)| (muggle(head), MagicRulesOrFixed::Rules { rules }))
            .collect();
        StratifiedMagicProgram::from_execution_order(vec![MagicProgram { prog }])
            .expect("test strata are well-formed")
    }

    #[test]
    fn translate_a_plain_positive_and_negated_rule() {
        let magic = one_stratum_program(vec![(
            "?",
            vec![magic_inline(
                vec!["X"],
                vec![
                    rel_atom("p", vec!["X"], false),
                    rel_atom("r", vec!["X"], true),
                ],
            )],
        )]);
        let program = translate(magic).expect("translation succeeds");
        assert_eq!(program.rules.len(), 1);
        let rule = &program.rules[0];
        assert_eq!(rule.head_rel, sym("?"));
        assert_eq!(rule.head_args, vec![x()]);
        assert_eq!(rule.body.len(), 2);
        assert_eq!(rule.body[0].rel, sym("p"));
        assert!(!rule.body[0].is_negated());
        assert_eq!(rule.body[1].rel, sym("r"));
        assert!(rule.body[1].is_negated());
    }

    /// A rule reference (not a stored relation) uses the referenced
    /// rule's OWN MagicSymbol identity — its canonical Debug rendering,
    /// which is unique per adornment, not just the plain inner name.
    #[test]
    fn translate_a_rule_reference_uses_the_magic_symbol_identity() {
        let magic = one_stratum_program(vec![
            (
                "mid",
                vec![magic_inline(
                    vec!["X"],
                    vec![rel_atom("p", vec!["X"], false)],
                )],
            ),
            (
                "?",
                vec![magic_inline(
                    vec!["X"],
                    vec![rule_atom("mid", vec!["X"], false)],
                )],
            ),
        ]);
        let program = translate(magic).expect("translation succeeds");
        let entry_rule = program
            .rules
            .iter()
            .find(|r| r.head_rel == sym("?"))
            .expect("entry rule present");
        assert_eq!(entry_rule.body[0].rel, sym(&format!("{:?}", muggle("mid"))));
    }

    /// A constant hoisted into a `Unification` atom folds back into
    /// `Term::Const` on every literal (and the head) that shares its
    /// bound variable.
    #[test]
    fn translate_folds_a_constant_unification_into_term_const() {
        let magic = one_stratum_program(vec![(
            "?",
            vec![magic_inline(
                vec!["X", "Y"],
                vec![rel_atom("p", vec!["X", "Y"], false), const_unif("Y", v(42))],
            )],
        )]);
        let program = translate(magic).expect("translation succeeds");
        let rule = &program.rules[0];
        assert_eq!(rule.head_args, vec![x(), Term::Const(v(42))]);
        assert_eq!(rule.body[0].args, vec![x(), Term::Const(v(42))]);
    }

    /// `MagicInlineRule::aggr` is carried straight through translation
    /// (it is already this module's exact `HeadAggr` shape) — never
    /// refused.
    #[test]
    fn translate_carries_aggregation_through() {
        let mut inline = magic_inline(vec!["X", "Y"], vec![rel_atom("p", vec!["X", "Y"], false)]);
        let sum = kyzo_model::program::aggregate::parse_aggr("sum")
            .unwrap()
            .expect("real aggregation exists");
        inline.aggr = vec![
            HeadAggrSlot::Plain,
            HeadAggrSlot::Aggregated {
                aggr: sum,
                args: vec![],
            },
        ];
        let magic = one_stratum_program(vec![("?", vec![inline])]);
        let program = translate(magic).expect("translation succeeds");
        let rule = &program.rules[0];
        assert_eq!(rule.aggr.len(), 2);
        assert!(!rule.aggr[0].is_aggregated());
        assert_eq!(rule.aggr[1].as_aggregated().unwrap().0, &sum);
    }

    #[test]
    fn translate_refuses_fixed_rules() {
        use crate::rules::contract::{EmptyNamedRowsBody, FixedRuleHandle, SimpleFixedRule};
        let fixed_impl: std::sync::Arc<dyn crate::rules::contract::FixedRule> =
            std::sync::Arc::new(SimpleFixedRule::new(0, EmptyNamedRowsBody));
        let fixed = crate::exec::plan::program::MagicFixedRuleApply {
            fixed_handle: FixedRuleHandle::new("?", SourceSpan::default()),
            rule_args: vec![],
            options: kyzo_model::program::rule::FixedRuleOptions::empty(),
            span: SourceSpan::default(),
            arity: 1,
            fixed_impl,
        };
        let prog = BTreeMap::from([(muggle("?"), MagicRulesOrFixed::Fixed { fixed })]);
        let magic = StratifiedMagicProgram::from_execution_order(vec![MagicProgram { prog }])
            .expect("test strata are well-formed");
        let err = translate(magic).unwrap_err();
        assert_eq!(err, TranslationRejection::FixedRule);
    }

    #[test]
    fn translate_refuses_predicates_and_index_searches() {
        let magic_pred = one_stratum_program(vec![(
            "?",
            vec![magic_inline(
                vec!["X"],
                vec![
                    rel_atom("p", vec!["X"], false),
                    MagicAtom::Predicate(kyzo_model::program::expr::Expr::Const {
                        val: DataValue::Bool(true),
                        span: SourceSpan::default(),
                    }),
                ],
            )],
        )]);
        let err = translate(magic_pred).unwrap_err();
        assert_eq!(err, TranslationRejection::Unsupported("a predicate filter"));
    }

    /// A non-constant unification (a computed expression) has no
    /// representation in this module's `Term` and is refused, named.
    #[test]
    fn translate_refuses_non_constant_unification() {
        let magic = one_stratum_program(vec![(
            "?",
            vec![magic_inline(
                vec!["X", "Y"],
                vec![
                    rel_atom("p", vec!["X"], false),
                    MagicAtom::Unification(Unification {
                        binding: sym("Y"),
                        expr: kyzo_model::program::expr::Expr::Apply {
                            op: kyzo_model::program::op::OP_ADD,
                            args: Box::new([]),
                            span: SourceSpan::default(),
                        },
                        one_many_unif: false,
                        span: SourceSpan::default(),
                    }),
                ],
            )],
        )]);
        let err = translate(magic).unwrap_err();
        assert_eq!(
            err,
            TranslationRejection::Unsupported("a non-constant unification")
        );
    }

    /// End to end: translate, then run the SAME hard-corner scenario
    /// (retraction through negation) through `incremental_eval` on the
    /// translated program — proving translate() and incremental_eval()
    /// compose correctly, not just each in isolation.
    #[test]
    fn translated_program_runs_through_incremental_eval() {
        let magic = one_stratum_program(vec![(
            "?",
            vec![magic_inline(
                vec!["X"],
                vec![
                    rel_atom("p", vec!["X"], false),
                    rel_atom("r", vec!["X"], true),
                ],
            )],
        )]);
        let program = translate(magic).expect("translation succeeds");
        let state = state_of(vec![
            ("p", vec![Tuple::from_vec(vec![v(1)])]),
            ("r", vec![Tuple::from_vec(vec![v(1)])]),
        ]);
        let patch = patch_of(vec![("r", SignedFact::Minus(Tuple::from_vec(vec![v(1)])))]);
        let (deltas, _new_state) = incremental_eval(&program, &state, &patch).unwrap();
        assert_eq!(
            deltas[&sym("?")],
            [SignedFact::Plus(Tuple::from_vec(vec![v(1)]))]
                .into_iter()
                .collect()
        );
    }
