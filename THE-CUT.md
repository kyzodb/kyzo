# THE CUT

Two jobs: get the target-state ontology right, then script the cut and run it.

## Ontology

We already earned the zone one-liners. This pass refines them from what we have learned since and from what storage will demand — not from old maps.

Seat every construct by the kind of truth it is. Prefer a split into a context the future will clearly own over parking two truths under one name because they can cohabit today. Bag filenames that mean two things are the enemy of the foundation.

Maps, census rows, and prior seating tables are helpers. They are not trusted. Reconcile them against the live tree, the zone laws, and the storage architecture already ruled. Observation, induction, deduction — then name the seat. Pattern-matching a stale doc into a pretty layout is failure.

## The script

Once the seating table is ruled, encode it and execute in one hard cut. Bulk apply is the default: faster and safer than construct-by-construct fear. Red after the cut is signal — the diff and the table locate the break; fix from there. Do not optimize for a perfect first compile. Optimize for a complete, interrogable cut from a true ontology.

## Files to Kill

Paths that must not exist when the cut is 100% done.
Deduced from deprecated-construct-map non-`-` sources (directories expanded
against the live tree), retired deletes, and condemned zone contents.
Intentional map preserves (`tests/*`, host `mod.rs`/`lib.rs`, same-path noops)
are omitted. Closure: every path below is gone.

200 paths.

## Remaining kill paths by map type

120 paths from Files to Kill still present in the working tree.
Typed only by deprecated-construct-map sections: absorbed / migrated / retired / sealed / split.

| type | count |
| --- | --- |
| migrated | 94 |
| retired | 12 |
| split | 8 |
| absorbed | 4 |
| sealed | 2 |

