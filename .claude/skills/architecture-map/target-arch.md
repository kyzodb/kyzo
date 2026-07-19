# KyzoDB ideal target architecture

Flat path list of known target-state files.
Built from: deprecated L1 targets ∪ tree files that are not census sources.
`crates/kyzo-core/src/model/*` idealized to `crates/kyzo-model/src/*`.

Format: `path` — description

`crates/kyzo-arrow-interop/src/lib.rs` — foreign Arrow bytes lifted into Kyzo wire values at the interop boundary
`crates/kyzo-arrow-interop/tests/decode_kyzo_stream.rs` — proves the Arrow decode door refuses garbage and round-trips lawful streams
`crates/kyzo-bin/src/bulk.rs` — native host path that streams bulk facts into the engine without a query script
`crates/kyzo-bin/src/engine.rs` — holds the Engine composition (config-once genesis/arm injection through the composition root; not a live Db)
`crates/kyzo-bin/src/main.rs` — composition root that wires config into REPL or HTTP and never imports engine internals past the sealed door
`crates/kyzo-bin/src/repl/commands.rs` — maps typed REPL verbs onto sealed engine calls
`crates/kyzo-bin/src/repl/editor.rs` — interactive line editing and history for the console host
`crates/kyzo-bin/src/repl/fetch.rs` — pulls remote script or data payloads into the REPL session
`crates/kyzo-bin/src/repl/mod.rs` — owns the interactive console host loop
`crates/kyzo-bin/src/repl/render.rs` — turns engine NamedRows into human-readable console output
`crates/kyzo-bin/src/server/auth.rs` — authenticates HTTP callers before any engine admission
`crates/kyzo-bin/src/server/bulk.rs` — HTTP door for bulk fact ingest
`crates/kyzo-bin/src/server/console.rs` — browser/console HTTP surface over the same sealed engine
`crates/kyzo-bin/src/server/feeds.rs` — HTTP door for change feeds and standing-query subscriptions
`crates/kyzo-bin/src/server/query.rs` — HTTP door that admits KyzoScript and returns rows
`crates/kyzo-bin/src/server/rules.rs` — HTTP door that invokes fixed rules with typed inputs
`crates/kyzo-bin/tests/repl_smoke.rs` — end-to-end smoke that the REPL host can start and answer one script
`crates/kyzo-core/benches/bench_api.rs` — shared harness so permanent benches talk to the engine the same way
`crates/kyzo-core/benches/db_scan.rs` — measures ordered substrate scan cost under realistic keys
`crates/kyzo-core/benches/ra_exec.rs` — measures relational operator / plan execution throughput
`crates/kyzo-core/benches/storage.rs` — measures fjall put/get/commit cost in isolation
`crates/kyzo-core/benches/string_eq.rs` — measures string equality where binary order must stay semantic order
`crates/kyzo-core/examples/language_tour.rs` — teaching script that walks KyzoScript surface features against a live Db
`crates/kyzo-model/src/value/arena.rs` — order-preserving interning dictionary: dense Code, epoch observers, Admission/Denial mint law
`crates/kyzo-model/src/value/row.rs` — admitted row/tuple-key currency of the value plane (storage key + scan bounds)
`crates/kyzo-core/src/exec/expr/batch.rs` — vectorized expression evaluation over columnar batches
`crates/kyzo-core/src/exec/expr/eval.rs` — scalar expression evaluation against bound variables
`crates/kyzo-core/src/exec/fixpoint/delta_store.rs` — delta relation store that feeds stratified fixpoint rounds
`crates/kyzo-core/src/exec/fixpoint/eval.rs` — stratified Datalog fixpoint engine
`crates/kyzo-core/src/exec/fixpoint/parallel.rs` — parallel scheduling of independent fixpoint strata / work
`crates/kyzo-core/src/exec/fold/aggr.rs` — aggregation folds that reduce bags of values to declared aggregates
`crates/kyzo-core/src/exec/fold/sketch/aggr.rs` — approximate aggregation via sketch folds
`crates/kyzo-core/src/exec/fold/sketch/count_min.rs` — Count-Min frequency sketch fold
`crates/kyzo-core/src/exec/fold/sketch/hll.rs` — HyperLogLog cardinality sketch fold
`crates/kyzo-core/src/exec/fold/sketch/mod.rs` — sketch-fold family used when exact aggregates are refused
`crates/kyzo-core/src/exec/fold/sketch/tdigest.rs` — t-digest quantile sketch fold
`crates/kyzo-core/src/exec/mod.rs` — evaluation zone: plans, operators, fixpoint, provenance — never persistence
`crates/kyzo-core/src/exec/op/batch_ops.rs` — batch-oriented relational primitives shared by operators
`crates/kyzo-core/src/exec/op/delta.rs` — emits or consumes differential deltas for incremental/fixpoint work
`crates/kyzo-core/src/exec/op/join.rs` — relational join of two inputs under declared keys
`crates/kyzo-core/src/exec/op/literal.rs` — materializes an in-memory literal relation as an operator input
`crates/kyzo-core/src/exec/op/mod.rs` — RelAlgebra spine: total constructors, typed invariants, batched dispatch, explain substrate; sibling op files are the operators
`crates/kyzo-core/src/exec/op/neg.rs` — stratified negation against a positive relation
`crates/kyzo-core/src/exec/op/search.rs` — drives projection/search indexes (vector, FTS, spatial, …) from the plan
`crates/kyzo-core/src/exec/op/stored.rs` — ordered scan of a stored relation through the store contract
`crates/kyzo-core/src/exec/op/temporal.rs` — time-travel / validity-window scan of stored facts
`crates/kyzo-core/src/exec/op/transform.rs` — project, filter, and reshape operators on intermediate rows
`crates/kyzo-core/src/exec/plan/compile.rs` — lowers normalized program IR into an executable plan
`crates/kyzo-core/src/exec/plan/expr.rs` — plan-level expression nodes bound into operators
`crates/kyzo-core/src/exec/plan/graph.rs` — dependency graph of plan nodes for scheduling and strata
`crates/kyzo-core/src/exec/plan/magic.rs` — magic-set rewriting that specializes recursive rules to queries
`crates/kyzo-core/src/exec/plan/normalize.rs` — canonicalizes program IR before compile
`crates/kyzo-core/src/exec/plan/program.rs` — program IR shapes the planner consumes
`crates/kyzo-core/src/exec/plan/search.rs` — plans index-backed search atoms into search operators
`crates/kyzo-core/src/exec/plan/stratify.rs` — computes negation/aggregate strata and refuses illegal cycles
`crates/kyzo-core/src/exec/provenance/counted.rs` — multiplicity-tracking provenance annotations on derived facts
`crates/kyzo-core/src/exec/provenance/eval.rs` — evaluates provenance alongside ordinary derivation
`crates/kyzo-core/src/exec/provenance/semiring.rs` — semiring algebra that defines how provenance combines
`crates/kyzo-core/src/exec/sort.rs` — orders result rows under the one law’s semantic order
`crates/kyzo-core/src/exec/stdlib/bind.rs` — bind_op + bind::resolve_op: sole mint of BoundOp from OpDecl + body, and the sole public name→BoundOp registry
`crates/kyzo-core/src/exec/stdlib/bound_op.rs` — BoundOp: OpDecl paired with a total body fn; private mint via bind only
`crates/kyzo-core/src/exec/stdlib/collection.rs` — list and JSON algebra kernels (not evidence/chunks-as-truth)
`crates/kyzo-core/src/exec/stdlib/compare.rs` — language equality/order/type predicates and assert; cross-type refuse (not Tag order)
`crates/kyzo-core/src/exec/stdlib/convert.rs` — eval-time value construction/conversion (to_*, vec, uuid fields, validity mint)
`crates/kyzo-core/src/exec/stdlib/errors.rs` — typed numeric/domain refuse + NaN checkpoint helpers both evaluators share
`crates/kyzo-core/src/exec/stdlib/geo.rs` — geospatial numeric kernels
`crates/kyzo-core/src/exec/stdlib/interval.rs` — Allen and interval-value kernels beside the Interval kind
`crates/kyzo-core/src/exec/stdlib/metric.rs` — ResultValue distance/score kernels for Candidates membership (never TagOrdered; not vec ctor)
`crates/kyzo-core/src/exec/stdlib/mod.rs` — stdlib module tree and re-exports; owns no domain meaning
`crates/kyzo-core/src/exec/stdlib/nondet.rs` — ambient clock and rng ops; determinism-as-data; never authority for what exists
`crates/kyzo-core/src/exec/stdlib/numeric.rs` — pure numeric / bit kernels over DataValue
`crates/kyzo-core/src/exec/stdlib/temporal_format.rs` — timestamp format/parse as values; not validity admission
`crates/kyzo-core/src/exec/stdlib/text.rs` — string/regex kernels over string values
`crates/kyzo-core/src/format/tests.rs` — locks encode/decode round-trips where binary order equals semantic order
`crates/kyzo-core/src/lib.rs` — sealed public door of the engine crate; hosts import only what this re-exports
`crates/kyzo-core/src/project/contract.rs` — trait boundary every rebuildable projection must satisfy
`crates/kyzo-core/src/project/current.rs` — which projection build is live for readers right now
`crates/kyzo-core/src/project/dedup/lsh.rs` — LSH projection that finds near-duplicate documents/rows
`crates/kyzo-core/src/project/gazetteer.rs` — entity gazetteer projection for name lookup
`crates/kyzo-core/src/project/mod.rs` — projection zone: rebuildable indexes derived from stored facts
`crates/kyzo-core/src/project/projection.rs` — lifecycle of building, sealing, and querying a projection
`crates/kyzo-core/src/project/residency.rs` — which projection pages/segments are resident in memory
`crates/kyzo-core/src/project/sparse/sparse.rs` — sparse vector index projection
`crates/kyzo-core/src/project/sparse/sparse_hostile.rs` — adversarial sparse-index cases that must not corrupt order or recall
`crates/kyzo-core/src/project/spatial/spatial.rs` — geospatial index projection over stored geometries
`crates/kyzo-core/src/project/text/README.md` — design notes for the FTS analyzer / tokenizer tree
`crates/kyzo-core/src/project/text/alphanum_only.rs` — drops non-alphanumeric tokens from the FTS stream
`crates/kyzo-core/src/project/text/ascii_folding_filter.rs` — folds accented characters into ASCII for FTS
`crates/kyzo-core/src/project/text/ast.rs` — structured FTS query tree before it hits the index
`crates/kyzo-core/src/project/text/cangjie/mod.rs` — Chinese Cangjie analyzer family
`crates/kyzo-core/src/project/text/cangjie/options.rs` — configuration for Cangjie tokenization
`crates/kyzo-core/src/project/text/cangjie/stream.rs` — Cangjie token stream producer
`crates/kyzo-core/src/project/text/cangjie/tokenizer.rs` — Cangjie tokenizer for Chinese text
`crates/kyzo-core/src/project/text/empty_tokenizer.rs` — tokenizer that emits no tokens (pipeline edge case)
`crates/kyzo-core/src/project/text/fts.rs` — full-text inverted index projection and query
`crates/kyzo-core/src/project/text/lower_caser.rs` — lowercases tokens before indexing/query
`crates/kyzo-core/src/project/text/mod.rs` — text analysis and FTS projection subtree
`crates/kyzo-core/src/project/text/ngram_tokenizer.rs` — breaks text into overlapping character n-grams
`crates/kyzo-core/src/project/text/options.rs` — FTS index and analyzer options
`crates/kyzo-core/src/project/text/raw_tokenizer.rs` — passes the whole string through as one token
`crates/kyzo-core/src/project/text/remove_long.rs` — drops tokens longer than a configured limit
`crates/kyzo-core/src/project/text/simple_tokenizer.rs` — whitespace/punctuation simple split for FTS
`crates/kyzo-core/src/project/text/split_compound_words.rs` — splits compound words into FTS subtokens
`crates/kyzo-core/src/project/text/stemmer.rs` — reduces tokens to stems for FTS recall
`crates/kyzo-core/src/project/text/stop_word_filter/gen_stopwords.py` — generates language stopword tables checked into the tree
`crates/kyzo-core/src/project/text/stop_word_filter/mod.rs` — filters stopwords out of the FTS token stream
`crates/kyzo-core/src/project/text/stop_word_filter/stopwords.rs` — per-language stopword tables
`crates/kyzo-core/src/project/text/stream.rs` — token-stream trait the analyzer pipeline implements
`crates/kyzo-core/src/project/text/tokenized_string.rs` — string already broken into tokens for indexing
`crates/kyzo-core/src/project/text/tokenizer.rs` — tokenizer trait and dispatch into concrete analyzers
`crates/kyzo-core/src/project/text/tokenizer/alphanum_only.rs` — alphanumeric-only filter in the tokenizer subtree
`crates/kyzo-core/src/project/text/tokenizer/ascii_folding_filter.rs` — ASCII folding in the tokenizer subtree
`crates/kyzo-core/src/project/text/tokenizer/empty_tokenizer.rs` — empty tokenizer in the tokenizer subtree
`crates/kyzo-core/src/project/text/tokenizer/lower_caser.rs` — lowercase filter in the tokenizer subtree
`crates/kyzo-core/src/project/text/tokenizer/mod.rs` — nested tokenizer implementations (target layout may collapse with text/)
`crates/kyzo-core/src/project/text/tokenizer/ngram_tokenizer.rs` — n-gram tokenizer in the tokenizer subtree
`crates/kyzo-core/src/project/text/tokenizer/raw_tokenizer.rs` — raw tokenizer in the tokenizer subtree
`crates/kyzo-core/src/project/text/tokenizer/remove_long.rs` — long-token filter in the tokenizer subtree
`crates/kyzo-core/src/project/text/tokenizer/simple_tokenizer.rs` — simple tokenizer in the tokenizer subtree
`crates/kyzo-core/src/project/text/tokenizer/split_compound_words.rs` — compound splitter in the tokenizer subtree
`crates/kyzo-core/src/project/text/tokenizer/stemmer.rs` — stemmer in the tokenizer subtree
`crates/kyzo-core/src/project/text/tokenizer/stop_word_filter/gen_stopwords.py` — stopword generator under tokenizer/
`crates/kyzo-core/src/project/text/tokenizer/stop_word_filter/mod.rs` — stopword filter under tokenizer/
`crates/kyzo-core/src/project/text/tokenizer/stop_word_filter/stopwords.rs` — stopword tables under tokenizer/
`crates/kyzo-core/src/project/text/tokenizer/tokenized_string.rs` — tokenized string under tokenizer/
`crates/kyzo-core/src/project/text/tokenizer/tokenizer_impl.rs` — shared tokenizer implementation details
`crates/kyzo-core/src/project/text/tokenizer/whitespace_tokenizer.rs` — whitespace-only tokenizer in the tokenizer subtree
`crates/kyzo-core/src/project/text/tokenizer_impl.rs` — shared tokenizer implementation details at text/
`crates/kyzo-core/src/project/text/whitespace_tokenizer.rs` — whitespace-only tokenizer at text/
`crates/kyzo-core/src/project/vector/hnsw.rs` — HNSW dense vector ANN projection
`crates/kyzo-core/src/project/vector/hnsw_filter_harness.rs` — harness for filtered HNSW search correctness under predicates
`crates/kyzo-core/src/react/incremental.rs` — maintains derived relations as stored facts change, without full recompute
`crates/kyzo-core/src/react/standing.rs` — registers and fires standing queries when matching mutations arrive
`crates/kyzo-core/src/rules/algo/all_pairs_shortest_path.rs` — fixed rule: all-pairs shortest paths over a graph view
`crates/kyzo-core/src/rules/algo/astar.rs` — fixed rule: A* shortest path with a declared heuristic
`crates/kyzo-core/src/rules/algo/bfs.rs` — fixed rule: breadth-first traversal from declared seeds
`crates/kyzo-core/src/rules/algo/cliques.rs` — fixed rule: enumerates cliques in a graph view
`crates/kyzo-core/src/rules/algo/degree_centrality.rs` — fixed rule: degree centrality scores
`crates/kyzo-core/src/rules/algo/dfs.rs` — fixed rule: depth-first traversal from declared seeds
`crates/kyzo-core/src/rules/algo/dijkstra.rs` — fixed rule: Dijkstra shortest paths with edge weights
`crates/kyzo-core/src/rules/algo/k_core.rs` — fixed rule: k-core decomposition
`crates/kyzo-core/src/rules/algo/kruskal.rs` — fixed rule: Kruskal minimum spanning tree
`crates/kyzo-core/src/rules/algo/label_propagation.rs` — fixed rule: community labels via label propagation
`crates/kyzo-core/src/rules/algo/louvain.rs` — fixed rule: Louvain community detection
`crates/kyzo-core/src/rules/algo/max_flow.rs` — fixed rule: maximum flow between source and sink
`crates/kyzo-core/src/rules/algo/mod.rs` — graph algorithm fixed rules invoked from KyzoScript
`crates/kyzo-core/src/rules/algo/pagerank.rs` — fixed rule: PageRank scores on a graph view
`crates/kyzo-core/src/rules/algo/prim.rs` — fixed rule: Prim minimum spanning tree
`crates/kyzo-core/src/rules/algo/random_walk.rs` — fixed rule: random walks on a graph view
`crates/kyzo-core/src/rules/algo/reorder_sort.rs` — fixed rule: reorders/sorts relation rows under declared keys
`crates/kyzo-core/src/rules/algo/scc.rs` — fixed rule: strongly connected components
`crates/kyzo-core/src/rules/algo/shortest_path_bfs.rs` — fixed rule: unweighted shortest paths via BFS
`crates/kyzo-core/src/rules/algo/top_sort.rs` — fixed rule: topological order of a DAG view
`crates/kyzo-core/src/rules/algo/triangles.rs` — fixed rule: triangle counting on a graph view
`crates/kyzo-core/src/rules/algo/yen.rs` — fixed rule: Yen’s k-shortest loopless paths
`crates/kyzo-core/src/rules/contract.rs` — FixedRule trait + SessionFixedRule: typed inputs, named outputs, deterministic run; session-backed FixedRuleEval adapter
`crates/kyzo-core/src/rules/gazetteer.rs` — fixed rule that queries the gazetteer projection
`crates/kyzo-core/src/rules/graph_view.rs` — adapts stored edge/vertex relations into the graph algorithms’ view
`crates/kyzo-core/src/rules/io/constant.rs` — fixed rule that emits a declared constant table
`crates/kyzo-core/src/rules/io/csv.rs` — fixed rule that reads/writes CSV at the IO boundary
`crates/kyzo-core/src/rules/io/jlines.rs` — fixed rule that reads/writes JSON Lines at the IO boundary
`crates/kyzo-core/src/rules/io/mod.rs` — IO fixed rules that cross into foreign file formats
`crates/kyzo-core/src/rules/mod.rs` — fixed-rule zone: deterministic algorithms and IO rules callable from scripts
`crates/kyzo-core/src/rules/parallel.rs` — runs fixed-rule work across threads where the contract allows
`crates/kyzo-core/src/rules/rng.rs` — fixed rule that supplies controlled randomness under an explicit seed
`crates/kyzo-core/src/session/access.rs` — who may read or write which relations in this session
`crates/kyzo-core/src/session/admit.rs` — admits external requests into typed engine work or refuses with reason
`crates/kyzo-core/src/session/capacity.rs` — enforces memory/row/time budgets on a live session
`crates/kyzo-core/src/session/catalog.rs` — named relations, schemas, and metadata visible to the session
`crates/kyzo-core/src/session/composition.rs` — CompositionId + BestEffort|Saga|ReadAt
`crates/kyzo-core/src/session/constraint.rs` — integrity constraints checked on mutate/commit
`crates/kyzo-core/src/session/db.rs` — Engine(Store, Catalog) composition seat: Engine holds Store/Catalog capabilities by composition; not an ambient Db facade. Owns SessionView + SessionNormalizer (session-backed catalog/temp view and body normalizer). §1 obligation: current currency is still named `Db` until the storage epic demolishes it into Engine composition — do not half-rename here.
`crates/kyzo-core/src/session/footprint.rs` — AskShape + Footprint algebra + Frontier
`crates/kyzo-core/src/session/fts.rs` — session door that builds/queries the FTS projection
`crates/kyzo-core/src/session/generation.rs` — generation/epoch counters that invalidate stale handles
`crates/kyzo-core/src/session/hnsw.rs` — session door that builds/queries the HNSW projection
`crates/kyzo-core/src/session/jobs.rs` — background jobs owned by the session (rebuilds, feeds, …)
`crates/kyzo-core/src/session/json.rs` — JSON helpers at the session boundary for host-facing payloads
`crates/kyzo-core/src/session/lsh.rs` — session door that builds/queries the LSH dedup projection
`crates/kyzo-core/src/session/mod.rs` — session zone: live handles, admission, catalog — not evaluation itself
`crates/kyzo-core/src/session/normalize.rs` — normalizes host inputs into model types before admission
`crates/kyzo-core/src/session/observe.rs` — observation/metrics hooks on session activity
`crates/kyzo-core/src/session/ops.rs` — system and catalog operations exposed on the live Db
`crates/kyzo-core/src/session/pinned_handle.hex` — golden bytes for pinned handle layout / stability checks
`crates/kyzo-core/src/session/spatial.rs` — session door that builds/queries the spatial projection
`crates/kyzo-core/src/session/verify.rs` — session door that runs store integrity verification
`crates/kyzo-core/src/store/authority.rs` — WriteAuthority + IncarnationMintCap/IncarnationId + RecoveryMatrix + address fence
`crates/kyzo-core/src/store/backup.rs` — backup and restore of the ordered substrate
`crates/kyzo-core/src/store/commit_cap.rs` — StableCommitCap closed sum + SnapshotFork + ForkGenerationWitness
`crates/kyzo-core/src/store/compact.rs` — pace=f(debt) + MergeProof + range-class classifier
`crates/kyzo-core/src/store/contract.rs` — store trait: ordered put/get/scan/commit the rest of the engine depends on
`crates/kyzo-core/src/store/crypto.rs` — DEK/KEK/ShredSalt/WrappedShredSalt/AuditKey/AEAD pipeline
`crates/kyzo-core/src/store/epoch.rs` — FenceEpoch + CryptoDomain + EpochGrant + IntentClear
`crates/kyzo-core/src/store/failure.rs` — StoreRefuse closed enum + failure lattice + debt + operator surface
`crates/kyzo-core/src/store/fjall.rs` — fjall-backed implementation of the store contract
`crates/kyzo-core/src/store/grants.rs` — ForkGrant/RecoveryGrant + materialize + AncestorReadGrant
`crates/kyzo-core/src/store/idempotency.rs` — OperationKey + OperationOutcome + request_digest memo
`crates/kyzo-core/src/store/keys.rs` — key layout so binary order of stored keys matches semantic order
`crates/kyzo-core/src/store/merkle.rs` — merkle proofs over stored ranges for integrity
`crates/kyzo-core/src/store/nonce.rs` — NonceLease + MintDomain + DomainCounter + pure nonce fn
`crates/kyzo-core/src/store/objects.rs` — ObjectSlot/staging/permanence/GC seam
`crates/kyzo-core/src/store/open.rs` — StoreId + StoreOpen + genesis construction
`crates/kyzo-core/src/store/replica.rs` — AdmissionCertificate + verify_replica + ReplicaCustody
`crates/kyzo-core/src/store/retry.rs` — retry policy for transient store IO failures
`crates/kyzo-core/src/store/scratch.rs` — ephemeral scratch space for evaluation that must not leak into durable state
`crates/kyzo-core/src/store/seal.rs` — CheckpointSeal + truncate(consumes seal) + seal verification
`crates/kyzo-core/src/store/skip_walk.rs` — skip-list style walk over ordered keys
`crates/kyzo-core/src/store/sweep.rs` — SweepDoor + IntentionQueue + AdmittedIntent + IntentOrdinal/CommitOrdinal + Committed
`crates/kyzo-core/src/store/time.rs` — validity timestamps and temporal addressing in the store
`crates/kyzo-core/src/store/transcript.rs` — CanonicalTranscript + golden vectors + unknown-version refuse
`crates/kyzo-core/src/store/tx.rs` — write transactions with commit/abort over the substrate
`crates/kyzo-core/src/store/verify_walk.rs` — full ordered walk used by integrity verification
`crates/kyzo-core/src/store/wal.rs` — WAL segment format + cross-segment hash chain + replay + mutable floors
`crates/kyzo-core/tests/adversarial_robustness.rs` — public-surface cases that try to break order, admission, or safety
`crates/kyzo-core/tests/aggregation.rs` — public-surface aggregation behavior through the sealed API
`crates/kyzo-core/tests/arity_compile_fail.rs` — trybuild suite: illegal arities must not compile
`crates/kyzo-core/tests/common/mod.rs` — shared fixtures and helpers for integration tests
`crates/kyzo-core/tests/compile_fail/arity_zero_refused.rs` — zero-arity constructs must be a compile error
`crates/kyzo-core/tests/compile_fail/commit_failure_downcast_ref_refused.rs` — commit failures must not be downcast via ref escapes
`crates/kyzo-core/tests/compile_fail/projection_query_on_builder.rs` — unfinished projection builders must not accept queries
`crates/kyzo-core/tests/compile_fail/projection_query_on_stale.rs` — stale projection handles must not accept queries
`crates/kyzo-core/tests/compile_fail/storage_key_rejects_tuple_key.rs` — raw tuples must not be usable as storage keys
`crates/kyzo-core/tests/compile_fail/validity_raw_i64_refused.rs` — bare i64 must not stand in for Validity
`crates/kyzo-core/tests/compile_fail/write_tx_use_after_abort.rs` — write tx must be unusable after abort
`crates/kyzo-core/tests/compile_fail/write_tx_use_after_commit.rs` — write tx must be unusable after commit
`crates/kyzo-core/tests/data_types.rs` — public-surface coverage of value kinds and typing
`crates/kyzo-core/tests/errors_and_refusals.rs` — public-surface typed refusals and error shapes
`crates/kyzo-core/tests/key_shape_compile_fail.rs` — trybuild suite: illegal key shapes must not compile
`crates/kyzo-core/tests/projection_compile_fail.rs` — trybuild suite: projection lifecycle violations must not compile
`crates/kyzo-core/tests/public_api_surface.rs` — locks what the sealed door exports and how it behaves
`crates/kyzo-core/tests/recursion_and_negation.rs` — public-surface recursive rules and stratified negation
`crates/kyzo-core/tests/relational_core.rs` — public-surface joins, filters, and core relational queries
`crates/kyzo-core/tests/standing_queries.rs` — public-surface standing query registration and fire
`crates/kyzo-core/tests/storage_allocation_law.rs` — enforces allocation/admission law at the storage boundary
`crates/kyzo-core/tests/system_ops.rs` — public-surface system/catalog operations
`crates/kyzo-core/tests/time_travel.rs` — public-surface as-of / validity window queries
`crates/kyzo-core/tests/unified_scenario.rs` — multi-feature scenario through the sealed API
`crates/kyzo-core/tests/validity_compile_fail.rs` — trybuild suite: Validity mistypes must not compile
`crates/kyzo-core/tests/vector_and_fts.rs` — public-surface vector ANN and full-text search
`crates/kyzo-core/tests/write_tx_compile_fail.rs` — trybuild suite: write-tx lifecycle violations must not compile
`crates/kyzo-crashfs/src/fault.rs` — injectable faults (crash, delay, corrupt) under the fake FS
`crates/kyzo-crashfs/src/harness.rs` — mounts crashfs and drives crash campaigns against a Db
`crates/kyzo-crashfs/src/lib.rs` — fault-injecting filesystem crate used by durability trials
`crates/kyzo-crashfs/src/passthrough.rs` — passthrough FS layer that can interpose faults on real IO
`crates/kyzo-crashfs/src/sim.rs` — in-process simulated FS for deterministic crash testing
`crates/kyzo-crashfs/tests/standalone_mount.rs` — proves crashfs can mount and serve without the full trials stack
`crates/kyzo-lsp/src/translate.rs` — maps LSP requests/responses onto sealed engine parse and query doors
`crates/kyzo-model/src/envelope/json.rs` — DataValue ↔ JSON / serde wire conversions (NamedRows diagnostic envelopes stay in kyzo-core data/json)
`crates/kyzo-model/src/envelope/arrow.rs` — Arrow-shaped wire envelope for values crossing process boundaries
`crates/kyzo-model/src/format.rs` — KyzoScript pretty-printer (proof → one canonical source text); value byte encode is value/canonical.rs
`crates/kyzo-model/src/format/tests.rs` — encoding-law battery ownership: corpus + laws 1–3 (round-trip, exhaustive-pairwise order embedding, no-panic / byte-flip harness); the one law's property suite, not store scenarios
`crates/kyzo-model/src/lib.rs` — vocabulary crate root: types and parse, no engine state
`crates/kyzo-model/src/parse/expr.rs` — lifts KyzoScript expression syntax into program IR
`crates/kyzo-model/src/parse/grammar.pest` — pest grammar that owns KyzoScript surface syntax
`crates/kyzo-model/src/parse/mod.rs` — parse zone: text → typed IR with spans and refusals
`crates/kyzo-model/src/parse/query.rs` — lifts query/rule syntax into program IR
`crates/kyzo-model/src/parse/schema.rs` — lifts schema declaration syntax into schema types
`crates/kyzo-model/src/parse/script.rs` — lifts multi-statement scripts into program IR
`crates/kyzo-model/src/parse/search.rs` — lifts search-atom syntax (vector/FTS/…) into program IR
`crates/kyzo-model/src/parse/sys.rs` — lifts system-op syntax into typed system requests
`crates/kyzo-model/src/program/aggregate.rs` — aggregate declaration shapes in program IR
`crates/kyzo-model/src/program/expr.rs` — expression AST used after parse and before exec
`crates/kyzo-model/src/program/op.rs` — builtin op declarations: name, arity, determinism-as-data (no bodies)
`crates/kyzo-model/src/program/span.rs` — source spans carried on every IR node for refusals
`crates/kyzo-model/src/program/symbol.rs` — interned/program symbols naming relations and variables
`crates/kyzo-model/src/schema/column.rs` — column name and type in a relation schema
`crates/kyzo-model/src/schema/relation.rs` — relation schema: keys, values, constraints as vocabulary
`crates/kyzo-model/src/typestate.rs` — Unset/Set builders so incomplete construction cannot be sealed
`crates/kyzo-model/src/value/admission.rs` — rules for admitting raw inputs into lawful values
`crates/kyzo-model/src/value/arity.rs` — arity as a first-class constrained count
`crates/kyzo-model/src/value/bytes_qty.rs` — byte-quantity newtype (sizes/limits), not a bare usize
`crates/kyzo-core/src/data/json.rs` — NamedRows + diagnostic JSON envelopes (composes kyzo-model envelope/json)
`crates/kyzo-model/src/value/canonical.rs` — canonicalization so equal values share one byte form
`crates/kyzo-model/src/value/code.rs` — dense interned Code / StampedCode handles (epoch-scoped)
`crates/kyzo-model/src/value/column.rs` — columnar admitted codes over a Domain
`crates/kyzo-model/src/value/cell.rs` — single stored/query cell holding one tagged value
`crates/kyzo-model/src/value/kind/collection.rs` — list/set/map-like value kinds and their order
`crates/kyzo-model/src/value/kind/interval.rs` — interval value kind (bounds as variants, not sentinels)
`crates/kyzo-model/src/value/kind/json.rs` — JSON document value kind
`crates/kyzo-model/src/value/kind/mod.rs` — sum of specialized value kinds under Tag
`crates/kyzo-model/src/value/kind/regex.rs` — regex value kind
`crates/kyzo-model/src/value/kind/uuid.rs` — UUID value kind
`crates/kyzo-model/src/value/kind/validity.rs` — temporal validity value kind
`crates/kyzo-model/src/value/kind/vector.rs` — dense/sparse vector value kinds
`crates/kyzo-model/src/value/mod.rs` — DataValue: owned logical value face and domain value vocabulary the one law binds; kyzo-model owns this type (not data/value)
`crates/kyzo-model/src/value/number.rs` — numeric value kinds with total order-safe encoding
`crates/kyzo-model/src/value/prefix.rs` — byte prefixes that keep cross-kind order coherent
`crates/kyzo-model/src/value/proofs.rs` — executable proofs that Ord and encode agree
`crates/kyzo-model/src/value/search_hits.rs` — search-result hit values returned from projections
`crates/kyzo-model/src/value/string.rs` — string value kind under Tag
`crates/kyzo-model/src/value/tag.rs` — single cross-kind discriminant that owns type order
`crates/kyzo-model/src/value/validity_coerce.rs` — one `@` / write-coordinate coercion law shared by parse and mutate
`crates/kyzo-oracle/src/eval.rs` — independent reference evaluator for conformance against the engine
`crates/kyzo-oracle/src/incremental.rs` — reference incremental semantics for standing/delta campaigns
`crates/kyzo-oracle/src/lib.rs` — oracle crate: reference semantics, not the production engine
`crates/kyzo-oracle/src/temporal.rs` — reference temporal/as-of semantics for time-travel campaigns
`crates/kyzo-trials/src/conformance.rs` — campaign: engine answers must match the oracle
`crates/kyzo-trials/src/crash.rs` — campaign: crash/restart must not lose committed facts
`crates/kyzo-trials/src/determinism.rs` — campaign: same inputs must yield the same ordered answers
`crates/kyzo-trials/src/dst.rs` — campaign: deterministic simulation of schedules and faults
`crates/kyzo-trials/src/fuzz.rs` — campaign: fuzzed scripts/bytes must not panic or corrupt
`crates/kyzo-trials/src/gauntlet.rs` — campaign: hostile multi-feature query gauntlet
`crates/kyzo-trials/src/lib.rs` — long-running trial campaigns outside ordinary unit tests
`crates/kyzo-trials/src/provenance.rs` — campaign: provenance annotations stay consistent with derivation
`crates/kyzo-trials/src/serializability.rs` — campaign: concurrent txs obey serializability expectations
`crates/kyzo-trials/src/time_travel/mod.rs` — temporal trial lane: script-surface and full-path batteries, split by kind of proof
`crates/kyzo-trials/src/time_travel/path.rs` — campaign: as-of through compile→RA→eval vs naive oracle (full path)
`crates/kyzo-trials/src/time_travel/script.rs` — campaign: as-of through Db::run_script + real `@` KyzoScript (language surface)
`crates/xtask/src/allowlist.rs` — which paths each mechanical gate is allowed to touch
`crates/xtask/src/checks/agreement_registry.rs` — gate: agreement/registry invariants hold in the tree
`crates/xtask/src/checks/allocation_admission.rs` — gate: allocations cross an admission boundary, not ad hoc
`crates/xtask/src/checks/authority_graph.rs` — gate: module authority edges match the ruled graph
`crates/xtask/src/checks/boundary_closure.rs` — gate: host/engine boundary does not leak internals
`crates/xtask/src/checks/build_script_sandbox.rs` — gate: build scripts stay sandboxed
`crates/xtask/src/checks/copy_detector.rs` — gate: forbids duplicated logic that should be one seat
`crates/xtask/src/checks/dead_code_ratchet.rs` — gate: dead code may only shrink, never grow silently
`crates/xtask/src/checks/derive_bypass.rs` — gate: forbids Ord/Hash derives that bypass the one law
`crates/xtask/src/checks/mod.rs` — mechanical law checks run by the gate
`crates/xtask/src/checks/panic_lint.rs` — gate: forbids panic/unwrap on reachable engine paths
`crates/xtask/src/checks/pure_rust.rs` — gate: forbids non-Rust / forbidden dependency shapes
`crates/xtask/src/checks/unchecked_arith.rs` — gate: forbids unchecked arithmetic in engine code
`crates/xtask/src/checks/unsafe_check.rs` — gate: audits or forbids unsafe in ruled regions
`crates/xtask/src/fsutil.rs` — filesystem helpers for walking and rewriting the tree in gates
`crates/xtask/src/gate.rs` — orchestrates the full cargo xtask gate (merge witness, not Plan DoD)
`crates/xtask/src/main.rs` — xtask binary entry: dispatches gate verbs
`crates/xtask/src/proc.rs` — spawns and collects subprocesses for gate commands
`crates/xtask/src/resonance.rs` — resonance runner that keeps mechanical checks in agreement
`crates/xtask/src/synutil.rs` — syn-based AST helpers for source gates
`crates/xtask/src/verbs.rs` — named xtask verbs the operator/CI invoke
`fuzz/fuzz_targets/compare_prefixed_slice.rs` — fuzzes prefixed-slice compare under the one law
`fuzz/fuzz_targets/data_block.rs` — fuzzes data-block encode/decode
`fuzz/fuzz_targets/fact_payload_decode.rs` — fuzzes fact payload decode for refusals and no-panic
`fuzz/fuzz_targets/index_block.rs` — fuzzes index-block encode/decode
`fuzz/fuzz_targets/kyzoscript_parser.rs` — fuzzes KyzoScript parse for refusals and no-panic
`fuzz/fuzz_targets/memcmp_codec.rs` — fuzzes memcmp codec against semantic order
`fuzz/fuzz_targets/table_read.rs` — fuzzes table read paths over crafted bytes

`crates/kyzo-model/src/value/json_convert.rs` — DataValue ↔ JSON conversion vocabulary
