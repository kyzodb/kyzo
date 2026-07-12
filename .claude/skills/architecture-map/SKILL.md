---
name: architecture-map
description: The ideal target architecture of KyzoDB — the crate and module structure nature dictates for a max-purity, type-driven, deterministic, proof-carrying database engine. Use whenever placing a new construct, creating or naming a file, deciding which zone owns work, writing a story that touches structure, or judging whether code sits in the wrong place. This is the placement authority; per-construct coding law lives in the rules.
---

# The Architecture Map

This is the target state, derived from what the engine IS — not from where code
happens to sit today. The current tree is a migration in progress toward this
map. When they disagree, this map is right and the tree is work.

## The Fundamental Principles

These produced this structure and govern every placement decision:

1. **Structure is the authority graph.** Every zone rules exactly one kind of
   truth. The tree order is the dependency order: a zone consumes only zones
   above it, never below, never sideways. A construct's home is decided by what
   kind of truth it is — never by which feature wanted it, never by where the
   edit was convenient.
2. **One authority per truth.** If two files can answer the same question, one
   of them is a bug in the architecture. A second implementation of any meaning
   (a value's bytes, an expression's result, a batch's shape) is either the
   judge's deliberate naive twin, or a defect.
3. **The judge shares vocabulary, never machinery.** Verification code must
   agree with the engine about what a value and a query *are* (the model), and
   must share nothing about *how* answers are computed. This wall is a crate
   boundary so the compiler enforces it — independence is physics here, not
   review discipline.
4. **Projections never own truth.** Every index, residency structure, and
   acceleration is rebuildable from canonical facts and points back at them.
   Anything in the projection zone that canonical storage cannot regenerate is
   misplaced authority.
5. **Features are laws, not directories.** Determinism, time, provenance, and
   purity have no folder. They enter as obligations on every zone: time is a
   value kind in the model, a key law in the store, operators in exec.
   A directory named after a feature is the first symptom of a second authority.
6. **Zones are stable; files grow.** New capability = new files inside existing
   zones. Splitting a fat file at a concept boundary is normal life. A NEW zone
   requires a new kind of truth and is an operator ruling, never a drive-by.
7. **Names are meaning.** A file is named for the concept it defines (`tag.rs`
   defines `Tag`); a zone is named for the truth it rules. `util`, `helper`,
   `common`, `misc` are banned — a construct that fits no name has no clear
   concept, and that is a design smell to resolve, not to file away.
8. **Small files, true names.** More files with fewer lines beats fewer files
   with more, *provided the split follows a concept boundary*. A wrong split —
   one concept under two names — is worse than a fat file, because every future
   edit touches half a truth.

## Placement Procedure

Before writing a construct, answer in order:

1. **What kind of truth is it?** → picks the crate and zone (see map).
2. **Who must agree on it?** Engine *and* judges → `kyzo-model`. Engine only →
   `kyzo-core`. A verdict about the engine → `kyzo-oracle`/`kyzo-trials`.
   A way to *reach* the engine → a host crate.
3. **Does it mint, consume, or judge?** Minting code lives in the truth's own
   zone with private constructors; consuming code imports the type and cannot
   forge it; judging code lives across the crate wall.
4. **Name it for what it is.** If no domain noun fits, the concept is not yet
   understood — stop and model, don't file.

Every capability lands with its judge: a new operator, value kind, or engine
gets its naive twin in `kyzo-oracle` and (when it carries a public claim) a
campaign hook in `kyzo-trials` in the same story.

## The fabric boundary — the guarantee line (NATS)

A construct is **ours** when a Kyzo guarantee flows through it — **meaning** (what a record
or answer *is*), **determinism** (byte-identical, replayable, verifiable), or **accountability**
(provenance, authority, refusal). A construct is **not ours** when all it needs is a commodity
infrastructure guarantee — delivery, durability, ordered movement, or zero-trust identity —
because the NATS fabric already makes those better than we could, and there its overhead is
dominated by disk/network physics anyway.

You cannot outsource a guarantee to a party that does not make it, and you should not rebuild
one already made better. The line is therefore derived, not chosen, and it puts almost all of
the engine on our side. What crosses to the fabric:

- **Change-feed / standing-query DELIVERY** — subscription, fan-out, backpressure, durable
  resume cursors. The engine keeps *what an event is* and snapshot-consistency and emits the
  typed record-event log; the fabric carries and delivers it.
- **Replication / failover** — JetStream replay + engine determinism = provably-equal replicas.
  We never write a replicator; the store keeps only the portable dump FORMAT, not a shipping
  mechanism.
- **Post-commit external notification** — the trigger *semantics* stay; the external delivery is
  a publish-on-commit to the fabric.
- **Blob/bundle movement, inter-node RPC, network identity, tenancy** — object-store transport,
  request/reply, NKeys/JWT/accounts. Semantic authorization stays ours; network trust is the fabric's.

What NEVER crosses: store commit/WAL and the in-memory hot paths (determinism is produced there),
exec/ evaluation and planning, rules/, project/ indexes, record semantics and verification,
provenance and authority interpretation, deterministic refusal. The fabric adapters live OUTSIDE
these crates (`kyzo-net-core` / `kyzo-net-nats` and the product crates); the engine gains only a
record-event *emit* seam, never fabric machinery.

## The Target Tree

```
crates/kyzo-model/                      # THE SHARED VOCABULARY — what engine, judges, and hosts
│                                #   must all agree on before any execution exists.
│                                #   Pure data + boundary lifts. No IO, no evaluation,
│                                #   no storage. Everything downstream depends on this;
│                                #   this depends on nothing of ours.
├── src/
│   ├── lib.rs                   # the vocabulary façade: what a value, schema, and program ARE
│   ├── value/                   # the value plane: identity, order, and bytes of every datum
│   │   ├── tag.rs               # Tag: the one type-discriminant and cross-type order authority
│   │   ├── canonical.rs         # the one order-preserving byte form — interning key IS the disk key
│   │   ├── cell.rs              # Value: the 16-byte tagged cell, inline or arena-backed
│   │   ├── number.rs            # Num: the unified sortable int/float space
│   │   ├── string.rs            # the inline-or-heap string realization of the cell
│   │   ├── prefix.rs            # the one prefix-first comparison doctrine
│   │   ├── proofs.rs            # compile-time ABSENCE proofs: no raw doors, no forged identity
│   │   └── kind/                # wide value faces — each kind's identity law before its bytes
│   │       ├── collection.rs    # List/Set: the collection faces of the canonical sequence
│   │       ├── json.rs          # Json: canonical JSON semantics, never syntax
│   │       ├── uuid.rs          # Uuid: sixteen raw bytes, fixed width
│   │       ├── regex.rs         # Regex: textual identity under one execution contract
│   │       ├── vector.rs        # Vector: dimensionality + canonical element identity
│   │       ├── interval.rs      # Interval: a pure value over valid-time microseconds
│   │       └── validity.rs      # Validity: the time-axis coordinate vocabulary
│   ├── schema/                  # what a stored relation promises about its rows
│   │   ├── relation.rs          # relation schema: key/value column split, typed columns
│   │   └── column.rs            # column types, nullability, defaults
│   ├── program/                 # what a query IS at each tier, as pure data
│   │   ├── symbol.rs            # names and namespaces
│   │   ├── span.rs              # source location: where in the text a thing came from
│   │   ├── expr.rs              # Expr: expression SEMANTICS as types — defined once here,
│   │   │                        #   evaluated in exactly two places: exec/ and the oracle
│   │   ├── rule.rs              # rules, atoms, heads, bodies — the datalog program model
│   │   ├── aggregate.rs         # what an aggregation IS (its meaning; folds live in exec)
│   │   └── query.rs             # the whole input program: rules + options + params
│   ├── parse/                   # claimed text becomes proven program (the one boundary lift)
│   │   ├── grammar.pest         # the KyzoScript grammar — advertises nothing unowned
│   │   ├── script.rs            # scripts and imperative chaining
│   │   ├── query.rs             # rules, options, and the proofs that bind them
│   │   ├── expr.rs              # the Pratt expression parser
│   │   ├── schema.rs            # schema clause parsing
│   │   ├── sys.rs               # the :: system-operation surface
│   │   └── search.rs            # the index-search and FTS mini-language
│   ├── format.rs                # the canonical formatter: program -> one source text, idempotent
│   └── envelope/                # portable wire meanings of the vocabulary
│       ├── json.rs              # DataValue <-> JSON, rows <-> the JSON envelope
│       └── arrow.rs             # the dependency-free Arrow IPC stream encoding
│
crates/kyzo-core/                       # THE ENGINE — everything that computes and persists truth.
│                                #   Depends on kyzo-model and kyzo-oracle (for the verify
│                                #   door), never on trials or hosts.
├── src/
│   ├── lib.rs                   # the sealed public contract: the one Db façade
│   ├── store/                   # FACT PERSISTENCE — the one substrate and its laws
│   │   ├── contract.rs          # the storage contract: ordered scans, SSI, consuming commits
│   │   ├── fjall.rs             # the one backend: the owned pure-Rust LSM fork
│   │   ├── tx.rs                # transactions: snapshot isolation, typed conflicts
│   │   ├── retry.rs             # conflict-retry: the liveness half of optimistic concurrency
│   │   ├── time.rs              # the bitemporal key law: one fact key, a two-axis question
│   │   ├── skip_walk.rs         # the ONE bitemporal skip-scan walk, generic over its driver
│   │   ├── scratch.rs           # the session's temporary store species
│   │   ├── backup.rs            # portable dump/load FORMAT (ours); replication is NATS replay, never a replicator
│   │   ├── verify_walk.rs       # the whole-store invariant walk
│   │   └── merkle.rs            # the deterministic state root over the ordered keyspace
│   │                            #   (placed here: it is a property OF the store; diff/merge
│   │                            #    capabilities consume it from above)
│   ├── exec/                    # DERIVED TRUTH — one machine that turns programs into answers
│   │   ├── currency/            # the execution currency: the ONE hot-loop form
│   │   │   ├── arena.rs         # the order-preserving interning dictionary
│   │   │   ├── code.rs          # Code: the dense interned handle — hot-path identity
│   │   │   ├── row.rs           # interned tuple rows; cell views only at boundaries
│   │   │   ├── column.rs        # code columns and native-typed arrays
│   │   │   └── admitted.rs      # admitted rows under a proven domain — unforgeable
│   │   ├── plan/                # program -> executable plan
│   │   │   ├── compile.rs       # the plan compiler
│   │   │   ├── stratify.rs      # the stratification proof: negation and aggregation are safe
│   │   │   ├── magic.rs         # the magic-sets demand transform
│   │   │   └── graph.rs         # rule-dependency analysis (SCC, levels)
│   │   ├── op/                  # relational-algebra operators: the executable rule body
│   │   │   ├── join.rs          # column joins and storage-probe joins
│   │   │   ├── neg.rs           # anti-join
│   │   │   ├── stored.rs        # canonical scans, current and time-travel
│   │   │   ├── delta.rs         # fixpoint total/delta scans
│   │   │   ├── search.rs        # projection searches as relations
│   │   │   ├── temporal.rs      # interval derivation and net-diff operators
│   │   │   ├── transform.rs     # streaming transforms: reorder, filter, project
│   │   │   └── literal.rs       # unit and literal-block relations
│   │   ├── expr/                # the ONE production expression evaluator (columnar)
│   │   │   └── eval.rs          # kernel-per-expression over code columns
│   │   ├── stdlib/              # the scalar standard library, one domain per file
│   │   │   ├── math.rs          # arithmetic and numeric functions
│   │   │   ├── text.rs          # string, regex, fuzzy, and phonetic scalars
│   │   │   ├── collection.rs    # list/set/json functions
│   │   │   ├── time.rs          # temporal scalars and coordinate functions
│   │   │   └── geo.rs           # spatial scalars (haversine and kin)
│   │   ├── fold/                # aggregation execution
│   │   │   ├── aggr.rs          # grouped and global folds
│   │   │   └── sketch/          # deterministic sketches as folds
│   │   │       ├── hll.rs       # HyperLogLog: distinct-count, union is a semilattice
│   │   │       ├── count_min.rs # Count-Min: frequency, merge is a monoid
│   │   │       └── tdigest.rs   # t-digest: quantiles by a sorted deterministic fold
│   │   ├── fixpoint/            # the semi-naive stratified fixpoint
│   │   │   ├── eval.rs          # the loop: recursion over admitted currency
│   │   │   ├── delta_store.rs   # working memory keyed on packed-code identity
│   │   │   └── parallel.rs      # deterministic sharded parallelism: byte-identical 1..N
│   │   ├── provenance/          # derivations that explain themselves
│   │   │   ├── semiring.rs      # annotation algebra: the idempotent pair + certificates
│   │   │   ├── counted.rs       # the non-idempotent tier on its own fixpoint
│   │   │   └── witness.rs       # proof-tree extraction and the explain rendering seams
│   │   └── sort.rs              # result ordering, limits, offsets
│   ├── rules/                   # ALGORITHMS AS RULES — the invocable library
│   │   ├── contract.rs          # what a fixed rule promises: determinism, seeded randomness
│   │   ├── graph_view.rs        # the graph the algorithms run on (backed by projections)
│   │   ├── rng.rs               # the seed-reproducible randomness every stochastic rule uses
│   │   ├── algo/                # one algorithm per file, named for the algorithm
│   │   │   └── …                # bfs, dfs, dijkstra, astar, yen, pagerank, louvain,
│   │   │                        #   label_propagation, scc, k_core, max_flow, cliques,
│   │   │                        #   centralities, mst, random_walk, top_sort, triangles, …
│   │   └── io/                  # utility rules: constant data, csv and json-lines readers
│   ├── project/                 # PROJECTIONS — rebuildable speed, never truth
│   │   ├── contract.rs          # the projection law: rebuildable, canonical-pointing,
│   │   │                        #   maintained on commit, searched through exec/op/search
│   │   ├── residency.rs         # the rebuild/validity discipline (generations, invalidation)
│   │   ├── vector/              # dense proximity: graph index, quantized search, filtering
│   │   ├── text/                # full text: inverted index, analyzers, tokenizers (owned)
│   │   ├── sparse/              # sparse-vector inverted lists
│   │   ├── dedup/               # MinHash-LSH near-duplicate signatures
│   │   ├── spatial/             # the space-filling-curve access path
│   │   ├── graph/               # the resident canonical CSR for traversal
│   │   └── current.rs           # current-state segments over bitemporal bases
│   ├── react/                   # DERIVED TRUTH KEPT CURRENT — the continuous lifecycle
│   │   ├── incremental.rs       # IVM: maintained views provably equal to recompute
│   │   ├── standing.rs          # standing queries: compute + snapshot-consistency ours; subscriber DELIVERY → NATS
│   │   └── feed.rs              # change feeds: emit the ordered record-event log (ours); fan-out/DELIVERY → NATS
│   ├── session/                 # THE ONE DOOR — everything between a caller and the truth
│   │   ├── db.rs                # the entrypoint: script string to result rows
│   │   ├── json.rs              # the one JSON door over the envelope vocabulary
│   │   ├── admit.rs             # the write admission path: mutation enters here only
│   │   ├── constraint.rs        # integrity as denial rules with witnesses, gating admission
│   │   ├── catalog.rs           # the store's knowledge of its relations; coherent multi-row moves
│   │   ├── access.rs            # per-relation protection tiers
│   │   ├── observe.rs           # relation triggers ours; post-commit external NOTIFICATION delivery → NATS
│   │   ├── jobs.rs              # running-query listing and kill
│   │   ├── ops.rs               # operator surface: compaction, maintenance
│   │   └── verify.rs            # the ::verify door: the engine summons its judge (kyzo-oracle)
│   └── benches/                 # permanent performance instrumentation (bench-internals feature)
│
crates/kyzo-oracle/                     # THE REFERENCE SEMANTICS — the engine's naive twin.
│                                #   Depends ONLY on kyzo-model. Deliberately slow, small
│                                #   enough to hostile-review line by line. Optimizing it
│                                #   is a defect. The crate wall makes independence physics.
├── src/
│   ├── lib.rs                   # the judge's contract: same question, independent answer
│   ├── eval.rs                  # naive stratified datalog over plain values
│   ├── expr.rs                  # the naive expression evaluator — the oracle's OWN, complete
│   ├── temporal.rs              # naive as-of and interval semantics
│   ├── provenance.rs            # reference annotations: independent support and cost
│   └── checker.rs               # the proof-tree checker: re-derives witnesses from scratch
│
crates/kyzo-trials/                     # THE CAMPAIGNS — attacks on public claims, rerunnable by
│                                #   strangers. Depends on kyzo-core's public surface,
│                                #   kyzo-oracle, and kyzo-crashfs. Nothing depends on it.
├── src/
│   ├── gauntlet.rs              # metamorphic logic-bug hunting over generated programs
│   ├── differential.rs          # whole-corpus engine-vs-oracle equality
│   ├── determinism.rs           # byte-identical replay across runs, threads, hardware
│   ├── serializability.rs       # elle/Adya-style transaction anomaly detection
│   ├── crash.rs                 # the crash matrix over real and fault-injected filesystems
│   ├── dst.rs                   # deterministic simulation: storage seam and query path
│   ├── conformance.rs           # the storage-contract kit any backend must pass (public)
│   ├── fuzz.rs                  # generative fuzzing drivers and the ledger's corpus
│   └── time_travel.rs           # the temporal law and trial batteries
│
crates/kyzo-crashfs/                    # THE FAULT INJECTOR — a standalone instrument whose nature
│                                #   dictates its three parts: a plan, an application, a mount
├── src/
│   ├── fault.rs                 # the fault plan: every decision a pure function of the seed
│   ├── passthrough.rs           # the filesystem that applies the plan to a backing dir
│   └── harness.rs               # mount lifecycle: capability detection, setup, teardown
│
crates/kyzo-bin/                        # THE NATIVE HOST — an entrypoint, enumerable doors, and
│                                #   rendering. Every door derives from the sealed contract;
│                                #   a module the contract does not entail has no seat here.
├── src/
│   ├── main.rs                  # process entry: typed config lifted from args/env (a boundary
│   │                            #   lift — malformed config is a typed refusal, not a panic)
│   ├── bulk.rs                  # the one bulk-movement codec (export/import), shared by both doors
│   ├── repl/                    # THE INTERACTIVE DOOR — a human speaking the contract
│   │   ├── editor.rs            # line editing and continuation over the language's shape
│   │   ├── commands.rs          # the % session commands, each an enumerated deliberate surface
│   │   ├── render.rs            # envelope -> human: tables and readable errors, adding no meaning
│   │   └── fetch.rs             # the %import egress: fetching a remote source to lift at the boundary
│   └── server/                  # THE NETWORK DOOR — programs speaking the contract over HTTP
│       ├── auth.rs              # the gate every route passes; the route table is the attack surface
│       ├── query.rs             # execute: script + params in, envelope out
│       ├── bulk.rs              # move: export/import over the shared codec
│       ├── feeds.rs             # subscribe: shrinks — delivery is NATS/JetStream; keeps only guarantee-preserving shape
│       ├── rules.rs             # extend: downstream-computed fixed rules bridged to the engine
│       └── console.rs           # inspect: the static human console page
│
crates/kyzo-wasm/                       # (reserved) THE RUNTIME ENVELOPE — the real engine in foreign
│                                #   hosts. Its nature dictates three parts and no more:
├── src/
│   ├── request.rs               # the typed request/response/error envelope (the one shape)
│   ├── boundary.rs              # the panic boundary: an engine panic crosses ONLY as the typed error
│   └── host.rs                  # host glue: deterministic inputs in, serialized results out,
│                                #   byte-identical to native by standing proof
│
crates/kyzo-lsp/                        # THE EDITOR HOST — a protocol adapter over model's parse tier
├── src/
│   ├── main.rs                  # the LSP protocol loop
│   └── translate.rs             # parse refusals -> diagnostics verbatim; the canonical formatter
│                                #   surfaced; nothing invented editor-side
│
vendor/fjall, vendor/lsm-tree    # the OWNED storage fork — ours to rule, licenses preserved
```

## The Tooling Layer (entailed by the rules, not by habit)

Enforcement machinery exists only as the mechanical rules entail it — every
guard traces to a rule, and a script no rule entails is unowned machinery.

```
justfile                         # the named-commands authority: gate, test, bench, run
Dockerfile / docker-compose.yml  # the pinned environment: the container IS the limits;
rust-toolchain.toml / Cargo.lock #   no C compiler, so impurity fails to build
scripts/
├── check-unsafe.sh              # entailed by the unsafe law: forbid present, zero allows,
│                                #   no doc claims a nonexistent exception
├── check-pure-rust.sh           # entailed by the purity law: no C/C++ in first-party trees
├── authority-graph.py           # entailed by the authority law: @authority extraction,
│                                #   drift audit, the ratchet against the committed baseline
├── check-structure (to exist)   # entailed by the placement law: zone dependency direction
│                                #   and module-doc coverage, mechanically
└── smell-scan.sh                # the candidate-finder for the judgment bucket: it finds,
                                 #   a human/LLM classifies — deliberately not a gate
authority/                       # the committed ratchet artifacts (map + report)
ci/                              # the remote mirror of the gate: pure scripts, no agent,
                                 #   depending on neither compliance nor maintainer
crates/xtask/                           # guards that need Rust to express (workspace-level checks);
                                 #   same law as scripts/: each traces to a rule or has no seat
```

## Where Tests Live (the test ontology)

- **Unit and hostile tests of internals**: beside the code they test, in the
  module's test submodule — they may see internals because they live inside
  the authority they test.
- **The naive twin and proof checkers**: `kyzo-oracle` — a separate crate so
  sharing machinery with the engine is impossible, not just forbidden.
- **Campaigns against public claims**: `kyzo-trials` — everything a stranger
  should be able to rerun.
- **Public-API batteries**: `crates/kyzo-core/tests/` — the end-to-end surface proofs.
- **Performance instrumentation**: `crates/kyzo-core/benches/` and the kyzo-bench
  proving ground. Issue-pinned reproducers die with their issues — the tree
  keeps no museum.

## Reserved Growth (named now so placement is never invented later)

- `exec/op/` gains operators as capabilities land (rank fusion, window functions,
  path-query compilation, geometry predicates) — operators, never new engines.
- `project/` gains a directory per genuinely new projection kind; posting-lattice
  work lands inside `text/`/`sparse/`, not beside them.
- `session/` gains doors only by operator ruling — the door count is the attack
  surface.
- The product tier (records, memory, retrieval, federation, encryption, topology)
  is downstream of the sealed contract and lives OUTSIDE these crates; it never
  reaches into the engine.
- A new top-level zone or crate requires a new kind of truth and an operator
  ruling. There is no other way one appears.
