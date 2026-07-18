/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! KyzoDB's engine: a deterministic knowledge primitive.
//!
//! One declarative language (KyzoScript, a Datalog) over relational, graph,
//! vector, and full-text data, on one memcomparable transactional key-value
//! substrate. The telos is narrow and absolute: turn meaning into bytes and
//! back **without loss of truth**. Every answer is provably entailed by the
//! stored facts; every refusal is typed, and a refusal born of source text
//! points at the span that caused it (conflict, budget, kill, and
//! format-version refusals have no source text and carry none); the same
//! facts, query, and budget yield identical answers *and
//! identical refusals* at any thread count. The query authors of the next
//! decade are language models — brilliant, adversarial, unbounded — so the
//! engine hands them contracts, not hopes: clever execution is invisible,
//! meaning is exact, and nothing user-reachable can panic the process.
//!
//! # The world model is the type graph
//!
//! Every type is a claim about what exists in this domain, and its
//! constructors are the only ways that thing can come to be — one name per
//! concept, one concept per name. The crate is layered kernel-outward; each
//! tier names the proofs it owns. (The kernel types are re-exported below and
//! linked; the engine tiers are `pub(crate)` — named here in prose, see the
//! boundaries section for why.)
//!
//! ## Kernel — data and storage, the load-bearing substrate
//!
//! - [`DataValue`] — the atom of meaning: thirteen kinds whose *declaration
//!   order is the cross-type order*. [`Tuple`] is a fact's body, an ordered
//!   sequence of them.
//! - [`Validity`] / [`ValidityTs`] — the time coordinate of an existence
//!   claim, ordered newest-first. Every fact is **bitemporal**: a valid
//!   instant (when the fact holds in the world) and a system version (when
//!   the store learned it) ride in every key, and the claim's polarity —
//!   assert, retract, erase (`data::bitemporal::ClaimPolarity`) — rides in
//!   the value, so one valid instant has exactly one system lineage and
//!   retraction is a first-class assertion of absence.
//! - [`StorageKey`] — a fact's written form: relation prefix, memcomparable
//!   tuple bytes, and a fixed-width bitemporal tail (valid instant outer,
//!   system version inner); the value side is FormatVersion 3's
//!   self-describing tagged fields (`data::fact_payload`). Constructed only by encoders, so
//!   possession proves provenance; bytes read back from disk are *claimed*
//!   keys until fallible decoding proves them. The law beneath the whole
//!   store: encoded byte order equals semantic value order (`data::memcmp`),
//!   so one ordered keyspace serves relational, graph, vector, and text
//!   access paths alike.
//! - [`FormatVersion`] — the identity of the on-disk encoding, stamped into
//!   every store and dump; a mismatch refuses to open rather than read
//!   garbage.
//! - [`Storage`] — a universe of facts, handing out transactions of two
//!   species. A [`ReadTx`] is one consistent snapshot and *cannot* write: the
//!   mutating operations do not exist on it. A [`WriteTx`] adds a
//!   conflict-tracked write set, and [`WriteTx::commit`] *consumes* it — a
//!   committed-but-alive transaction is not a state to guard against but a
//!   value that no longer exists. Isolation is SSI over both reads and writes
//!   — commit validates the write set too, first committer wins; a lost race
//!   is the typed, retryable [`ConflictError`],
//!   and [`retry_on_conflict`] is its liveness half. Both traits are sealed:
//!   one backend by decree (`fjall`, a pure-Rust LSM). Time travel is
//!   mandatory, not per-relation opt-in: the bitemporal tail lets an as-of
//!   scan seek to the newest system version at or before an `AsOf`
//!   coordinate. Stamp minting is snapshot-then-mint by construction (the
//!   mint takes the open snapshot as an argument), which is the proof that
//!   reads-from order agrees with stamp order.
//! - [`VerifyReport`] — the store's integrity, made inspectable.
//!
//! ## Parse — claimed text becomes proven syntax (`parse`)
//!
//! Below this tier's boundary a program is a *claim* (`&str`); above it,
//! *proof*: a `Script` whose every value carries the source span of the text
//! it came from, so any later stage can point a diagnostic at the exact
//! characters responsible. One grammar, three script species — a Datalog
//! query (`InputProgram`), an imperative block, or one system op (`SysOp`).
//! Grammar shape is consumed through typed accessors, so grammar/consumer
//! drift is a spanned `GrammarShapeError`, never an abort; adversarial input
//! cannot panic, hang, or overflow the native stack (`NestingTooDeep`,
//! refused by a structural scan before any recursive work).
//!
//! ## Query — the answer the logic says, whatever the plan (`query`)
//!
//! The Datalog engine is a **typestate pipeline**: each stage's output type
//! is the proof its checks passed, so an unstratifiable or entryless program
//! never becomes an evaluable value. `InputProgram` (the entry `?` proven
//! present, as a field) → `NormalFormProgram` (bodies flat and deduplicated)
//! → `StratifiedNormalFormProgram` (strata in execution order, entry proven
//! to sit in the last) → `StratifiedMagicProgram` (demand-rewritten, entry
//! proven to survive unadorned) → compiled relational algebra (`query::ra`,
//! where `NegRight` is the constructor proof that negation's illegal right
//! sides are unreachable) → semi-naive fixpoint evaluation (`query::eval`).
//! Execution is **vectorized**: rows flow as batches (`query::batch_ops`)
//! through the columnar expression evaluator (`query::vm`), law-bound
//! observationally identical — values, presence, and error identity — to
//! scalar `Expr` evaluation; the connectives `&&`/`||`/`~` short-circuit as
//! `Expr::Lazy`, and no strict boolean ops exist. Fixpoint state lives as
//! level-merged immutable sorted runs (`query::levels`): each epoch's
//! accumulator seals as the newest level — the delta IS the newest level —
//! and compaction is a logarithmic pure function of level sizes.
//! The tier's seven laws and where each is enforced are documented in
//! `query/mod.rs`; the load-bearing ones: optimized evaluation equals the
//! naive fixpoint, unstratifiable programs are *refused* rather than
//! mis-answered, recursion terminates, and no query text or stored bytes can
//! panic the process. Evaluation is governed by a `Budget` — required by
//! parameter, because no unbounded fixpoint exists (the epoch ceiling is
//! mandatory) — whose deterministic dimensions are checked only at the
//! sequential merge barrier, which is what makes the determinism law hold
//! under any parallel schedule. `EvalRuleSet` carries a head's aggregation
//! signature and a meet head folds *inside* recursion via `MeetLayout`.
//! Provenance rides the `AdmissionSink` seam: with recording on, the *first*
//! derivation of each admitted tuple is witnessed at the barrier in canonical
//! order; the off-state is the `()` sink, compiled away.
//!
//! [`Db::register_standing`] answers the same query LIVE: the compiled
//! `StratifiedMagicProgram` (the `?` proven present, negation's meaning
//! still `Symbol`-level, not yet the column-index/`NegJoin` form RA
//! lowering produces) translates into a small maintained-IVM program, and
//! [`StandingQuery::apply_pending`] keeps its answer correct across every
//! commit by finding affected candidates (or, for an aggregating head,
//! affected GROUPS) and re-deriving each directly from current state —
//! never a per-tuple or per-kind signed delta formula, which does not
//! exist in general (retracting the current `min`/`max` is the reason).
//!
//! ## Runtime — the session tier (`runtime`)
//!
//! Everything between a caller and the engine organs: the [`Db`] entrypoint
//! (`run_query`, sessions, cooperative cancellation, commit retry with
//! backoff), the mutation tier (`runtime::mutate` — puts, retractions at
//! bitemporal coordinates, index creation with resumable backfill), the
//! typed catalog (`runtime::relation`, whose `SystemKey` is the closed set
//! of system-keyspace shapes, so no third shape appears by accident),
//! transaction-scoped constraints, and change callbacks. The fixpoint
//! stores' `Admitted` count (`query::levels`) is the budget's deterministic
//! unit of account. [`Db::run_script_json`] (`runtime::json`) is the one
//! JSON-params-in, JSON-envelope-out surface every binding shares; it
//! composes `data::json`'s wire format ([`DataValue`] <-> JSON, `NamedRows`
//! <-> JSON, error reports -> JSON) rather than reimplementing it — a
//! binding adds transport, not JSON shaping.
//!
//! ## Engines — the derived-index organs (`engines`)
//!
//! HNSW vector search, full-text search, MinHash-LSH, spatial, sparse
//! vectors, and the gazetteer — each an index engine whose search surface
//! joins into query plans as a relation (`query::ra::search`), with the
//! text-analysis pipeline (`engines::text`) feeding the ones that read
//! prose. Each engine's laws are pinned by its own harness in-tree.
//!
//! ## Fixed rules (`fixed_rule`)
//!
//! A fixed rule is an opaque, stratum-bounded computation (the built-in graph
//! algorithms and utilities) that consumes whole input relations and fills a
//! `FixedRuleOutput` branded with its declared arity — a lying arity is
//! refused at the first wrong row, not fed as mis-shaped tuples into
//! downstream joins. Long-running algorithms poll a `CancelFlag` from the
//! cancel lifecycle (`CancelAuthority` → consuming `Cancelled`); the
//! session arms one authority shared with the budget interrupt path —
//! and draw any randomness from a seeded PRNG, so the same facts and
//! query answer identically run to run. (Full-text analysis lives with
//! the engines: a `TokenizerConfig` is pure data in an index manifest,
//! validated at definition time and re-checked fallibly at use time,
//! because stored data is never trusted to be well-formed just because
//! it was once written.)
//!
//! # The enforcement ladder: compiler > constructor > test
//!
//! Every law is pushed as far up this ladder as it will go; an invariant held
//! by a type costs nothing to maintain and cannot be forgotten.
//!
//! - **Compiler.** Zero `unsafe` is a build guarantee here, not a
//!   convention: the crate root declares `#![forbid(unsafe_code)]` (below),
//!   the maximum standard — no exception, not even a locally-liftable one.
//!   `GermanStr` is a SAFE wrapper over the 16-byte value cell; the
//!   16-byte layout is achieved entirely in safe Rust. `cargo xtask unsafe`
//!   enforces that this claim and the lint agree — zero `unsafe`, zero
//!   `allow(unsafe_code)`, anywhere in the crate. A [`ReadTx`] cannot write because the
//!   mutating methods are not on the trait; [`WriteTx::commit`] takes `self`,
//!   so commit-twice does not compile; the storage traits are sealed, so no
//!   foreign backend can weaken the contract.
//! - **Constructor** ("parse, don't validate"). `InputProgram::new` refuses a
//!   program with no entry; `EvalRuleSet::new` refuses an empty or
//!   aggregation-inconsistent rule set; `NegRight` refuses an illegal
//!   negation right side; `FixedRuleOutput` refuses a mis-width row;
//!   [`FormatVersion`] refuses a non-canonical stamp; `Budget::new` demands
//!   the epoch ceiling. Possession of the output type *is* the proof.
//! - **Test.** What cannot yet be a type is pinned by executable law: the
//!   memcmp round-trip and order-embedding property tests (`storage`), the naive
//!   reference oracle and the refusal corpus (`query::laws`), the determinism
//!   law, and the campaigns below.
//!
//! # Verification is architecture, not an afterthought
//!
//! - **The oracle** (`query::laws`, `cfg(test)` — judge, never production): a
//!   deliberately naive fixpoint evaluator, written to be obviously correct,
//!   that folds through the *real* landed aggregation ops. Optimized
//!   evaluation must produce byte-identical answer sets to it, and the oracle
//!   is itself cross-checked against a second evaluation strategy.
//! - **Differentials**: the optimized engine against the oracle, on generated
//!   programs including the meet-recursive fragment.
//! - **Deterministic simulation** (`storage::sim`, `cfg(test)`): a second
//!   [`Storage`] implementation whose thread interleavings, faults, crashes,
//!   and power cuts are a pure function of one `u64` seed — a failing campaign
//!   prints the seed that replays it exactly.
//! - **Fuzzing** (`parse::fuzz_tests`): a grammar-aware generator plus a
//!   mutation layer, because the caller is a fuzzer with intent; the parser
//!   never panics and every refusal names its span.
//! - **Mutation**: the suites prove their guarantees by surviving mutants,
//!   not merely by passing.
//!
//! # Honest boundaries: complete, refusing, still internal
//!
//! The **kernel is complete and load-bearing**: `data` and `storage` are the
//! substrate everything else stands on. The **engine is live end to end**:
//! the public surface is the kernel re-exports plus the [`Db`] session
//! entrypoint below; the tiers between (parse, query, engines, runtime,
//! fixed_rule) stay `pub(crate)` — internal organs, not API. `format`,
//! `parse`, and `fixed_rule` carry no module-level `allow(dead_code)` —
//! their host doors are real (`format_program` / `parse_script` /
//! `FixedRule::run`); any unused residual is a rustc warning, not a
//! blanket lie about a consumer that "lands later". The same P112 posture
//! now applies to `query`, `runtime`, `engines`, and the value plane's
//! production modules (`SearchHits::admit_decoded`, canonical encode/decode):
//! module-level `allow(dead_code)` is gone; unlanded surfaces are
//! `#[cfg(test)]`, wired, or left to warn honestly. What refuses
//! *today*, typed and (where it has a source location) spanned: malformed
//! query text (parser), unstratifiable programs (stratifier), budget
//! exhaustion, fixed-rule arity mismatch, format-version mismatch, and
//! transaction conflict. No claim here is aspirational; every type and law
//! named above exists as named in the tree.

