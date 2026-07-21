<p align="center">
  <img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/logo_k.png" width="160" alt="KyzoDB logo">
</p>

<h1 align="center">KyzoDB</h1>

<p align="center"><em>One language for similarity, structure, time, and proof —<br>
on one deterministic substrate where answers replay, explain, or refuse.</em></p>

<p align="center">
  <a href="https://github.com/kyzodb/kyzo/actions/workflows/ci.yml"><img src="https://github.com/kyzodb/kyzo/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/kyzodb/kyzo/actions/workflows/fuzz-nightly.yml"><img src="https://github.com/kyzodb/kyzo/actions/workflows/fuzz-nightly.yml/badge.svg?branch=main" alt="Fuzz (nightly deep run)"></a>
  <a href="https://github.com/kyzodb/kyzo/actions/workflows/codeql.yml"><img src="https://github.com/kyzodb/kyzo/actions/workflows/codeql.yml/badge.svg?branch=main" alt="CodeQL"></a>
  <a href="rust-toolchain.toml"><img src="https://img.shields.io/badge/rust-1.96.1%20pinned-2F7E52" alt="Rust 1.96.1, pinned"></a>
  <a href="LICENSE-MPL"><img src="https://img.shields.io/badge/license-MPL--2.0-2F7E52" alt="License: MPL-2.0"></a>
</p>

