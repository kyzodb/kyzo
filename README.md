<img src="static/logo_k.png" width="200" height="175" alt="KyzoDB logo">

[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue)](LICENSE.txt)

# KyzoDB

🚧 **UNDER CONSTRUCTION: this README describes the target state of an in-flight rebuild. The
[board](https://github.com/orgs/kyzodb/projects/1) is the live status; [Status](#status) below says
what is proven today.** 🚧

**KyzoDB is an embeddable, transactional, pure-Rust database in which relational queries, graph
traversal, vector similarity, full-text search, near-duplicate detection, and point-in-time reads are
one query, in one language, against one store.**

Most stacks assemble retrieval out of separate parts: a relational store for facts, a graph database
for relationships, a vector index for similarity, a search engine for text, and something extra for
history. Each part brings its own query language, its own copy of the data, and the standing job of
keeping the copies in sync. KyzoDB collapses the assembly. A vector search is a join. A graph traversal
is recursion. A read at a past instant is a query parameter. They compose because underneath they are
the same thing: relations over one ordered, transactional key-value store.

> LLMs gave software the ability to think out loud. KyzoDB exists so that what such systems come to
> know can be held — exactly, durably, explainably, and identically every time it's asked for. Not the
> mind; the ground the mind stands on.

That is the design brief, and it demands more than feature coverage. It demands an engine whose answers
are reproducible, whose errors explain themselves, and whose derivations can show their work. The rest
of this document demonstrates what that looks like.

## Retrieval is one act

The retrieval paths a knowledge system usually spreads across five services are ordinary relations
here, so they combine in a single query.

Take documents that carry a title and a vector embedding, plus a relation recording which document
cites which:

```
?[id, title, emb] <- [
    ['graph-db',   'Graph databases',       [0.1,  0.9]],
    ['vec-search', 'Vector search',         [0.9,  0.1]],
    ['datalog',    'Datalog and recursion', [0.15, 0.85]],
    ['mvcc',       'Transactions and MVCC', [0.85, 0.2]]
]
:create doc {id: String => title: String, emb: <F32; 2>}

?[from, to] <- [['datalog', 'graph-db'], ['graph-db', 'mvcc'], ['vec-search', 'datalog']]
:create cites {from: String, to: String}
```

Index the embeddings with HNSW:

```
::hnsw create doc:emb {dim: 2, dtype: F32, fields: [emb], distance: L2, m: 50, ef_construction: 20}
```

A nearest-neighbour search binds with `~` and unifies like any other relation. Joined straight to
`cites`, one query performs semantic recall and then follows the relationships of whatever it finds:

```
?[hit, title, cited] := ~doc:emb{id: hit, title | query: q, k: 2, ef: 20, bind_distance: dist},
                        *cites{from: hit, to: cited},
                        q = vec([0.12, 0.88])
```

| hit      | title                  | cited    |
|----------|------------------------|----------|
| datalog  | Datalog and recursion  | graph-db |
| graph-db | Graph databases        | mvcc     |

Full-text and near-duplicate search take the same shape. A full-text index over the same titles:

```
::fts create doc:text {extractor: title, tokenizer: Simple, filters: [Lowercase, Stemmer('English'), Stopwords('en')]}
```

answers `~doc:text{id, title | query: 'graph', k: 5, bind_score: s}`. A MinHash-LSH index for
near-duplicates:

```
::lsh create doc:lsh {extractor: title, tokenizer: NGram, n_gram: 3, target_threshold: 0.5}
```

answers `~doc:lsh{id | query: 'Graph databases', k: 5}`. In every case the search result is a relation
you can join, filter, negate, and recurse over. Hybrid retrieval is a query, not a pipeline — there is
no fan-out layer, no re-ranking glue service, and no copy of your data waiting to drift.

## Recursion is native

The query language is Datalog — a dialect called **KyzoScript**. Datalog expresses everything
relational algebra can, and it makes recursion a first-class, composable construct rather than SQL's
bolted-on `WITH RECURSIVE`. Rules compose like functions: you build a query piece by piece, and
decomposition costs nothing.

Here `*route` is a relation of airport-to-airport routes, and `FRA` is Frankfurt. Every airport
reachable from Frankfurt, by any number of stops, is three lines:

```
reachable[to] := *route{fr: 'FRA', to}
reachable[to] := reachable[stop], *route{fr: stop, to}
?[count_unique(to)] := reachable[to]
```

| count_unique(to) |
|------------------|
| 3462             |

For the recursions that graph analysis reaches for constantly, the engine ships whole-graph algorithms
(PageRank, community detection, shortest paths, centralities, and more) as built-in rules over your
relations — no export to a graph runtime and back:

```
start[] <- [['FRA']]
end[] <- [['YPO']]
?[src, dst, distance, path] <~ ShortestPathDijkstra(*route[], start[], end[])
```

| src | dst | distance | path                                                      |
|-----|-----|----------|-----------------------------------------------------------|
| FRA | YPO | 4544.0   | `["FRA","YUL","YVO","YKQ","YMO","YFA","ZKE","YAT","YPO"]` |

And because vector and text search results are relations too, they feed these same recursions: a
similarity hit can seed a graph traversal in the query that found it.

## Time is a query parameter

Relations can opt in to history. For a relation with time travel enabled, writes never destroy: an
update supersedes, a deletion retracts, and the previous state remains addressable. Any query can then
be evaluated *as of* a past instant — what did we know on Tuesday? — as a parameter of the read, not an
archaeology project over change-data-capture logs.

The capability is per-relation because history has a cost, and you should only pay it where you want
it. Under the hood, validity is encoded in the storage key itself, so an as-of read is an ordinary
ordered scan — not a reconstruction.

## The engine keeps its word

These are the properties that separate a component you build on from a component you babysit. KyzoDB
treats them as capabilities and engineers them deliberately:

- **Determinism as a law.** The same facts, the same query, and the same execution budget produce
  identical answers — and identical refusals — on every run, at any thread count, on any machine. Not
  "usually": it is a stated invariant with a test suite whose job is to break it.
- **Refusals that explain themselves.** Where the query is wrong, the engine answers with a typed error
  naming the reason and pointing at the exact span of the script — never a panic, never a shrug. An
  error message is an interface, and increasingly its reader is a program.
- **Budgeted execution.** Evaluation runs under an explicit budget — derivation ceilings, deadlines —
  and exceeding it yields a typed, deterministic refusal rather than a runaway query or a silent kill.
- **Answers that show their work.** Provenance is being built into evaluation, not bolted on: a derived
  fact can name the rule and premises that entailed it, recursively down to stored ground facts, and
  the resulting proof is itself cheap to verify. "Why do you believe that" becomes a query.

## One substrate, no ballast

The architecture is three layers, each calling only into the one below.

**Storage.** A `Storage` trait defines an ordered key-value store with range scans, MVCC commit
semantics, and validity-in-key as-of reads. The implementation is [`fjall`](https://github.com/fjall-rs/fjall),
a pure-Rust LSM store. Rows are encoded with a
[memcomparable format](https://github.com/facebook/mysql-5.6/wiki/MyRocks-record-format#memcomparable-format):
binary blobs whose lexicographic order *is* their semantic order. That single invariant is why one dumb
ordered store can serve relational scans, graph traversals, vector and text index lookups, and time
travel uniformly — every access path above is just a range scan below.

**Query engine.** KyzoScript compiles to relational algebra and evaluates with semi-naive, stratified,
magic-set Datalog. Schema, transactions, functions, aggregations, algorithms, and the index operators
live here. Rust programs call this API directly.

**Wrappers.** Every other language gets a thin FFI layer over the Rust API: a C ABI, Python (pyo3),
Java (jni), Node (neon), Swift (swift-bridge), WASM (wasm-bindgen).

The whole engine and server build as **pure Rust — no C or C++ anywhere in the toolchain**. That is not
an aesthetic preference. It is one `cargo build` on any platform Rust supports, one compiler's memory
model, one supply chain to audit, no vendored C++ submodule breaking on next year's compiler, and
backups in a pure-Rust portable format. CI enforces it mechanically: a dependency that smuggles in a C
compiler fails the build.

## Proven, not promised

A database earns the right to hold what a system knows by being hostile to its own bugs. KyzoDB's
development runs on that discipline:

- **A differential oracle** — an independent, sealed implementation of the query semantics — judges the
  engine's answers on generated workloads, so correctness is checked against an adversary, not against
  the implementation's opinion of itself.
- **Mutation testing** proves the test suites bite: a guarantee whose tests survive deliberate sabotage
  of the code under test is not a guarantee.
- **Deterministic simulation testing** at the storage seam injects faults, spurious conflicts, and
  adversarial schedules under reproducible seeds, then replays any failure exactly.
- **Generative fuzzing** of the query language assumes a caller that is brilliant, adversarial, and
  unbounded — the engine must never panic, and every refusal must name its reason.
- Dozens of defects inherited from the fork base — including silent-wrong-answer bugs in recursive
  evaluation — have been found this way, fixed, and pinned with regression tests.

Performance numbers will be published the same way: with methodology, hardware, seeds, and the losing
runs, against the standard public yardsticks for each capability. Receipts, or it didn't happen.

## Using KyzoDB

KyzoDB is a Rust workspace on stable Rust.

```
git clone https://github.com/kyzodb/kyzo
cd kyzo
cargo build -p kyzo --release
cargo test  -p kyzo --release
```

To depend on it from a Rust project:

```
kyzo = { git = "https://github.com/kyzodb/kyzo", package = "kyzo" }
```

It runs embedded — in your process, like SQLite, no server and no setup — and client-server when you
want shared access and more concurrency. Language bindings (C, Python, Java, Node, Swift, WASM, with
Go, Clojure, and Android in separate repos) are being ported and published under KyzoDB; the
[issues](https://github.com/kyzodb/kyzo/issues) track each one.

## Status

KyzoDB is early and mid-rebuild, and this README describes the target the work is converging on —
capability by capability, story by story, each landing only after adversarial review. The storage
kernel (fjall backend, memcomparable encoding, pure-Rust backup, contract tests) is proven and green;
the engine is being stood up around it; the bindings follow. The plan of record is
[REFACTOR.md](REFACTOR.md), and the live state is always the
[board](https://github.com/kyzodb/kyzo/issues).

As a pre-1.0 project under active development, expect churn: no promise yet of syntax/API stability or
storage compatibility.

## Origins

KyzoDB began as a fork of [CozoDB](https://github.com/cozodb/cozo) by Ziyang Hu and the Cozo Project
Authors, whose design it gratefully builds on — the full story and attribution live in
[FORK.md](FORK.md).

## Links

* [Repository](https://github.com/kyzodb/kyzo)
* [Issues and board](https://github.com/kyzodb/kyzo/issues)
* [REFACTOR.md](REFACTOR.md) (the plan)
* [FORK.md](FORK.md) (origins and attribution)

## License

KyzoDB is licensed under [**MPL-2.0**](LICENSE.txt). Every license header and copyright notice from the
work it builds on is preserved, and incorporated contributor fixes keep their original authorship; see
[FORK.md](FORK.md) for the project's origins. Contributions are welcome via the
[issue tracker](https://github.com/kyzodb/kyzo/issues) and pull requests.