// Zero `unsafe` is a compiler guarantee in this crate, not a convention;
// CI checks that this attribute stays. `forbid`, not `deny`: the strongest
// standard, which cannot be locally lifted by any `#[allow(unsafe_code)]`.
// The value plane — including `GermanStr`'s 16-byte layout — is pure safe
// Rust, so no exception exists. A future story that genuinely needs unsafe
// must lower this lint deliberately, at the narrowest scope, with a full
// safety case; until then, unsafe does not exist in this crate.
// `cargo xtask unsafe` enforces that this lint stays and that no
// `allow(unsafe_code)` appears anywhere in kyzo-core.
#![forbid(unsafe_code)]
// Joins the panic-lint rung: sealed discriminants are matched exhaustively
// (workspace `[workspace.lints.clippy] wildcard_enum_match_arm = "deny"`).
// No `allow(wildcard_enum_match_arm)` escapes — name the remaining variants
// or use `if let` / `let else` for single-variant gates.
#![deny(clippy::wildcard_enum_match_arm)]
// The transaction traits return boxed iterator types by design; naming them
// would not simplify the contract.
#![allow(clippy::type_complexity)]
// `DataValue` is used as a set/map key throughout (e.g. `DataValue::Set`);
// clippy flags it as a "mutable key type" through false-positive interior-
// mutability detection in its field types. Keys are never mutated via shared
// references.
#![allow(clippy::mutable_key_type)]