> [!NOTE]
> **Latest binary release: [v0.8.1](https://github.com/kyzodb/kyzo/releases/tag/v0.8.1).** Pre-1.0 by design:
> the public API is not frozen, and we do not publish yardstick latency/throughput until measured
> with methodology and losing runs ([VERSIONING.md](VERSIONING.md)). The
> [board](https://github.com/orgs/kyzodb/projects/1) is live status.

## Install

Linux x86_64:

```bash
curl -L https://github.com/kyzodb/kyzo/releases/download/v0.8.1/kyzo -o kyzo
chmod +x kyzo
./kyzo
```

Thirty seconds later you have a REPL. A join is shared variables — not `JOIN`:

<p align="center"><img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/repl_first_touch.svg" width="860" alt="KyzoDB REPL: create cites and runbook, join them, get rows."></p>

Or skip the typing and run the ops-world demo that seeds incidents, privilege edges, HNSW, claims,
and coverage, then asks the knowing question:

```bash
# with a release binary on PATH, or after: cargo build -p kyzo-bin --release
./examples/readme/demo.sh
```

Embed like SQLite — no server; a database is a file handle:

<p align="center"><img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/embed.svg" width="860" alt="Rust: Db::new + new_fjall_storage + run_script."></p>

Other targets: `cargo build -p kyzo-bin --release`.

## Why the usual stack fails

<p align="center"><img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/collapse.svg" width="820" alt="Five stores drift; KyzoDB collapses them to one language, one transaction."></p>

Keeping facts, vectors, graph, text, and history in sync is the second product nobody asked for.
KyzoDB collapses them to **one query, one transaction, one snapshot**.

## The question a stitched stack can’t ask

Vector DBs don’t join. Graph DBs don’t mean. Audit logs don’t query.
KyzoScript (Datalog) treats search hits as relations — so similarity, recursion, negation, and
privilege closure compose in **one program**.

<p align="center"><img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/repl_knowing.svg" width="860" alt="Near live prod unclaimed incidents with runbooks while attacker reach includes db-customers."></p>

Near this alert · live · prod · has a runbook · **no claim yet** · and the attacker can still reach
`db-customers`. That is retrieval as *knowing*, not a fan-out pipeline.

The same program shape also joins full-text hits the same way — hybrid retrieval is a join, not a
fusion microservice. See `examples/readme/demo.sh` for a runnable seed of this world.

## Time is a coordinate

Correct the record; as-of the incident date still returns what was believed then — a seek, not a
change-log archaeology project. Same ops memory: customer `C-77` was `trial` when the incident
fired, `enterprise` after the correction:

<p align="center"><img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/repl_timetravel.svg" width="860" alt="As-of reads: coverage trial on the incident date, enterprise today."></p>

## Filtered search that cannot come back empty

Anyone who has run a vector database knows the failure: fill `k`, then filter, watch the set go empty
at low selectivity. Here the filter is inside the search; `k` counts matches:

<p align="center"><img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/repl_filtered_contrast.svg" width="860" alt="Naive post-filter ANN empty; KyzoDB filtered HNSW returns min(k, matches)."></p>

## The engine keeps its word

Ask it to prove a recursive answer against its own oracle — or hit a budget and get a typed refusal.
Same facts and budget also produce **byte-identical** answers across thread counts:

<p align="center"><img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/repl_verify.svg" width="860" alt="::verify returns match when engine and reference oracle agree."></p>

<p align="center"><img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/repl_refusal.svg" width="860" alt="Typed budget refusal: eval::limit_exceeded."></p>

<p align="center"><img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/repl_determinism.svg" width="860" alt="Same reach query row hash under 1 and 32 Rayon threads."></p>

## Why you can believe that

KyzoDB ships **its own adversary**: a deliberately naive reference oracle that speaks the whole
language. Generated workloads are answered twice; the answers must match.

<p align="center"><img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/oracle.svg" width="760" alt="Optimized engine and naive oracle must agree on every generated program."></p>

- **Oracle** — stratified Datalog semantics as an executable, slow, obviously-correct evaluator.
- **`::verify`** — user surface: match, budgeted refusal, or a reproducible mismatch bundle.
- **Determinism** — seeded campaigns at multiple thread counts demand byte-identical answers and refusals.
- **Typed refusals** — wrong shape, exceeded budget, unsafe program → named error, never panic.
- **One law** — memcomparable keys: binary order equals semantic order, so every access path is a range scan on one substrate.

## Answers that show their work

When an agent must not get it wrong, a derived fact names the premises that entailed it — re-checked
by an independent checker that imports nothing from the evaluator:

<p align="center"><img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/repl_provenance.svg" width="860" alt="Provenance proof: must_clear derived from ground facts, checker Ok."></p>

## Architecture

<p align="center"><img src="https://raw.githubusercontent.com/kyzodb/kyzo/main/docs/assets/architecture.svg" width="560" alt="KyzoScript → relational algebra → relational/graph/HNSW/FTS/as-of → memcomparable → fjall."></p>

KyzoScript compiles to relational algebra and evaluates with semi-naive, stratified, magic-set
Datalog. Storage is [`fjall`](https://github.com/fjall-rs/fjall) behind a memcomparable encoding —
the invariant that lets relational, graph, vector, text, and time share one ordered store. Pure Rust
end to end: embedded, server, or browser — no C/C++ in the build.

**Not** a petabyte warehouse. **Not** a distributed OLTP cluster. KyzoDB is for one body of knowledge
that must answer as facts, graph, similarity, text, and history — consistently, accountably, in one
place.

## Status

Feature-complete for its scope and correctness-proven (serializable transactions, crash recovery,
oracle-verified query semantics, shipped `::verify`). Still pre-1.0: expect API churn; empty
scoreboards do not belong on this page. See [VERSIONING.md](VERSIONING.md). The
[board](https://github.com/orgs/kyzodb/projects/1) is live status.

## Origins

KyzoDB began as a fork of [CozoDB](https://github.com/cozodb/cozo) by Ziyang Hu and the Cozo Project
Authors. Full story and attribution: [FORK.md](FORK.md).

## Links

* [Repository](https://github.com/kyzodb/kyzo)
* [Releases](https://github.com/kyzodb/kyzo/releases)
* [Roadmap](https://github.com/orgs/kyzodb/projects/2)
* [Issues and board](https://github.com/kyzodb/kyzo/issues)
* [VERSIONING.md](VERSIONING.md) · [CONTRIBUTING.md](CONTRIBUTING.md) · [FORK.md](FORK.md)

## License

Multi-licensed; **[LICENSING.md](LICENSING.md)** is the authoritative map. Engine/hosts are
[MPL-2.0](LICENSE-MPL); agent tooling under `.claude/` is [BSL-1.1](LICENSE-BSL). See
[CONTRIBUTING.md](CONTRIBUTING.md).