- `crates/kyzo-bin/src/client.rs`
- `crates/kyzo-bin/src/relations.rs`
- `crates/kyzo-bin/src/repl/output.rs`
- `crates/kyzo-bin/src/server/changes.rs`
- `crates/kyzo-bin/src/server/pages.rs`
- `crates/kyzo-bin/src/server/standing.rs`
- `crates/kyzo-core/examples/bench_tc.rs`
- `crates/kyzo-core/examples/bulk_ingest_profile.rs`
- `crates/kyzo-core/examples/determinism_digest.rs`
- `crates/kyzo-core/examples/fixpoint_mem_profile.rs`
- `crates/kyzo-core/examples/hnsw_build_profile.rs`
- `crates/kyzo-core/examples/lsm_keyspace_policy_bench.rs`
- `crates/kyzo-core/examples/oltp_mixed_profile.rs`
- `crates/kyzo-core/examples/pointsto_repro.rs`
- `crates/kyzo-core/examples/ra_determinism.rs`
- `crates/kyzo-core/examples/ra_profile.rs`
- `crates/kyzo-core/examples/standing_smoke.rs`
- `crates/kyzo-core/examples/tc_regress.rs`
- `crates/kyzo-core/src/bench_api.rs`
- `crates/kyzo-core/src/capacity.rs`
- `crates/kyzo-core/src/data/aggr.rs`
- `crates/kyzo-core/src/data/arrow_ipc.rs`
- `crates/kyzo-core/src/data/bitemporal.rs`
- `crates/kyzo-core/src/data/expr.rs`
- `crates/kyzo-core/src/data/functions.rs`
- `crates/kyzo-core/src/data/json.rs`
- `crates/kyzo-core/src/data/mod.rs`
- `crates/kyzo-core/src/data/program.rs`
- `crates/kyzo-core/src/data/relation.rs`
- `crates/kyzo-core/src/data/sketch/aggr.rs`
- `crates/kyzo-core/src/data/sketch/count_min.rs`
- `crates/kyzo-core/src/data/sketch/hll.rs`
- `crates/kyzo-core/src/data/sketch/mod.rs`
- `crates/kyzo-core/src/data/sketch/tdigest.rs`
- `crates/kyzo-core/src/data/span.rs`
- `crates/kyzo-core/src/data/symb.rs`
- `crates/kyzo-core/src/data/tests/exprs.rs`
- `crates/kyzo-core/src/data/tests/functions.rs`
- `crates/kyzo-core/src/data/tests/mod.rs`
- `crates/kyzo-core/src/data/value/admission.rs`
- `crates/kyzo-core/src/data/value/arena.rs`
- `crates/kyzo-core/src/data/value/arity.rs`
- `crates/kyzo-core/src/data/value/bytes_qty.rs`
- `crates/kyzo-core/src/data/value/canonical.rs`
- `crates/kyzo-core/src/data/value/cell.rs`
- `crates/kyzo-core/src/data/value/code.rs`
- `crates/kyzo-core/src/data/value/column.rs`
- `crates/kyzo-core/src/data/value/exec.rs`
- `crates/kyzo-core/src/data/value/mod.rs`
- `crates/kyzo-core/src/data/value/number.rs`
- `crates/kyzo-core/src/data/value/prefix.rs`
- `crates/kyzo-core/src/data/value/proofs.rs`
- `crates/kyzo-core/src/data/value/row.rs`
- `crates/kyzo-core/src/data/value/search_hits.rs`
- `crates/kyzo-core/src/data/value/string.rs`
- `crates/kyzo-core/src/data/value/tag.rs`
- `crates/kyzo-core/src/data/value/wide/collection.rs`
- `crates/kyzo-core/src/data/value/wide/interval.rs`
- `crates/kyzo-core/src/data/value/wide/json.rs`
- `crates/kyzo-core/src/data/value/wide/mod.rs`
- `crates/kyzo-core/src/data/value/wide/regex.rs`
- `crates/kyzo-core/src/data/value/wide/uuid.rs`
- `crates/kyzo-core/src/data/value/wide/validity.rs`
- `crates/kyzo-core/src/data/value/wide/vector.rs`
- `crates/kyzo-core/src/engines/fts.rs`
- `crates/kyzo-core/src/engines/gazetteer.rs`
- `crates/kyzo-core/src/engines/gazetteer_hostile.rs`
- `crates/kyzo-core/src/engines/hnsw.rs`
- `crates/kyzo-core/src/engines/hnsw_filter_harness.rs`
- `crates/kyzo-core/src/engines/lsh.rs`
- `crates/kyzo-core/src/engines/mod.rs`
- `crates/kyzo-core/src/engines/projection.rs`
- `crates/kyzo-core/src/engines/segments.rs`
- `crates/kyzo-core/src/engines/sparse.rs`
- `crates/kyzo-core/src/engines/sparse_hostile.rs`
- `crates/kyzo-core/src/engines/spatial.rs`
- `crates/kyzo-core/src/engines/text/README.md`
- `crates/kyzo-core/src/engines/text/ast.rs`
- `crates/kyzo-core/src/engines/text/cangjie/mod.rs`
- `crates/kyzo-core/src/engines/text/cangjie/options.rs`
- `crates/kyzo-core/src/engines/text/cangjie/stream.rs`
- `crates/kyzo-core/src/engines/text/cangjie/tokenizer.rs`
- `crates/kyzo-core/src/engines/text/mod.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/alphanum_only.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/ascii_folding_filter.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/empty_tokenizer.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/lower_caser.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/mod.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/ngram_tokenizer.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/raw_tokenizer.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/remove_long.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/simple_tokenizer.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/split_compound_words.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/stemmer.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/stop_word_filter/gen_stopwords.py`
- `crates/kyzo-core/src/engines/text/tokenizer/stop_word_filter/mod.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/stop_word_filter/stopwords.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/tokenized_string.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/tokenizer_impl.rs`
- `crates/kyzo-core/src/engines/text/tokenizer/whitespace_tokenizer.rs`
- `crates/kyzo-core/src/fixed_rule/algos/all_pairs_shortest_path.rs`
- `crates/kyzo-core/src/fixed_rule/algos/astar.rs`
- `crates/kyzo-core/src/fixed_rule/algos/bfs.rs`
- `crates/kyzo-core/src/fixed_rule/algos/degree_centrality.rs`
- `crates/kyzo-core/src/fixed_rule/algos/dfs.rs`
- `crates/kyzo-core/src/fixed_rule/algos/k_core.rs`
- `crates/kyzo-core/src/fixed_rule/algos/kruskal.rs`
- `crates/kyzo-core/src/fixed_rule/algos/label_propagation.rs`
- `crates/kyzo-core/src/fixed_rule/algos/louvain.rs`
- `crates/kyzo-core/src/fixed_rule/algos/max_flow.rs`
- `crates/kyzo-core/src/fixed_rule/algos/maximal_cliques.rs`
- `crates/kyzo-core/src/fixed_rule/algos/mod.rs`
- `crates/kyzo-core/src/fixed_rule/algos/pagerank.rs`
- `crates/kyzo-core/src/fixed_rule/algos/prim.rs`
- `crates/kyzo-core/src/fixed_rule/algos/random_walk.rs`
- `crates/kyzo-core/src/fixed_rule/algos/shortest_path_bfs.rs`
- `crates/kyzo-core/src/fixed_rule/algos/shortest_path_dijkstra.rs`
- `crates/kyzo-core/src/fixed_rule/algos/strongly_connected_components.rs`
- `crates/kyzo-core/src/fixed_rule/algos/top_sort.rs`
- `crates/kyzo-core/src/fixed_rule/algos/triangles.rs`
- `crates/kyzo-core/src/fixed_rule/algos/yen.rs`
- `crates/kyzo-core/src/fixed_rule/graph.rs`
- `crates/kyzo-core/src/fixed_rule/mod.rs`
- `crates/kyzo-core/src/fixed_rule/parallel.rs`
- `crates/kyzo-core/src/fixed_rule/rng.rs`
- `crates/kyzo-core/src/fixed_rule/utilities/constant.rs`
- `crates/kyzo-core/src/fixed_rule/utilities/csv.rs`
- `crates/kyzo-core/src/fixed_rule/utilities/jlines.rs`
- `crates/kyzo-core/src/fixed_rule/utilities/mod.rs`
- `crates/kyzo-core/src/fixed_rule/utilities/reorder_sort.rs`
- `crates/kyzo-core/src/format.rs`
- `crates/kyzo-core/src/format/tests.rs`
- `crates/kyzo-core/src/fuzz_api.rs`
- `crates/kyzo-core/src/jepsen_trials.rs`
- `crates/kyzo-core/src/kyzoscript.pest`
- `crates/kyzo-core/src/parse/expr.rs`
- `crates/kyzo-core/src/parse/fts.rs`
- `crates/kyzo-core/src/parse/fuzz_tests.rs`
- `crates/kyzo-core/src/parse/imperative.rs`
- `crates/kyzo-core/src/parse/mod.rs`
- `crates/kyzo-core/src/parse/query.rs`
- `crates/kyzo-core/src/parse/schema.rs`
- `crates/kyzo-core/src/parse/sys.rs`
- `crates/kyzo-core/src/query/batch.rs`
- `crates/kyzo-core/src/query/batch_ops.rs`
- `crates/kyzo-core/src/query/compile.rs`
- `crates/kyzo-core/src/query/dst_query.rs`
- `crates/kyzo-core/src/query/eval.rs`
- `crates/kyzo-core/src/query/gauntlet.rs`
- `crates/kyzo-core/src/query/graph.rs`
- `crates/kyzo-core/src/query/incremental.rs`
- `crates/kyzo-core/src/query/laws.rs`
- `crates/kyzo-core/src/query/levels.rs`
- `crates/kyzo-core/src/query/magic.rs`
- `crates/kyzo-core/src/query/mod.rs`
- `crates/kyzo-core/src/query/normalize.rs`
- `crates/kyzo-core/src/query/provenance.rs`
- `crates/kyzo-core/src/query/ra/fixed.rs`
- `crates/kyzo-core/src/query/ra/join.rs`
- `crates/kyzo-core/src/query/ra/mod.rs`
- `crates/kyzo-core/src/query/ra/neg.rs`
- `crates/kyzo-core/src/query/ra/search.rs`
- `crates/kyzo-core/src/query/ra/stored.rs`
- `crates/kyzo-core/src/query/ra/temp.rs`
- `crates/kyzo-core/src/query/ra/temporal.rs`
- `crates/kyzo-core/src/query/ra/transform.rs`
- `crates/kyzo-core/src/query/search.rs`
- `crates/kyzo-core/src/query/semiring.rs`
- `crates/kyzo-core/src/query/sort.rs`
- `crates/kyzo-core/src/query/standing.rs`
- `crates/kyzo-core/src/query/stratify.rs`
- `crates/kyzo-core/src/query/temp_store.rs`
- `crates/kyzo-core/src/query/time_travel_script_laws.rs`
- `crates/kyzo-core/src/query/time_travel_trials.rs`
- `crates/kyzo-core/src/query/trials.rs`
- `crates/kyzo-core/src/query/vm.rs`
- `crates/kyzo-core/src/runtime/callback.rs`
- `crates/kyzo-core/src/runtime/constraint.rs`
- `crates/kyzo-core/src/runtime/db.rs`
- `crates/kyzo-core/src/runtime/db_battery.rs`
- `crates/kyzo-core/src/runtime/generation.rs`
- `crates/kyzo-core/src/runtime/json.rs`
- `crates/kyzo-core/src/runtime/mod.rs`
- `crates/kyzo-core/src/runtime/mutate.rs`
- `crates/kyzo-core/src/runtime/pinned_handle.hex`
- `crates/kyzo-core/src/runtime/relation.rs`
- `crates/kyzo-core/src/runtime/verify.rs`
- `crates/kyzo-core/src/storage/backup.rs`
- `crates/kyzo-core/src/storage/conformance.rs`
- `crates/kyzo-core/src/storage/crash_matrix.rs`
- `crates/kyzo-core/src/storage/fjall.rs`
- `crates/kyzo-core/src/storage/merkle.rs`
- `crates/kyzo-core/src/storage/mod.rs`
- `crates/kyzo-core/src/storage/retry.rs`
- `crates/kyzo-core/src/storage/sim.rs`
- `crates/kyzo-core/src/storage/skip_walk.rs`
- `crates/kyzo-core/src/storage/temp.rs`
- `crates/kyzo-core/src/storage/tests.rs`
- `crates/kyzo-core/src/storage/verify.rs`
- `crates/kyzo-core/src/typestate.rs`
