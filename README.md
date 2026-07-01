<img src="static/logo_k.png" width="200" height="175" alt="KyzoDB logo">

[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue)](LICENSE.txt)

# KyzoDB

KyzoDB is a fork of [CozoDB](https://github.com/cozodb/cozo) by Ziyang Hu and the Cozo Project Authors,
carried forward after upstream development went quiet. The original design and codebase are theirs, with
thanks; KyzoDB continues from that foundation, and its MPL-2.0 license and every copyright notice are
preserved in full.

Most systems assemble retrieval out of separate parts: a relational store for facts, a graph database
for relationships, a vector index for similarity, a search engine for text, and something extra for
history. Each part comes with its own query language, its own copy of the data, and the standing job of
keeping that copy in sync with the others. KyzoDB makes the opposite choice. It holds relational, graph,
and vector data, together with full-text search, MinHash-LSH near-duplicate detection, and as-of time
travel, in a single embeddable engine behind one declarative language. A vector search is a join. A
graph traversal is recursion. A read at a past instant is a query parameter. They compose because
underneath they are the same thing: relations over one ordered, transactional key-value store.

That is an operational simplification before it is anything else. One store in place of five is one
query language, one transaction and consistency model, and no synchronization pipeline for copies to
drift out of. Retrieval that used to span several systems becomes a single query against a single source
of truth.

The same coherence is what knowledge-heavy and agent-facing workloads have started to require: exact
facts, the relationships among them, semantic and lexical recall, awareness of duplicates and versions,
and the ability to ask what was true at a given moment, all consistent with one another. KyzoDB serves
those from one governed model rather than a fleet of separately synchronized systems.

### Table of contents

1. [Introduction](#introduction)
2. [Retrieval in one query](#retrieval-in-one-query)
3. [Query examples](#query-examples)
4. [Using KyzoDB](#using-kyzodb)
5. [Architecture](#architecture)
6. [Status](#status)
7. [Links](#links)
8. [License](#license)

## Introduction

KyzoDB is a transactional, relational database. It queries with **Datalog** through a dialect called
**KyzoScript**, runs **embedded** in your process (and client-server when you want it), and treats
**graph**, **vector**, **full-text**, **near-duplicate**, and **time-travel** retrieval as ordinary
operations over the same relations. The sections here explain the foundations;
[Retrieval in one query](#retrieval-in-one-query) shows the retrieval paths working together.

### What does _embeddable_ mean here?

A database is almost surely embedded if you can use it on a phone which _never_ connects to any network
(this situation is not as unusual as you might think). SQLite is embedded. MySQL/Postgres/Oracle are
client-server.

> A database is _embedded_ if it runs in the same process as your main program. This is in
> contradistinction to _client-server_ databases, where your program connects to a database server
> (maybe on a separate machine) via a client library. Embedded databases generally require no setup and
> can be used in a much wider range of environments.
>
> KyzoDB is _embeddable_ rather than _embedded_: you can also run it in client-server mode, which makes
> better use of server resources and allows much more concurrency than embedded mode.

### Why _graphs_?

Because data are inherently interconnected. Most insights about data can only be obtained if you take
this interconnectedness into account.

> Most graph databases require you to shoehorn your data into the labelled-property graph model. KyzoDB
> doesn't: the traditional relational model is easier to work with for storing data, more versatile, and
> handles graph data just fine. More importantly, the most piercing insights usually come from graph
> structures _implicit_ several levels deep in your data. The relational model, being an _algebra_, can
> deal with that; the property graph model, not so much, since it is not very composable.

### Why _Datalog_?

Datalog can express all _relational_ queries. _Recursion_ in Datalog is much easier to express, more
powerful, and usually faster than in SQL, and Datalog is extremely composable: you build queries piece
by piece.

> Recursion is especially important for graph queries. KyzoScript supercharges it by allowing recursion
> through a safe subset of aggregations, and by providing efficient built-in algorithms (such as
> PageRank) for the recursions common in graph analysis. The _rules_ of Datalog are like functions in a
> programming language: composable, and decomposing a query into rules makes it clearer and more
> maintainable with no loss in efficiency, unlike the monolithic SQL `select-from-where` in nested forms.

### Time travel

Time travel means tracking changes to data over time and allowing queries to be logically executed at a
point in time, to get a historical view of the data.

> In a sense this makes your database _immutable_, since nothing is really deleted. In KyzoDB time travel
> is not automatic for all data: you decide, per relation, whether you want it, because every extra
> capability has a cost you should not pay if you do not use it.

## Retrieval in one query

The retrieval paths a knowledge system usually spreads across separate services are ordinary relations
here, so they combine in a single query.

Take documents that carry a title and a vector embedding, plus a relation recording which document cites
which:

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
`cites`, one query does semantic recall and then follows the relationships of whatever it finds:

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

answers `~doc:lsh{id | query: 'Graph databases', k: 5}`. In each case the search result is a relation you
can join, filter, negate, and recurse over, so hybrid retrieval is a query rather than a pipeline.

## Query examples

For a taste of the graph and recursion side, here `*route` is a relation with two columns `fr` and `to`
representing a route between two airports, and `FRA` is Frankfurt Airport.

How many airports are reachable from `FRA` by any number of stops?

```
reachable[to] := *route{fr: 'FRA', to}
reachable[to] := reachable[stop], *route{fr: stop, to}
?[count_unique(to)] := reachable[to]
```

| count_unique(to) |
|------------------|
| 3462             |

The shortest path between `FRA` and `YPO` by actual distance travelled:

```
start[] <- [['FRA']]
end[] <- [['YPO']]
?[src, dst, distance, path] <~ ShortestPathDijkstra(*route[], start[], end[])
```

| src | dst | distance | path                                                      |
|-----|-----|----------|-----------------------------------------------------------|
| FRA | YPO | 4544.0   | `["FRA","YUL","YVO","YKQ","YMO","YFA","ZKE","YAT","YPO"]` |

## Using KyzoDB

KyzoDB is a Rust workspace, built on stable Rust, pure Rust with no C or C++ toolchain for the engine
and server.

```
git clone https://github.com/kyzodb/kyzo
cd kyzo
cargo build -p kyzo --release
cargo test  -p kyzo --release
```

To depend on it from another Rust project:

```
kyzo = { git = "https://github.com/kyzodb/kyzo", package = "kyzo" }
```

Language bindings (Python, Node, C, Java, Swift, WASM) are being ported from the fork base and published
under KyzoDB; see the [issues](https://github.com/kyzodb/kyzo/issues) for progress.

## Architecture

KyzoDB has three layers, each calling only into the one below: a language/environment wrapper, the query
engine, and the storage engine.

**Storage engine.** A `Storage` trait defines an ordered key-value store with range scans; `fjall`, a
pure-Rust LSM store, implements it. Keys use a
[memcomparable encoding](https://github.com/facebook/mysql-5.6/wiki/MyRocks-record-format#memcomparable-format),
so rows stored as binary blobs sort lexicographically into the correct order. That single invariant is
what lets one ordered key-value store serve relational scans, graph traversals, vector and text index
lookups, and as-of time travel uniformly.

**Query engine.** Holds most of the code: function/aggregation/algorithm definitions, schema,
transactions, and query compilation and execution. KyzoScript is compiled to relational algebra and
evaluated with semi-naive, stratified, magic-set Datalog. Rust programs use the library API directly.

**Wrapper.** For every language except Rust, a thin FFI layer translates the Rust API into the target
runtime (a C ABI, or pyo3, jni, neon, swift-bridge, wasm-bindgen).

## Status

KyzoDB is early, assembled from its CozoDB base as a pure-Rust engine: the core is built on `fjall`, a
pure-Rust key-value backend, and does not carry the base's RocksDB (C++) or SQLite (C); the project and
its language bindings are branded `kyzo`. The full plan is in [REFACTOR.md](REFACTOR.md), and the work is
tracked story by story in the [issues](https://github.com/kyzodb/kyzo/issues) and on the org project board.

As a pre-1.0 project under active development, expect churn: there is no promise yet of syntax/API
stability or storage compatibility.

## Links

* [Repository](https://github.com/kyzodb/kyzo)
* [Issues and board](https://github.com/kyzodb/kyzo/issues)
* [REFACTOR.md](REFACTOR.md) (the plan)

## License

KyzoDB is licensed under [**MPL-2.0**](LICENSE.txt). It is a fork of CozoDB by Ziyang Hu and the Cozo
Project Authors; every license header and copyright notice from that work is preserved, and fixes
incorporated from other contributors keep their original authorship. Contributions are welcome via the
[issue tracker](https://github.com/kyzodb/kyzo/issues) and pull requests.