pub(crate) mod capacity;
pub(crate) mod data;
// Engines production host doors: `runtime/mutate.rs` (fts/hnsw/lsh create/drop),
// `query/search.rs` (`RelationIndexSearch::search_relation`),
// `engines::admit_relation_search_hits` → `SearchHits::admit_decoded`.
// Unlanded kind engines (gazetteer/sparse/spatial) are `#[cfg(test)]` until
// their `db.rs` surface lands. No module-level `allow(dead_code)` (P112).
pub(crate) mod engines;
// Formatter host doors: `format::format_program` /
// `format_program_with_comments` (P112). Exercised by the in-module
// property suite and by parse's comment-meaning guardrail; kyzo-lsp
// format-document will call the same doors. No module-level
// `allow(dead_code)` — unused helpers warn honestly until that call site.
pub(crate) mod format;
// Fixed-rule production consumer landed (`runtime/db.rs` →
// `SessionFixedRule` → `FixedRule::run`, plus `StoredInputSource` via
// `SessionView`). No module-level `allow(dead_code)` (P112); residual
// unused symbols warn rather than hide behind a blanket.
pub(crate) mod fixed_rule;
// Parse production consumer landed (`runtime/db.rs` via `parse_script`)
// for query and system genera. `Script::Imperative` is a typed refusal at
// execution (`ImperativeNotWired`); its AST is still constructed by the
// parser and exercised in-file — no module-level `allow(dead_code)` (P112).
pub(crate) mod parse;
// Query production host doors: `runtime/db.rs::compile_and_eval` (compile,
// magic, stratify, ra, eval, normalize, search, sort, batch_ops, vm) and
// `runtime/verify.rs` (`laws::naive_eval_at_budgeted`). No module-level
// `allow(dead_code)` (P112); unlanded oracle/eval surface warns honestly.
pub(crate) mod query;
// Runtime production host doors: `Db::run_script` / `compile_and_eval`,
// `mutate` index create/drop, `relation` catalog, `callback` notifications,
// and `constraint` enforcement at commit (P112). No module-level
// `allow(dead_code)` (P112).
pub(crate) mod runtime;
pub(crate) mod storage;
pub(crate) mod typestate;

