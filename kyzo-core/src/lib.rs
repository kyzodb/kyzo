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
//! - [`Validity`] / [`ValidityTs`] — a time-stamped existence claim, ordered
//!   newest-first; retraction is a first-class assertion of absence.
//! - [`EncodedKey`] — a fact's written form: relation prefix, memcomparable
//!   tuple bytes, fixed-width validity tail. Constructed only by encoders, so
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
//!   mandatory, not per-relation opt-in: the validity tail lets an as-of scan
//!   seek to the newest version at or before a query time.
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
//! ## Runtime — the execution substances (`runtime`)
//!
//! The typed catalog (`runtime::relation`, whose `SystemKey` is the closed
//! set of system-keyspace shapes, so no third shape appears by accident) and
//! the semi-naive delta stores (`runtime::temp_store`, whose `Admitted` count
//! is the budget's deterministic unit of account). The `db.rs` entrypoint —
//! `run_query`, sessions, cooperative cancellation — is the one tier not yet
//! landed (see the boundaries below).
//!
//! ## Fixed rules and full-text (`fixed_rule`, `fts`)
//!
//! A fixed rule is an opaque, stratum-bounded computation (the built-in graph
//! algorithms and utilities) that consumes whole input relations and fills a
//! `FixedRuleOutput` branded with its declared arity — a lying arity is
//! refused at the first wrong row, not fed as mis-shaped tuples into
//! downstream joins. Long-running algorithms poll a `CancelFlag` — the single
//! cooperative kill point for outside cancellation; the kill-switch and
//! budget-deadline wiring that pulls it arrives with the unlanded session
//! tier — and draw any randomness from a seeded PRNG, so the same
//! facts and query answer identically run to run. Full-text search resolves a
//! `TokenizerConfig` (pure data in an index manifest) into a runnable
//! analyzer — validated at definition time so a bad config never reaches the
//! manifest, and re-checked fallibly at use time because stored data is never
//! trusted to be well-formed just because it was once written.
//!
//! # The enforcement ladder: compiler > constructor > test
//!
//! Every law is pushed as far up this ladder as it will go; an invariant held
//! by a type costs nothing to maintain and cannot be forgotten.
//!
//! - **Compiler.** Zero `unsafe` is a build guarantee here, not a convention
//!   (`#![forbid(unsafe_code)]`, below). A [`ReadTx`] cannot write because the
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
//! # Honest boundaries: complete, refusing, not yet here
//!
//! The **kernel is complete and load-bearing**: `data` and `storage` are the
//! substrate everything else stands on. The **engine tiers exist, compile,
//! and are tested in-crate** (parse, query, runtime, fixed_rule, fts), but
//! they are `pub(crate)` and land bottom-up in dependency order (story #3).
//! The public surface today is the kernel — the re-exports below — and there
//! is **no public query entrypoint yet**: `runtime/db.rs` (`run_query`,
//! sessions) has not landed, so in the non-`test` build the engine tiers are
//! legitimately dead code, and the `expect(dead_code)` attributes below fire
//! — forcing their removal — as each consumer lands. What refuses *today*,
//! typed and (where it has a source location) spanned: malformed query text
//! (parser), unstratifiable programs (stratifier), budget exhaustion,
//! fixed-rule arity mismatch, format-version mismatch, and transaction
//! conflict. No claim here is aspirational; every type and law named above
//! exists as named in the tree.

// Zero `unsafe` is a compiler guarantee in this crate, not a convention;
// CI checks that this attribute stays.
#![forbid(unsafe_code)]
// The transaction traits return boxed iterator types by design; naming them
// would not simplify the contract.
#![allow(clippy::type_complexity)]
// `DataValue` is used as a set/map key throughout (e.g. `DataValue::Set`);
// clippy flags it as a "mutable key type" through false-positive interior-
// mutability detection in its field types. Keys are never mutated via shared
// references.
#![allow(clippy::mutable_key_type)]

pub(crate) mod data;
// The fixed-rule tier's consumer (the runtime evaluator, which drives
// `run` and merges the output stores) lands later. Same dead-code posture
// as `parse`: partially exercised by in-file tests, dead in the lib build.
#[allow(dead_code)]
pub(crate) mod fixed_rule;
// The fts tier's consumers (the operator tier: fts/indexing.rs and
// runtime/db.rs) land later. Same dead-code posture as `parse`: fully dead
// in the lib build, partially exercised by in-file tests.
#[cfg_attr(not(test), expect(dead_code))]
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod fts;
// The parse tier's consumers (runtime/db.rs) land later. In the lib build
// the whole module is dead (expect); in test builds the in-file tests
// exercise the parsers but not every AST field a runtime consumer reads,
// so a plain `allow` covers the remainder until the runtime tier lands.
#[allow(dead_code)]
pub(crate) mod parse;
pub(crate) mod query;
pub(crate) mod runtime;
pub(crate) mod storage;

pub use data::tuple::{EncodedKey, Tuple, encode_tuple_key};
pub use data::value::{
    DataValue, JsonData, Num, RegexWrapper, UuidWrapper, Validity, ValidityTs, VecElementType,
    Vector, current_validity,
};
pub use storage::backup::{dump_storage, restore_storage};
pub use storage::fjall::{
    FjallReadTx, FjallStorage, FjallWriteTx, StorageOptions, StorageStats, new_fjall_storage,
    new_fjall_storage_with,
};
pub use storage::retry::retry_on_conflict;
pub use storage::verify::{CorruptEntry, VerifyReport, verify_storage};
pub use storage::{ConflictError, FormatVersion, ReadTx, Storage, WriteTx};

pub use fixed_rule::{
    CancelFlag, FixedRule, FixedRuleInputRelation, FixedRulePayload, NamedRows, SimpleFixedRule,
};
pub use runtime::callback::{CallbackEvent, CallbackOp};
pub use runtime::db::{Db, ScriptOptions};

// A curated, opaque façade over the crate-internal query pipeline
// (compile → bind → semi-naive eval), for the RA-layer criterion benches.
// Gated behind the `bench-internals` feature so it never touches the normal
// public surface. Everything it exposes is an opaque handle or a primitive;
// no crate-internal type crosses the boundary.
#[cfg(feature = "bench-internals")]
pub mod bench_api;