// Trial (issue #34): single-node SSI serializability checker. Test-only,
// touches no engine source — consumes the public `Storage`/`Db` surface
// exactly as an outside caller would (see the module docs for scope).
#[cfg(test)]
mod jepsen_trials;

pub use data::json::JsonData;
pub use data::json::format_error_as_json;
pub use data::value::{
    Arity, AsOf, DataValue, Num, RegexSource, RelationId, StorageKey, Tuple, TupleKey, TupleT,
    UuidWrapper, Validity, ValidityTs, Vector, decode_tuple_from_key,
};
pub use storage::backup::{dump_storage, restore_storage};
pub use storage::fjall::{
    FjallReadTx, FjallStorage, FjallWriteTx, StorageOptions, StorageStats, new_fjall_storage,
    new_fjall_storage_with,
};
pub use storage::retry::retry_on_conflict;
pub use storage::verify::{CorruptEntry, VerifyReport, verify_storage};
pub use storage::{Aborted, CommitFailure, Committed, ConflictError, FormatVersion, ReadTx, Storage, WriteTx};

/// Build→seal→query projection machine (story #305). Public so compile-fail
/// proofs and later kind parameterizations share one crate-root door.
pub use engines::projection::{
    Generation, ProjectionBuilder, ProjectionKind, Sealed, Stale,
};

pub use fixed_rule::{
    CancelAuthority, CancelFlag, Cancelled, EmptyNamedRowsBody, FixedRule, FixedRuleInputRelation,
    FixedRulePayload, NamedRows, SimpleFixedRule, SimpleRuleBody,
};
pub use runtime::callback::{CallbackEvent, CallbackOp};
pub use runtime::db::{Db, ScriptOptions};
pub use runtime::verify::VerifyOutcome;

pub use query::ra::temporal::SignedFact;
pub use query::standing::StandingQuery;

// Sealed doors deleted (bench_api / fuzz_api / lsp_api). Tooling speaks the
// sealed contract or goes red — bespoke façades were contract debt.
