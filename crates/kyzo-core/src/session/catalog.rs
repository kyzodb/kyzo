/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0), re-architected for the KyzoDB kernel:
 *
 * - The system catalog keyspace is typed: [`SystemKey`] replaces the three
 *   key-shape conventions the original kept under `RelationId::SYSTEM`.
 *   The original's `STORAGE_VERSION` row is GONE, merged into the kernel's
 *   [`FormatVersion`](crate::FormatVersion) — see [`SystemKey`] for the
 *   record of that merge.
 * - `RelationHandle` holds its indices BY REFERENCE ([`IndexRef`]) instead
 *   of the original's embedded `(handle, manifest)` copies; the index
 *   relation's own catalog row is the single authority.
 * - Every transaction call site is adapted from the original `StoreTx`
 *   trait to the kernel's [`ReadTx`]/[`WriteTx`] species. The original's
 *   `lock: bool` parameter on reads dies: the kernel is full SSI and
 *   conflict-tracks every read, so there is no unlocked read to ask for.
 * - The relation-id counter is a transactional read-modify-write under SSI
 *   instead of the original's process-wide `AtomicU64` (which leaked ids on
 *   abort and could not survive a second process).
 * - `destroy_relation` applies `del_range` inside the transaction instead
 *   of returning byte ranges for a deferred post-commit cleanup pass; an
 *   aborted transaction now rolls the destruction back.
 * - Law 5 (no panics on user input or stored bytes): all msgpack
 *   (de)serialization is fallible; relation-id exhaustion is a typed error
 *   ([`RelationIdSpaceExhausted`]); `choose_index` survives empty argument
 *   lists, zero-key relations, and stale mappers; encode paths check arity
 *   before slicing.
 * - The original's key-prefix splice helper is deleted: it was dead code
 *   (`#[allow(dead_code)]` upstream) and splicing a different relation id
 *   into an [`StorageKey`]'s bytes would launder unproven provenance into
 *   the typed key. If a real consumer appears it returns as an encoder.
 * - Fix on port / #301 T6: the original's column-by-column
 *   `ensure_compatible` chained the *input*'s key columns with the
 *   *stored* relation's own non-key columns, so an input's dependent
 *   columns were never type-checked (a self-comparison that is trivially
 *   compatible). Compatibility is now one whole-schema proving constructor
 *   ([`CompatibleInputSchema::prove`]) that checks the input's keys and
 *   non-keys against the stored schema, or refuses the whole schema.
 * - Fix on port: the original's `create_relation` wrote the relation's id
 *   bytes under `[Str name]` and immediately overwrote them with the
 *   serialized handle under the byte-identical key (a dead store), and
 *   checked name conflicts against the *opposite* store (temp names against
 *   the persistent store and vice versa). One key, one write, one check.
 * - `rename_relation` refuses to rename a relation with indices attached:
 *   index relations are cataloged as `{base}:{index}`, and the original
 *   renamed the base while leaving the index rows under the old name,
 *   stranding them (its own `remove_index` then looked them up under the
 *   new name and failed).
 */

//! The catalog: the store's knowledge of its own relations.
//!
//! Every stored relation is described by one row in the system keyspace
//! ([`RelationId::SYSTEM`]), keyed by a [`SystemKey`] and holding a
//! msgpack-serialized [`RelationHandle`]. A handle is a *decoded catalog
//! row* — knowledge, not authority: it tells the engine how to read and
//! write a relation's keyspace, but the store's bytes remain the truth, and
//! a handle is only ever as current as the snapshot it was read from.
//!
//! The catalog operations ([`create_relation`], [`get_relation`],
//! [`list_relations`], [`destroy_relation`], …) speak directly to the
//! kernel's transaction species. Concurrency is the kernel's SSI: two
//! transactions that race on the same catalog row (or on the id counter)
//! conflict at commit, and the engine's retry loop reruns the loser. There
//! are no catalog locks and no process-wide atomics.
//!
//! ## Seams (documented, not hidden)
//!
//! - **Temp relations** (`_`-prefixed names) live in the session's
//!   in-memory temp store, which lands with the evaluator's wiring. The
//!   functions here address the persistent store only; `SessionTx` (session
//!   tier) owns the routing decision, handing these same [`RelationHandle`]
//!   scan/write methods the temp store's transaction instead. Nothing here
//!   fakes a temp store; [`create_relation`] refuses temp names with a
//!   typed error until the router exists.
//! - **Triggers** and **constraints** persist sealed [`InputProgram`]
//!   substance on the catalog wire (msgpack). Admit doors
//!   ([`Trigger::parse`], [`ConstraintRef::parse`]) lift script text at
//!   create/set time; catalog decode never calls `parse_script` and admits
//!   each stored program only through [`InputProgram::new`].
//! - **Index manifests** (HNSW / FTS / LSH) are operator-tier substances
//!   that have landed: [`IndexKind::Hnsw`]/`Fts`/`Lsh` carry their real
//!   manifest payloads (`session/ops.rs`'s `::hnsw|fts|lsh create`
//!   populates them), which was the catalog wire-format change the
//!   pinned-bytes test below forces to be deliberate.
//! - **`as_named_rows`** (the original's handle-to-`NamedRows` view) lands
//!   with the session tier that owns `NamedRows`.
//! - **[`IndexPositionUse`]** and [`RelationHandle::choose_index`] live in
//!   `exec/plan/compile.rs` — the compile tier owns index selection. This
//!   catalog module must NOT import `IndexPositionUse` (avoids a
//!   session↔exec cycle).
//!
//! ## Wire format
//!
//! A catalog row's value is the handle serialized as msgpack **with struct
//! maps** (self-describing field names, resilient to field reordering).
//! This is an on-disk format: the round-trip and pinned-bytes tests below
//! are its executable law, and changing it is a migration conversation, not
//! a refactor.

use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;

use miette::{Diagnostic, Result, bail, ensure};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::store::time::ClaimPolarity;
use crate::data::json::NamedRows;
use kyzo_model::schema::{CompatibleInputSchema, RelationWriteShape, StoredRelationMetadata};
use crate::rules::contract::{DEFAULT_FIXED_RULES, FixedRule};
use kyzo_model::program::{InputProgram, InputRelationHandle};
use crate::parse::parse_script;
use crate::parse::sys::AccessLevel as ParseAccessLevel;
use crate::session::access::{AccessLevel, InsufficientAccessLevel, map_access_level};
use crate::session::db::{Engine, ScriptOptions, SessionTx, status_ok};
use crate::store::{ReadTx, Storage, WriteTx};
use kyzo_model::SourceSpan;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{
    AsOf, DataValue, StorageKey, MAX_VALIDITY_TS, RelationId, ScanBound, StoredValiditySlot, Tuple,
    TupleT, ValidityTs, decode_tuple_from_kv, encode_key_with_suffix, extend_tuple_from_v,
    scan_key_lower, scan_key_lower_projected, scan_key_upper, scan_key_upper_projected,
};

// ---------------------------------------------------------------------------
// Catalog capability (decisions.md §1) — interpreter, not a byte owner.
// ---------------------------------------------------------------------------

/// Sole interpretive schema capability: as-of schema evaluation and
/// admission-gating of schema-mutating Records.
///
/// Owns no bytes, no fsync path, no counters. Schema Records are Store
/// facts; Catalog is their interpreter. Sealed — no public common
/// super-type with Store or Engine.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    _sealed: (),
}

impl Catalog {
    /// Mint an interpretive Catalog capability. Does not open storage,
    /// allocate counters, or touch durable bytes.
    pub fn new() -> Self {
        Self { _sealed: () }
    }
}

// ---------------------------------------------------------------------------
// The system keyspace, typed.
// ---------------------------------------------------------------------------

/// A key in the system catalog keyspace (`RelationId::SYSTEM`).
///
/// The CozoDB original kept three key shapes in this keyspace by
/// convention: `[Null]` for the id counter, `[Null, "STORAGE_VERSION"]` for
/// a version byte, and `[Str name]` for each relation's serialized handle —
/// coexisting only because `Null` orders before `Str` in the memcmp
/// encoding. KyzoDB keeps the *design* (one system keyspace, counter
/// sorting below all relation rows) and makes the shapes a closed type:
/// this enum is the only minter of system keys, so no fourth shape can
/// appear by accident.
///
/// **Where `STORAGE_VERSION` went.** The kernel's
/// [`FormatVersion`](crate::FormatVersion) — stamped into every store at
/// open (`storage/fjall.rs`) and into every dump — is THE version concept
/// for KyzoDB's on-disk data. A store written by a different format version
/// refuses to open before any engine code runs, which is strictly earlier
/// and stricter than the original's catalog-row check. The catalog
/// therefore carries no second version; `SystemKey` deliberately has no
/// variant for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SystemKey<'a> {
    /// The relation-id counter: the last id allocated, as 8 big-endian
    /// bytes ([`RelationId::raw_encode`]). Encodes as `[Null]`, which sorts
    /// before every relation row.
    IdCounter,
    /// A relation's catalog row, holding its msgpack-serialized
    /// [`RelationHandle`]. Encodes as `[Str name]`.
    Relation(&'a str),
}

impl SystemKey<'_> {
    /// The key's written form. Encoding is infallible and the result is a
    /// proven [`StorageKey`], like every encoder in the kernel.
    pub(crate) fn encode(&self) -> StorageKey {
        match self {
            SystemKey::IdCounter => [DataValue::Null].encode_as_key(RelationId::SYSTEM),
            SystemKey::Relation(name) => [DataValue::from(*name)].encode_as_key(RelationId::SYSTEM),
        }
    }
}

/// The relation-id space (48 bits) is exhausted. Reaching this takes 2^48
/// relation creations; it is typed rather than panicking because stored
/// bytes (a corrupt counter row) could also claim an out-of-range id.
#[derive(Debug, Error, Diagnostic)]
#[error("the relation-id space is exhausted (2^48 ids)")]
#[diagnostic(code(tx::relation_id_exhausted))]
pub(crate) struct RelationIdSpaceExhausted;

/// The successor of an allocated relation id, or a typed error at the end
/// of the 48-bit id space. The original's `RelationId::new` panicked here.
///
/// `RelationId::raw_decode` owns the single bounds check for the id space;
/// routing through it keeps one source of truth for the bound. (Landing
/// note: this wants to be `RelationId::try_next` beside the type in
/// `data/tuple.rs`; it lives here so this file is the whole draft.)
fn next_relation_id(cur: RelationId) -> Result<RelationId> {
    // Cannot overflow u64: every minted or decoded id is <= 2^48.
    let next = cur.raw() + 1;
    RelationId::raw_decode(&next.to_be_bytes()).map_err(|_| RelationIdSpaceExhausted.into())
}

// ---------------------------------------------------------------------------
// Indices, by reference.
// ---------------------------------------------------------------------------

/// A reference to an index attached to a stored relation.
///
/// The CozoDB original embedded a full copy of each index's
/// `RelationHandle` (and manifest) inside the parent handle, and kept the
/// copies coherent across rename/drop by hand. Here an index is referenced
/// by name: an index *is* a stored relation (cataloged as
/// `{base}:{index}`, its own row the single authority), and consumers
/// resolve [`IndexRef::relation_name`] through the catalog when they need
/// the index relation's handle.
// `PartialEq` only: `IndexKind` carries float-bearing manifests.
#[derive(Debug, Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct IndexRef {
    /// The index's short name, unique among the parent's indices.
    pub(crate) name: SmartString<LazyCompact>,
    pub(crate) kind: IndexKind,
}

impl IndexRef {
    /// The catalog name of this index's backing relation. The one place
    /// the `{base}:{index}` naming convention is spelled.
    pub(crate) fn relation_name(&self, base: &str) -> String {
        format!("{base}:{}", self.name)
    }
}

/// What species of index an [`IndexRef`] names.
///
/// MIGRATION RECORD (search-operator tier): `Hnsw`, `Fts`, and `Lsh` began
/// as unit variants — typed attachment points with no payload. They gained
/// their manifests (and `Lsh` its inverse-relation name) when the search
/// operators landed. No store can hold a payload-less row of these kinds:
/// until this change no operation could attach one (the seam refused), so
/// the extension is decode-compatible with every store ever written. The
/// manifests contain floats, so the kinds are `PartialEq` only — the same
/// story as [`RelationHandle`] itself.
#[derive(Debug, Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) enum IndexKind {
    /// A plain projection index. `mapper[i]` gives, for the `i`-th column
    /// of the index relation, that column's position in the base
    /// relation's full tuple (keys then non-keys).
    Plain { mapper: Vec<usize> },
    /// The transposed event-posting index (issue #62's temporal
    /// acceleration structure): one posting per point event, keyed
    /// `[Validity(valid instant) as a LEADING data column][base key
    /// columns…][valid, sys tail]` — the valid instant promoted ahead of
    /// the base's own key so the posting keyspace orders by WHEN first,
    /// answering "what changed at/near instant t" with a contiguous scan
    /// instead of a full-relation walk. No payload of its own: the base
    /// key columns are the posting's whole identity, and its polarity
    /// mirrors the base write's exactly (assert/retract/erase), so the
    /// posting relation is itself an ordinary bitemporal `Facts` keyspace
    /// — a window read is an as-of read of postings, with no separate
    /// correction logic.
    ///
    /// Scan-shaped, not search-shaped: unlike `Hnsw`/`Fts`/`Lsh` (each a
    /// user-named, engine-probed search structure with its own manifest),
    /// `Temporal` carries no manifest and is maintained through the same
    /// seam as `Plain` — see `session/admit.rs`'s `update_indices` and
    /// `session/ops.rs`'s `attach_and_backfill`. A `Plain` mapper cannot express this kind:
    /// a mapper only permutes positions already present in the base ROW,
    /// and the leading column here is the WRITE'S OWN coordinate, which is
    /// never one of the row's columns.
    Temporal,
    /// Vector proximity (HNSW): the persisted manifest that rebuilds the
    /// index's parameters and extractor.
    Hnsw(crate::project::vector::hnsw::HnswIndexManifest),
    /// Full-text search: the persisted manifest (fields, tokenizer,
    /// filters), re-`build()`-able at any later time.
    Fts(crate::project::text::FtsIndexManifest),
    /// MinHash-LSH: the persisted manifest plus the name of the second,
    /// inverse relation (`{base}:{index}:inv`) the engine maintains.
    Lsh {
        manifest: crate::project::dedup::lsh::MinHashLshIndexManifest,
        inverse: SmartString<LazyCompact>,
    },
}

/// The synthetic leading column of a [`IndexKind::Temporal`] posting row:
/// the write's own valid instant, promoted ahead of the base relation's
/// key columns. Not a user-visible schema name in the sense of anything
/// scriptable today — the read-side RA operator (issue #62's other
/// write-side-adjacent chunk) is the first consumer that resolves it by
/// name — but it must be a real, stable `ColumnDef` name because the
/// posting relation's catalog row is real schema, decoded like any other.
pub(crate) const TEMPORAL_POSTING_LEADING_COLUMN: &str = "_posting_valid";

// ---------------------------------------------------------------------------
// Constraints, mirrored onto every relation they read.
// ---------------------------------------------------------------------------

/// An integrity constraint attached to a stored relation: a named denial
/// rule held as sealed [`InputProgram`] substance — never re-parsed at
/// enforcement or catalog decode. The admit door is [`ConstraintRef::parse`],
/// so a source that would fail its own parse can never become a
/// `ConstraintRef` and therefore can never be stored on a [`RelationHandle`].
///
/// The same `ConstraintRef` (same name, same program) is written into the
/// catalog row of **every** stored relation the body reads — an FK
/// constraint `deny child-without-parent` sits on both the child and the
/// parent, so deleting a parent row triggers the check exactly like
/// inserting a child row. The constraint's identity is its globally unique
/// name; the mutation pipeline dedups by name when several touched
/// relations carry the same constraint.
///
/// The catalog stores `{ name, program }` — sealed substance is the store
/// form. Decode admits the program through [`InputProgram::new`]; it does
/// not call `parse_script`.
#[derive(Clone, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct ConstraintRef {
    name: SmartString<LazyCompact>,
    program: InputProgram,
}

impl ConstraintRef {
    /// The one constructor: parse `source` into its single program, proving
    /// it is a well-formed constraint body. A non-parsing source is refused
    /// here, so it can never be stored. Only the sealed program is retained.
    pub(crate) fn parse(
        name: impl Into<SmartString<LazyCompact>>,
        source: &str,
        fixed_rules: &BTreeMap<String, Arc<dyn FixedRule>>,
        cur_vld: ValidityTs,
    ) -> Result<ConstraintRef> {
        let program =
            parse_script(source, &BTreeMap::new())?.get_single_program()?;
        Ok(ConstraintRef {
            name: name.into(),
            program,
        })
    }

    pub(crate) fn name(&self) -> &SmartString<LazyCompact> {
        &self.name
    }

    pub(crate) fn program(&self) -> &InputProgram {
        &self.program
    }
}

impl PartialEq for ConstraintRef {
    /// Catalog identity: equal iff name and sealed program wire match.
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && program_wire_eq(&self.program, &other.program)
    }
}

impl Eq for ConstraintRef {}

impl Debug for ConstraintRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "ConstraintRef({:?}, {})", self.name, self.program)
    }
}

// ---------------------------------------------------------------------------
// The relation handle.
// ---------------------------------------------------------------------------

/// A stored relation, as the catalog knows it: a decoded catalog row.
///
/// Possessing a handle is knowledge, not authority — it describes how to
/// encode keys and values for the relation's keyspace and what the schema
/// promises, but it is only as current as the snapshot it was decoded from;
/// the store's bytes remain the truth. Handles are re-read from the
/// catalog per transaction, never cached across commits.
///
/// The serialized form of this struct (msgpack, struct maps) **is the
/// on-disk catalog row format**; see the module docs and the pinned-bytes
/// test.
/// What a keyspace's rows ARE — the dispatch between the one universal
/// bitemporal fact format and version-less algorithm state.
#[derive(Debug, Copy, Clone, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) enum KeyspaceKind {
    /// Facts in the universal bitemporal format: two pinned time slots in
    /// the key, polarity in the value. Every read is an as-of resolution;
    /// there is no un-versioned read of a fact.
    Facts,
    /// A manifest index's internal state (HNSW graph rows, FTS postings,
    /// LSH bands, ...): exact-key and current-only. The algorithm owns its
    /// rows' lifecycle; versioning them would corrupt its invariants, and
    /// historical reads go through the base relation, never this state.
    AlgorithmState,
}

/// Which store a relation's rows live in — the routing datum every
/// storage-side operation dispatches on. This is a DERIVED classification,
/// never a stored field: a relation's residency is a total function of its
/// name (the `_` prefix marks a session-scratch temp relation), so a handle
/// whose residency disagrees with its name is unrepresentable — there is no
/// second authority to disagree. `is_temp: bool` on the catalog row was that
/// second authority; deleting it and computing residency here makes the name
/// the one truth.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum Residency {
    /// A persistent relation in the durable store.
    Stored,
    /// A session-scratch temp relation (name `_`-prefixed), routed to the
    /// session's temp store.
    Temp,
}

/// A relation trigger: a KyzoScript program run on mutation, held as sealed
/// [`InputProgram`] substance — never re-parsed at fire time or catalog
/// decode. The admit door is [`Trigger::parse`], which parses at
/// construction, so a source that would fail its own parse can never become
/// a `Trigger` and therefore can never be stored on a [`RelationHandle`].
///
/// The parsed `program`'s write validity stays SYMBOLIC (`WriteValidity::Now`
/// resolves at fire time against the firing session's instant), so the
/// substance is independent of the `cur_vld` it was parsed under.
/// `set_relation_triggers` still admits through [`Trigger::parse`] with
/// [`DEFAULT_FIXED_RULES`] and a sentinel instant so session-custom fixed
/// rules cannot enter the durable catalog. Decode admits the sealed program
/// through [`InputProgram::new`]; it does not call `parse_script`.
#[derive(Clone, serde_derive::Serialize, serde_derive::Deserialize)]
#[serde(transparent)]
pub(crate) struct Trigger {
    program: InputProgram,
}

/// The sentinel instant used when admitting a trigger source at the store
/// boundary ([`Trigger::parse`] / [`set_relation_triggers`]). Sound because a
/// stored trigger's program is `cur_vld`-independent (see [`Trigger`]).
fn trigger_decode_vld() -> ValidityTs {
    ValidityTs::from_raw(0)
}

/// Catalog identity for sealed programs: compare the durable msgpack form
/// (spans/trivia skipped), matching encode/decode round-trip.
fn program_wire_eq(a: &InputProgram, b: &InputProgram) -> bool {
    use rmp_serde::Serializer;
    use serde::Serialize;
    let enc = |p: &InputProgram| -> Option<Vec<u8>> {
        let mut ret = vec![];
        p.serialize(&mut Serializer::new(&mut ret).with_struct_map())
            .ok()?;
        Some(ret)
    };
    match (enc(a), enc(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

impl Trigger {
    /// The one constructor: parse `source` into its single program, proving
    /// it is a well-formed trigger body. A non-parsing source is refused
    /// here, so it can never be stored. Only the sealed program is retained.
    pub(crate) fn parse(
        source: &str,
        fixed_rules: &BTreeMap<String, Arc<dyn FixedRule>>,
        cur_vld: ValidityTs,
    ) -> Result<Trigger> {
        let program =
            parse_script(source, &BTreeMap::new())?.get_single_program()?;
        Ok(Trigger { program })
    }

    pub(crate) fn program(&self) -> &InputProgram {
        &self.program
    }
}

impl PartialEq for Trigger {
    /// Catalog identity: equal iff sealed program wire matches.
    fn eq(&self, other: &Self) -> bool {
        program_wire_eq(&self.program, &other.program)
    }
}

impl Eq for Trigger {}

impl Debug for Trigger {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Trigger({})", self.program)
    }
}

#[derive(Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct RelationHandle {
    pub(crate) name: SmartString<LazyCompact>,
    pub(crate) id: RelationId,
    pub(crate) metadata: StoredRelationMetadata,
    /// Triggers run on put/rm/replace, held as sealed [`Trigger`] substances
    /// (see that type). The catalog stores [`InputProgram`] bodies; decode
    /// admits through [`InputProgram::new`] and does not re-parse source.
    #[serde(default)]
    pub(crate) put_triggers: Vec<Trigger>,
    #[serde(default)]
    pub(crate) rm_triggers: Vec<Trigger>,
    #[serde(default)]
    pub(crate) replace_triggers: Vec<Trigger>,
    pub(crate) access_level: AccessLevel,
    /// Attached indices, by reference, sorted by name (the attach hook —
    /// operator tier — maintains the ordering; names are unique).
    pub(crate) indices: Vec<IndexRef>,
    pub(crate) description: SmartString<LazyCompact>,
    /// Integrity constraints whose bodies read this relation, held as
    /// sealed [`ConstraintRef`] substances (see that type). Sorted by name
    /// (`::constraint create` maintains the ordering; names are globally
    /// unique). The catalog stores `{ name, program }`; decode admits
    /// programs through [`InputProgram::new`] and does not re-parse source.
    pub(crate) constraints: Vec<ConstraintRef>,
    /// What this keyspace's rows are — see [`KeyspaceKind`]. Decides
    /// whether reads resolve bitemporally (facts) or exactly (algorithm
    /// state).
    pub(crate) keyspace_kind: KeyspaceKind,
}

impl Debug for RelationHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Relation<{}>", self.name)
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("Cannot deserialize catalog row for a relation")]
#[diagnostic(code(deser::relation))]
#[diagnostic(help(
    "The catalog row's bytes do not decode as a relation handle. This \
     indicates on-disk corruption or a bug; the store's format version \
     already matched at open, so a version mismatch is ruled out."
))]
pub(crate) struct RelationDeserError;

/// Named refusal when msgpack serialization of a catalog record fails.
#[derive(Debug, Error, Diagnostic)]
#[error("cannot serialize catalog record")]
#[diagnostic(code(catalog::serialize))]
pub(crate) struct CatalogSerializeRefused;

#[derive(Debug, Error, Diagnostic)]
#[error("Arity mismatch for stored relation {name}: expect {expect_arity}, got {actual_arity}")]
#[diagnostic(code(eval::stored_rel_arity_mismatch))]
struct StoredRelArityMismatch {
    name: String,
    expect_arity: usize,
    actual_arity: usize,
    #[label]
    span: SourceSpan,
}

/// # The catalog serialization boundary (RULED, not incidental)
///
/// Row VALUES are the value plane's job: canonical `DataValue` encodings
/// (`data::value::canonical`), byte-order == value-order, no serde. Catalog
/// METADATA — a relation's schema, access level, index manifests, triggers
/// — is structured configuration, NOT a tuple of values: it is nested
/// enums/options/maps that need self-describing struct serialization and
/// never a memcmp value order (catalog rows are looked up by name, never
/// range-scanned by value order). msgpack (serde struct maps) is the right
/// tool for that, and this module is its ONE door.
///
/// The boundary is SEALED so it can never become a second value authority:
/// [`CatalogRecord`] has a private supertrait, so only types declared in
/// this module can be serialized through it. `DataValue`, `Tuple`, and any
/// row value CANNOT implement `CatalogRecord`, so no row value can ever be
/// routed through msgpack. (A compile-fail proof asserts this.)
///
/// Corruption is typed ([`RelationDeserError`]); the whole store is stamped
/// with [`FormatVersion`](crate::FormatVersion), which a mismatched decoder
/// refuses at the door.
mod catalog {
    use super::{RelationDeserError, RelationHandle};
    use miette::Result;
    use rmp_serde::Serializer;
    use serde::Serialize;

    mod seal {
        pub trait Sealed {}
    }

    /// The CLOSED set of types stored as catalog metadata. Sealed: no type
    /// outside this module can implement it, so a row value can never be
    /// serialized through the catalog door.
    pub(super) trait CatalogRecord:
        Serialize + serde::de::DeserializeOwned + seal::Sealed
    {
    }

    impl seal::Sealed for RelationHandle {}
    impl CatalogRecord for RelationHandle {}

    /// ABSENCE PROOF: a row VALUE cannot be a catalog record, so nothing
    /// can route `DataValue` (or any value) through the msgpack door. The
    /// seal (private supertrait) already forbids it structurally; this
    /// locks it at compile time -- if `DataValue` ever implemented
    /// `CatalogRecord`, the associated-item lookup below would become
    /// ambiguous and fail to build.
    const _: fn() = || {
        trait AmbiguousIfImpl<A> {
            fn __proof() {}
        }
        impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
        // Marker exists only to give the second blanket impl below a
        // distinct type parameter; the ambiguity trick never constructs
        // it, so a plain dead_code lint fires by design.
        #[allow(dead_code)]
        struct Marker;
        impl<T: CatalogRecord> AmbiguousIfImpl<Marker> for T {}
        let _ = <kyzo_model::value::DataValue as AmbiguousIfImpl<_>>::__proof;
    };

    /// Serialize a catalog record: msgpack with struct maps. Infallible in
    /// practice for these field types, but a failure is a typed error, never
    /// an unwrap.
    pub(super) fn encode_catalog_record(rec: &impl CatalogRecord) -> Result<Vec<u8>> {
        let mut ret = vec![];
        rec.serialize(&mut Serializer::new(&mut ret).with_struct_map())
            .map_err(|_| super::CatalogSerializeRefused)?;
        Ok(ret)
    }

    /// Parse a claimed catalog record; corrupt bytes are the typed
    /// [`RelationDeserError`].
    pub(super) fn decode_catalog_record<T: CatalogRecord>(
        bytes: &[u8],
    ) -> std::result::Result<T, RelationDeserError> {
        rmp_serde::from_slice(bytes).map_err(|_| RelationDeserError)
    }
}

impl RelationHandle {
    /// The pure half of relation creation: shape a handle from the parsed
    /// input declaration and an allocated id. Storage writes happen in
    /// [`create_relation`]; the session tier reuses this constructor for
    /// temp relations against its own store.
    pub(crate) fn new_from_input(
        input: InputRelationHandle,
        id: RelationId,
        keyspace_kind: KeyspaceKind,
    ) -> Self {
        RelationHandle {
            name: input.name.name,
            id,
            metadata: input.metadata,
            put_triggers: vec![],
            rm_triggers: vec![],
            replace_triggers: vec![],
            access_level: AccessLevel::default(),
            indices: vec![],
            description: Default::default(),
            constraints: vec![],
            keyspace_kind,
        }
    }

    pub(crate) fn arity(&self) -> usize {
        self.metadata.non_keys.len() + self.metadata.keys.len()
    }

    /// Column name → position in the full tuple (keys then non-keys), for
    /// binding resolution.
    pub(crate) fn raw_binding_map(&self) -> BTreeMap<Symbol, usize> {
        let mut ret = BTreeMap::new();
        for (i, col) in self.metadata.keys.iter().enumerate() {
            ret.insert(Symbol::new(col.name.clone(), Default::default()), i);
        }
        for (i, col) in self.metadata.non_keys.iter().enumerate() {
            ret.insert(
                Symbol::new(col.name.clone(), Default::default()),
                i + self.metadata.keys.len(),
            );
        }
        ret
    }

    pub(crate) fn has_index(&self, index_name: &str) -> bool {
        self.indices.iter().any(|idx| idx.name == index_name)
    }

    pub(crate) fn has_no_index(&self) -> bool {
        self.indices.is_empty()
    }

    /// Which store this relation's rows live in, derived from its name (the
    /// `_` prefix marks a session-scratch temp relation). Residency has one
    /// authority — the name — so this can never disagree with how
    /// `get_relation`/`create_relation`/`destroy_relation` route by name.
    pub(crate) fn residency(&self) -> Residency {
        if self.name.starts_with('_') {
            Residency::Temp
        } else {
            Residency::Stored
        }
    }

    /// Whether mutation must collect old/new rows for trigger execution.
    /// Replace triggers are deliberately not consulted here, matching the
    /// original: the replace path does its own check.
    pub(crate) fn has_triggers(&self) -> bool {
        !self.put_triggers.is_empty() || !self.rm_triggers.is_empty()
    }

    /// The handle's wire form: msgpack with struct maps — the on-disk
    /// catalog row format. Serialization into a `Vec` cannot fail for
    /// these field types in practice, but per law 5 a failure is an error,
    /// never an unwrap (the original unwrapped at every serialize site).
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        catalog::encode_catalog_record(self)
    }

    /// Parse a claimed catalog row. Fallible: the bytes may be corrupt.
    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        Ok(catalog::decode_catalog_record(data)?)
    }

    // -- key/value encoding for this relation's keyspace ------------------

    /// Encode the key columns of `tuple` as this relation's storage key.
    pub(crate) fn encode_key_for_store(
        &self,
        tuple: &[DataValue],
        span: SourceSpan,
    ) -> Result<StorageKey> {
        let len = self.metadata.keys.len();
        ensure!(
            tuple.len() >= len,
            StoredRelArityMismatch {
                name: self.name.to_string(),
                expect_arity: self.arity(),
                actual_arity: tuple.len(),
                span
            }
        );
        Ok(tuple[0..len].encode_as_key(self.id))
    }

    /// Encode a key prefix (fewer columns than the full key) for prefix
    /// scans. Infallible: any prefix length is a valid scan seed.
    pub(crate) fn encode_partial_key_for_store(&self, tuple: &[DataValue]) -> StorageKey {
        tuple.encode_as_key(self.id)
    }

    /// Encode a bitemporal row's storage key: the key columns followed by
    /// the two pinned time slots (valid instant outer, system version
    /// inner). The slots are infrastructure, not schema — `tuple` holds
    /// user columns only, and what the row SAYS at the instant is its
    /// [`ClaimPolarity`], written by
    /// [`Self::encode_bitemporal_val_for_store`].
    pub(crate) fn encode_bitemporal_key_for_store(
        &self,
        tuple: &[DataValue],
        valid: ValidityTs,
        sys: ValidityTs,
        span: SourceSpan,
    ) -> Result<StorageKey> {
        let len = self.metadata.keys.len();
        ensure!(
            tuple.len() >= len,
            StoredRelArityMismatch {
                name: self.name.to_string(),
                expect_arity: self.arity(),
                actual_arity: tuple.len(),
                span
            }
        );
        let slot = |ts: ValidityTs| StoredValiditySlot::new(ts).as_datavalue();
        // Zero-clone: the tuple's key columns plus the two bitemporal
        // slots, encoded straight to bytes in one pass — every fact write
        // (put/update/remove alike) goes through this, so the
        // materialize-then-encode `Vec<DataValue>` it replaced was a second
        // allocation and a second pass over every row in the bulk-write
        // path.
        Ok(encode_key_with_suffix(
            self.id,
            &tuple[0..len],
            &[slot(valid), slot(sys)],
        ))
    }

    /// Write the fact's Assert row at the valid coordinate, stamped with
    /// the transaction's system instant — the one-stop fact write for any
    /// tier holding a row and a write transaction.
    pub(crate) fn put_fact(
        &self,
        tx: &mut impl WriteTx,
        row: &[DataValue],
        valid: ValidityTs,
        span: SourceSpan,
    ) -> Result<()> {
        let key = self.encode_bitemporal_key_for_store(row, valid, tx.system_stamp(), span)?;
        let val = self.encode_bitemporal_val_for_store(row, ClaimPolarity::Assert, span)?;
        tx.put(&key, &val)
    }

    /// Write the fact's Retract row at the valid coordinate — revision,
    /// not erasure.
    pub(crate) fn retract_fact(
        &self,
        tx: &mut impl WriteTx,
        key_cols: &[DataValue],
        valid: ValidityTs,
        span: SourceSpan,
    ) -> Result<()> {
        let key = self.encode_bitemporal_key_for_store(key_cols, valid, tx.system_stamp(), span)?;
        let val = self.encode_bitemporal_val_for_store(key_cols, ClaimPolarity::Retract, span)?;
        tx.put(&key, &val)
    }

    /// Encode a bitemporal row's stored value: the relation-id header, the
    /// polarity byte, then — for [`ClaimPolarity::Assert`] rows only — the
    /// msgpack non-key columns. A retraction or erasure speaks about the
    /// fact's key alone and carries no column data.
    pub(crate) fn encode_bitemporal_val_for_store(
        &self,
        tuple: &[DataValue],
        polarity: ClaimPolarity,
        span: SourceSpan,
    ) -> Result<Vec<u8>> {
        let start = self.metadata.keys.len();
        ensure!(
            tuple.len() >= start,
            StoredRelArityMismatch {
                name: self.name.to_string(),
                expect_arity: self.arity(),
                actual_arity: tuple.len(),
                span
            }
        );
        // The stored value opens with its polarity byte, then — for an
        // assertion — the non-key columns' canonical encodings. The
        // relation id is NOT repeated here: it is already the key's
        // 8-byte prefix, so carrying it in the value too would be pure
        // redundancy.
        let mut ret = Vec::with_capacity(1 + 16 * (tuple.len() - start));
        ret.push(polarity.encode());
        if polarity == ClaimPolarity::Assert {
            for v in &tuple[start..] {
                kyzo_model::value::append_canonical(&mut ret, v);
            }
        }
        Ok(ret)
    }

    /// Encode the non-key columns of `tuple` as this relation's stored
    /// value: the columns' canonical encodings concatenated. No header —
    /// the relation id is the key prefix, not repeated here, and the value
    /// decoder ([`extend_tuple_from_v`]) reads canonical bytes directly.
    pub(crate) fn encode_val_for_store(
        &self,
        tuple: &[DataValue],
        span: SourceSpan,
    ) -> Result<Vec<u8>> {
        let start = self.metadata.keys.len();
        // A short tuple is a caller arity error, reported as one — never a
        // slice-index panic.
        ensure!(
            tuple.len() >= start,
            StoredRelArityMismatch {
                name: self.name.to_string(),
                expect_arity: self.arity(),
                actual_arity: tuple.len(),
                span
            }
        );
        let mut ret = Vec::with_capacity(16 * (tuple.len() - start));
        for v in &tuple[start..] {
            kyzo_model::value::append_canonical(&mut ret, v);
        }
        Ok(ret)
    }

    /// Encode an arbitrary tuple as a stored value (used where the caller
    /// has already split keys from dependents). Canonical bytes, no header.
    pub(crate) fn encode_val_only_for_store(
        &self,
        tuple: &[DataValue],
        _span: SourceSpan,
    ) -> Result<Vec<u8>> {
        let mut ret = Vec::with_capacity(16 * tuple.len());
        for v in tuple {
            kyzo_model::value::append_canonical(&mut ret, v);
        }
        Ok(ret)
    }

    /// Prove that an input declaration is compatible with this relation's
    /// whole schema for `shape`. Constructs a branded
    /// [`CompatibleInputSchema`] or refuses the whole schema — never
    /// approves columns one at a time.
    pub(crate) fn prove_compatible_input(
        &self,
        inp: &InputRelationHandle,
        shape: RelationWriteShape,
    ) -> Result<CompatibleInputSchema> {
        CompatibleInputSchema::prove(&self.metadata, &inp.metadata, shape)
    }

    // -- the scan surface, against the kernel's transaction species -------
    //
    // Every method takes the transaction to read; none of them routes.
    // Temp-store routing is the session tier's job (see the module docs):
    // for a temp handle the session passes its temp store's transaction to
    // these same methods.

    /// The inclusive lower bound of this relation's keyspace: the empty
    /// tuple under its prefix.
    fn keyspace_lower(&self) -> StorageKey {
        Tuple::default().encode_as_key(self.id)
    }

    /// The exclusive upper bound of this relation's keyspace: the next
    /// relation prefix, as raw bytes. Scan bounds live in the claimed-bytes
    /// domain (see `data/tuple.rs`); computing the successor prefix
    /// directly also avoids minting a `RelationId` that would trip the id
    /// bound when `self.id` is the last allocatable id (law 5: the
    /// original's `id.next()` panicked there).
    fn keyspace_upper(&self) -> [u8; 8] {
        // Cannot overflow u64: ids are <= 2^48, enforced at mint and decode.
        (self.id.raw() + 1).to_be_bytes()
    }

    /// Scan every fact's CURRENT row (a bitemporal resolution at the
    /// current-belief coordinate — a fact has no un-versioned read), or
    /// every raw row of an algorithm-state keyspace.
    pub(crate) fn scan_all<'a>(
        &self,
        tx: &'a impl ReadTx,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        match self.keyspace_kind {
            KeyspaceKind::Facts => self.skip_scan_all(tx, AsOf::current(MAX_VALIDITY_TS)),
            KeyspaceKind::AlgorithmState => {
                tx.range_scan_tuple(&self.keyspace_lower(), &self.keyspace_upper())
            }
        }
    }

    /// Scan every row as RAW key/value bytes — the batched execution path's
    /// feed, which decodes straight into a flattened batch (no per-row
    /// `Tuple`). Same range, same memcmp order as [`scan_all`](Self::scan_all).
    pub(crate) fn scan_all_raw<'a>(
        &self,
        tx: &'a impl ReadTx,
    ) -> Box<dyn Iterator<Item = Result<(fjall::Slice, fjall::Slice)>> + 'a> {
        tx.range_scan(&self.keyspace_lower(), &self.keyspace_upper())
    }

    /// Drop a decoded bitemporal row's two time slots, yielding the
    /// LOGICAL row (user columns only). The slots are infrastructure —
    /// addressed by `@` coordinates, never visible as columns.
    fn strip_time_slots(keys_len: usize, mut t: Tuple) -> Tuple {
        t.drain(keys_len..keys_len + 2);
        t
    }

    /// Bitemporal as-of scan of every row: each fact resolved at the
    /// [`AsOf`] coordinate, asserted facts only, as LOGICAL rows.
    pub(crate) fn skip_scan_all<'a>(
        &self,
        tx: &'a impl ReadTx,
        as_of: AsOf,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let keys_len = self.metadata.keys.len();
        Box::new(
            tx.range_skip_scan_tuple(&self.keyspace_lower(), &self.keyspace_upper(), as_of)
                .map(move |r| r.map(|t| Self::strip_time_slots(keys_len, t))),
        )
    }

    /// The fact's current row at the coordinate, if asserted: the first
    /// (and by resolution, only) hit of a bitemporal probe under the
    /// fact's full key-column prefix — the versioned format's point read.
    /// Returns the LOGICAL row.
    pub(crate) fn current_row(
        &self,
        tx: &impl ReadTx,
        key_cols: &[DataValue],
        as_of: AsOf,
        span: SourceSpan,
    ) -> Result<Option<Tuple>> {
        let len = self.metadata.keys.len();
        ensure!(
            key_cols.len() >= len,
            StoredRelArityMismatch {
                name: self.name.to_string(),
                expect_arity: self.arity(),
                actual_arity: key_cols.len(),
                span
            }
        );
        let key_cols = &key_cols[0..len];
        // Zero-clone: this probe runs on EVERY mutated row (the SSI
        // uniqueness/conflict probe `put_into_relation` etc. cannot skip),
        // so the intermediate `Vec<DataValue>` the upper bound used to
        // build (key columns copied, then `Bot` pushed) was one whole heap
        // allocation per row that carried no information the direct
        // encode below doesn't already have.
        let lower = scan_key_lower(self.id, key_cols, &[]);
        let upper = scan_key_upper(self.id, key_cols, &[]);
        tx.range_skip_scan_tuple(&lower, &upper, as_of)
            .next()
            .transpose()
            .map(|opt| opt.map(|t| Self::strip_time_slots(len, t)))
    }

    /// Point-read one fact's CURRENT row by its key columns (facts), or
    /// one row by exact key (algorithm state).
    pub(crate) fn get(&self, tx: &impl ReadTx, key: &[DataValue]) -> Result<Option<Tuple>> {
        match self.keyspace_kind {
            KeyspaceKind::Facts => self.current_row(
                tx,
                key,
                AsOf::current(MAX_VALIDITY_TS),
                SourceSpan::default(),
            ),
            KeyspaceKind::AlgorithmState => {
                let key_data = key.encode_as_key(self.id);
                tx.get(&key_data)?
                    .map(|val_data| {
                        decode_tuple_from_kv(&key_data, &val_data, Some(self.arity()))
                            .map_err(miette::Report::from)
                    })
                    .transpose()
            }
        }
    }

    /// Point-read only the non-key columns of the fact's CURRENT row
    /// (facts) or of the exact key's row (algorithm state).
    pub(crate) fn get_val_only(
        &self,
        tx: &impl ReadTx,
        key: &[DataValue],
    ) -> Result<Option<Tuple>> {
        match self.keyspace_kind {
            KeyspaceKind::Facts => Ok(self
                .current_row(
                    tx,
                    key,
                    AsOf::current(MAX_VALIDITY_TS),
                    SourceSpan::default(),
                )?
                .map(|row| Tuple::from_vec(row.as_slice()[self.metadata.keys.len()..].to_vec()))),
            KeyspaceKind::AlgorithmState => {
                let key_data = key.encode_as_key(self.id);
                match tx.get(&key_data)? {
                    None => Ok(None),
                    Some(val_data) => {
                        let mut ret = Tuple::new();
                        extend_tuple_from_v(&mut ret, &val_data)?;
                        Ok(Some(ret))
                    }
                }
            }
        }
    }

    /// Whether the fact is CURRENTLY asserted (facts) or the exact key
    /// present (algorithm state).
    pub(crate) fn exists(&self, tx: &impl ReadTx, key: &[DataValue]) -> Result<bool> {
        match self.keyspace_kind {
            KeyspaceKind::Facts => Ok(self
                .current_row(
                    tx,
                    key,
                    AsOf::current(MAX_VALIDITY_TS),
                    SourceSpan::default(),
                )?
                .is_some()),
            KeyspaceKind::AlgorithmState => {
                let key_data = key.encode_as_key(self.id);
                tx.exists(&key_data)
            }
        }
    }

    /// Scan the CURRENT rows whose key columns start with `prefix`
    /// (facts), or the raw prefix range (algorithm state).
    pub(crate) fn scan_prefix<'a>(
        &self,
        tx: &'a impl ReadTx,
        prefix: &Tuple,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let cols: Vec<usize> = (0..prefix.len().min(self.metadata.keys.len())).collect();
        self.scan_prefix_projected(tx, prefix.as_slice(), &cols)
    }

    /// [`scan_prefix`](Self::scan_prefix) reading its prefix THROUGH a
    /// column projection of `row` — the zero-clone probe path: both scan
    /// bounds are encoded straight from the projected values, and no
    /// prefix tuple ever exists.
    pub(crate) fn scan_prefix_projected<'a>(
        &self,
        tx: &'a impl ReadTx,
        row: &[DataValue],
        cols: &[usize],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        match self.keyspace_kind {
            KeyspaceKind::Facts => {
                self.skip_scan_prefix_projected(tx, row, cols, AsOf::current(MAX_VALIDITY_TS))
            }
            KeyspaceKind::AlgorithmState => {
                let cols = &cols[..cols.len().min(self.metadata.keys.len())];
                let lower = scan_key_lower_projected(self.id, row, cols, &[]);
                let upper = scan_key_upper_projected(self.id, row, cols, &[]);
                tx.range_scan_tuple(&lower, &upper)
            }
        }
    }

    /// Bitemporal as-of variant of [`scan_prefix`](Self::scan_prefix),
    /// yielding LOGICAL rows.
    pub(crate) fn skip_scan_prefix<'a>(
        &self,
        tx: &'a impl ReadTx,
        prefix: &Tuple,
        as_of: AsOf,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let cols: Vec<usize> = (0..prefix.len().min(self.metadata.keys.len())).collect();
        self.skip_scan_prefix_projected(tx, prefix.as_slice(), &cols, as_of)
    }

    /// [`skip_scan_prefix`](Self::skip_scan_prefix) through a column
    /// projection — see [`scan_prefix_projected`](Self::scan_prefix_projected).
    pub(crate) fn skip_scan_prefix_projected<'a>(
        &self,
        tx: &'a impl ReadTx,
        row: &[DataValue],
        cols: &[usize],
        as_of: AsOf,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let keys_len = self.metadata.keys.len();
        let cols = &cols[..cols.len().min(keys_len)];
        let lower = scan_key_lower_projected(self.id, row, cols, &[]);
        let upper = scan_key_upper_projected(self.id, row, cols, &[]);
        Box::new(
            tx.range_skip_scan_tuple(&lower, &upper, as_of)
                .map(move |r| r.map(|t| Self::strip_time_slots(keys_len, t))),
        )
    }

    /// Scan CURRENT rows under `prefix` whose next key column lies in
    /// `[lower, upper]`.
    /// The byte bounds bracketing every key of this relation (the
    /// engines' whole-keyspace scans and counts).
    pub(crate) fn whole_relation_bounds(&self) -> (Vec<u8>, Vec<u8>) {
        (
            scan_key_lower(self.id, &[], &[]),
            scan_key_upper(self.id, &[], &[]),
        )
    }

    pub(crate) fn scan_bounded_prefix<'a>(
        &self,
        tx: &'a impl ReadTx,
        prefix: &[DataValue],
        lower: &[ScanBound],
        upper: &[ScanBound],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let lower_encoded = scan_key_lower(self.id, prefix, lower);
        let upper_encoded = scan_key_upper(self.id, prefix, upper);
        match self.keyspace_kind {
            KeyspaceKind::Facts => {
                let keys_len = self.metadata.keys.len();
                Box::new(
                    tx.range_skip_scan_tuple(
                        &lower_encoded,
                        &upper_encoded,
                        AsOf::current(MAX_VALIDITY_TS),
                    )
                    .map(move |r| r.map(|t| Self::strip_time_slots(keys_len, t))),
                )
            }
            KeyspaceKind::AlgorithmState => tx.range_scan_tuple(&lower_encoded, &upper_encoded),
        }
    }

    /// [`scan_bounded_prefix`](Self::scan_bounded_prefix) with the prefix
    /// read THROUGH a projection of `row` — both scan bounds encoded
    /// straight from projected values plus the bound literals.
    pub(crate) fn scan_bounded_prefix_projected<'a>(
        &self,
        tx: &'a impl ReadTx,
        row: &[DataValue],
        cols: &[usize],
        lower: &[ScanBound],
        upper: &[ScanBound],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let lower_encoded = scan_key_lower_projected(self.id, row, cols, lower);
        let upper_encoded = scan_key_upper_projected(self.id, row, cols, upper);
        match self.keyspace_kind {
            KeyspaceKind::Facts => {
                let keys_len = self.metadata.keys.len();
                Box::new(
                    tx.range_skip_scan_tuple(
                        &lower_encoded,
                        &upper_encoded,
                        AsOf::current(MAX_VALIDITY_TS),
                    )
                    .map(move |r| r.map(|t| Self::strip_time_slots(keys_len, t))),
                )
            }
            KeyspaceKind::AlgorithmState => tx.range_scan_tuple(&lower_encoded, &upper_encoded),
        }
    }

    /// As-of variant of
    /// [`scan_bounded_prefix_projected`](Self::scan_bounded_prefix_projected).
    pub(crate) fn skip_scan_bounded_prefix_projected<'a>(
        &self,
        tx: &'a impl ReadTx,
        row: &[DataValue],
        cols: &[usize],
        lower: &[ScanBound],
        upper: &[ScanBound],
        as_of: AsOf,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let lower_encoded = scan_key_lower_projected(self.id, row, cols, lower);
        let upper_encoded = scan_key_upper_projected(self.id, row, cols, upper);
        let keys_len = self.metadata.keys.len();
        Box::new(
            tx.range_skip_scan_tuple(&lower_encoded, &upper_encoded, as_of)
                .map(move |r| r.map(|t| Self::strip_time_slots(keys_len, t))),
        )
    }

    /// The current row whose key equals the projection of `row` — the point
    /// lookup of the zero-clone probe path (key bytes built straight from
    /// the projected values).
    pub(crate) fn current_row_projected(
        &self,
        tx: &impl ReadTx,
        row: &[DataValue],
        cols: &[usize],
    ) -> Result<Option<Tuple>> {
        let len = self.metadata.keys.len();
        debug_assert!(cols.len() >= len, "point probe under key width");
        let cols = &cols[..len];
        let lower = scan_key_lower_projected(self.id, row, cols, &[]);
        let upper = scan_key_upper_projected(self.id, row, cols, &[]);
        tx.range_skip_scan_tuple(&lower, &upper, AsOf::current(MAX_VALIDITY_TS))
            .next()
            .transpose()
            .map(|opt| opt.map(|t| Self::strip_time_slots(len, t)))
    }

    /// Bitemporal as-of variant of
    /// [`scan_bounded_prefix`](Self::scan_bounded_prefix), yielding
    /// LOGICAL rows.
    pub(crate) fn skip_scan_bounded_prefix<'a>(
        &self,
        tx: &'a impl ReadTx,
        prefix: &Tuple,
        lower: &[ScanBound],
        upper: &[ScanBound],
        as_of: AsOf,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let lower_encoded = scan_key_lower(self.id, prefix.as_slice(), lower);
        let upper_encoded = scan_key_upper(self.id, prefix.as_slice(), upper);
        let keys_len = self.metadata.keys.len();
        Box::new(
            tx.range_skip_scan_tuple(&lower_encoded, &upper_encoded, as_of)
                .map(move |r| r.map(|t| Self::strip_time_slots(keys_len, t))),
        )
    }
}

// ---------------------------------------------------------------------------
// Catalog operations.
// ---------------------------------------------------------------------------

#[derive(Debug, Diagnostic, Error)]
#[error("Cannot create relation {0} as one with the same name already exists")]
#[diagnostic(code(eval::rel_name_conflict))]
pub(crate) struct RelNameConflictError(pub(crate) String);

#[derive(Debug, Error, Diagnostic)]
#[error("Cannot find requested stored relation '{0}'")]
#[diagnostic(code(query::relation_not_found))]
pub(crate) struct StoredRelationNotFoundError(pub(crate) String);

/// SEAM (temp store): `_`-prefixed relations live in the session's
/// in-memory store, which lands with the evaluator's wiring. Until the
/// session router exists, addressing one through the persistent catalog is
/// a typed refusal, not a silent misplacement.
#[derive(Debug, Error, Diagnostic)]
#[error("temp relation '{0}' cannot be addressed through the persistent catalog")]
#[diagnostic(code(tx::temp_relation_not_routable))]
#[diagnostic(help(
    "temporary ('_'-prefixed) relations will live in the session's temp store; \
     this tier has no multi-script session to route them to, so they are refused \
     here rather than routed"
))]
pub(crate) struct TempRelationNotRoutable(pub(crate) String);

#[derive(Debug, Error, Diagnostic)]
#[error("cannot {1} stored relation '{0}' while indices are attached")]
#[diagnostic(code(tx::relation_has_indices))]
#[diagnostic(help("remove the indices first (::index drop and friends)"))]
pub(crate) struct RelationHasIndices(pub(crate) String, pub(crate) &'static str);

/// A destructive catalog operation on a relation that participates in an
/// integrity constraint. Destroying or renaming it would leave constraint
/// bodies referring to a name that no longer resolves, so every later write
/// to a sibling relation would fail on the dangling reference — refused
/// here instead, with the constraint named (the `RelationHasIndices`
/// shape).
#[derive(Debug, Error, Diagnostic)]
#[error("cannot {1} stored relation '{0}' while integrity constraint '{2}' reads it")]
#[diagnostic(code(tx::relation_has_constraints))]
#[diagnostic(help("drop the constraint first (::constraint drop <name>)"))]
pub(crate) struct RelationHasConstraints(
    pub(crate) String,
    pub(crate) &'static str,
    pub(crate) String,
);

/// Whether a relation of this name exists in the persistent catalog.
pub(crate) fn relation_exists(tx: &impl ReadTx, name: &str) -> Result<bool> {
    tx.exists(&SystemKey::Relation(name).encode())
}

/// Read and decode a relation's catalog row.
///
/// The original took a `lock: bool` to opt into a locked read; under the
/// kernel's SSI every read in a write transaction is conflict-tracked, so
/// the read itself is the lock and the parameter has nothing left to mean.
pub(crate) fn get_relation(tx: &impl ReadTx, name: &str) -> Result<RelationHandle> {
    let found = tx
        .get(&SystemKey::Relation(name).encode())?
        .ok_or_else(|| StoredRelationNotFoundError(name.to_string()))?;
    RelationHandle::decode(&found)
}

/// Every relation in the persistent catalog, in name order (index
/// relations — `{base}:{index}` — included).
///
/// The scan starts at `[Str ""]`: the id counter encodes as `[Null]` and
/// `Null` orders before `Str` in the memcmp encoding, so the counter row
/// sits below the scan by construction (the design the original relied on,
/// kept deliberately).
pub(crate) fn list_relations(tx: &impl ReadTx) -> Result<Vec<RelationHandle>> {
    let lower = SystemKey::Relation("").encode();
    // The exclusive upper bound is the next relation prefix; SYSTEM is id
    // zero, so its successor is always in range.
    let upper = RelationId::SYSTEM
        .next()
        .expect("SYSTEM is id zero; its successor exists")
        .raw_encode();
    let mut ret = vec![];
    for kv in tx.range_scan(&lower, &upper) {
        let (_k, v) = kv?;
        ret.push(RelationHandle::decode(&v)?);
    }
    Ok(ret)
}

/// Allocate the next relation id: a transactional read-modify-write of the
/// [`SystemKey::IdCounter`] row.
///
/// ## Concurrency story, stated plainly
///
/// The kernel is full SSI and conflict-tracks this read (present *or*
/// absent). Two transactions that both allocate see the same counter, both
/// write its successor, and exactly one commits; the other fails with the
/// typed [`ConflictError`](crate::ConflictError) and the engine's retry
/// loop reruns it, which then observes the committed counter. Uniqueness
/// is isolation's theorem, not an atomic's side effect — and unlike the
/// original's process-wide `AtomicU64`, an aborted transaction rolls its
/// allocation back instead of leaking the id.
fn allocate_relation_id(tx: &mut impl WriteTx) -> Result<RelationId> {
    let counter_key = SystemKey::IdCounter.encode();
    let last = match tx.get(&counter_key)? {
        Some(bytes) => RelationId::raw_decode(&bytes)?,
        // Fresh store: nothing allocated yet; SYSTEM (zero) is the floor.
        None => RelationId::SYSTEM,
    };
    let id = next_relation_id(last)?;
    tx.put(&counter_key, &id.raw_encode())?;
    Ok(id)
}

/// Create a stored relation from its parsed declaration: allocate an id,
/// shape the handle, and write its catalog row. The relation's keyspace
/// starts empty; no data write happens here.
pub(crate) fn create_relation(
    tx: &mut impl WriteTx,
    input: InputRelationHandle,
    keyspace_kind: KeyspaceKind,
) -> Result<RelationHandle> {
    if input.name.is_temp_relation_name() {
        bail!(TempRelationNotRoutable(input.name.to_string()));
    }
    let row_key = SystemKey::Relation(&input.name.name).encode();
    if tx.exists(&row_key)? {
        bail!(RelNameConflictError(input.name.to_string()));
    }
    let id = allocate_relation_id(tx)?;
    let handle = RelationHandle::new_from_input(input, id, keyspace_kind);
    tx.put(&row_key, &handle.encode()?)?;
    Ok(handle)
}

/// Write a handle's catalog row (the single row-update path; every
/// mutation below funnels through it).
pub(crate) fn write_relation_row(tx: &mut impl WriteTx, handle: &RelationHandle) -> Result<()> {
    let row_key = SystemKey::Relation(&handle.name).encode();
    tx.put(&row_key, &handle.encode()?)
}

/// Destroy a stored relation: delete its catalog row and its whole
/// keyspace, inside this transaction.
///
/// The original returned byte ranges for a deferred post-commit cleanup
/// pass (a RocksDB-era shape); the kernel's [`WriteTx::del_range`] deletes
/// both snapshot data and the transaction's own writes, so destruction is
/// atomic with the rest of the transaction and an abort rolls it back.
pub(crate) fn destroy_relation(tx: &mut impl WriteTx, name: &str) -> Result<()> {
    let store = get_relation(tx, name)?;
    if !store.has_no_index() {
        bail!(RelationHasIndices(name.to_string(), "remove"));
    }
    if let Some(c) = store.constraints.first() {
        bail!(RelationHasConstraints(
            name.to_string(),
            "remove",
            c.name().to_string()
        ));
    }
    if store.access_level < AccessLevel::Normal {
        bail!(InsufficientAccessLevel(
            store.name.to_string(),
            "relation removal".to_string(),
            store.access_level
        ));
    }
    tx.del(&SystemKey::Relation(name).encode())?;
    let lower = Tuple::default().encode_as_key(store.id);
    // Successor prefix as raw bytes; cannot overflow (ids <= 2^48).
    let upper = (store.id.raw() + 1).to_be_bytes();
    tx.del_range(&lower, &upper)
}

/// Set a relation's access level. Deliberately ungated (matching the
/// original): lowering and raising the level is how relations are locked
/// and unlocked, so gating it on itself would wedge them shut.
pub(crate) fn set_access_level(
    tx: &mut impl WriteTx,
    name: &str,
    level: AccessLevel,
) -> Result<()> {
    let mut meta = get_relation(tx, name)?;
    meta.access_level = level;
    write_relation_row(tx, &meta)
}

/// Parse a list of trigger sources into [`Trigger`] substances at the store
/// boundary. Uses [`DEFAULT_FIXED_RULES`] + the sentinel instant so a source
/// referencing a session-custom fixed rule is refused HERE — the durable
/// catalog cannot carry it. Catalog decode admits sealed programs through
/// [`InputProgram::new`] and never re-parses these sources.
fn parse_triggers(sources: &[String]) -> Result<Vec<Trigger>> {
    sources
        .iter()
        .map(|src| Trigger::parse(src, &DEFAULT_FIXED_RULES, trigger_decode_vld()))
        .collect()
}

/// Replace a relation's triggers. Requires at least
/// [`AccessLevel::Protected`]. Each source is lifted through
/// [`Trigger::parse`] at this boundary, so an unparseable trigger cannot be
/// persisted.
pub(crate) fn set_relation_triggers(
    tx: &mut impl WriteTx,
    name: &Symbol,
    puts: &[String],
    rms: &[String],
    replaces: &[String],
) -> Result<()> {
    if name.is_temp_relation_name() {
        bail!(TempRelationNotRoutable(name.to_string()));
    }
    let mut original = get_relation(tx, name)?;
    if original.access_level < AccessLevel::Protected {
        bail!(InsufficientAccessLevel(
            original.name.to_string(),
            "set triggers".to_string(),
            original.access_level
        ));
    }
    original.put_triggers = parse_triggers(puts)?;
    original.rm_triggers = parse_triggers(rms)?;
    original.replace_triggers = parse_triggers(replaces)?;
    write_relation_row(tx, &original)
}

/// Set a relation's human-readable description.
pub(crate) fn describe_relation(
    tx: &mut impl WriteTx,
    name: &str,
    description: &str,
) -> Result<()> {
    let mut meta = get_relation(tx, name)?;
    meta.description = SmartString::from(description);
    write_relation_row(tx, &meta)
}

/// Rename a stored relation: move its catalog row. The relation's id — and
/// therefore its whole keyspace — is untouched; only the name key changes.
///
/// Refuses while indices are attached: index relations are cataloged as
/// `{base}:{index}`, so a coherent rename must move their rows too. That
/// lands with the operator tier that owns index lifecycles (the original
/// renamed the base and stranded the index rows under the old name).
pub(crate) fn rename_relation(tx: &mut impl WriteTx, old: &Symbol, new: &Symbol) -> Result<()> {
    if old.is_temp_relation_name() || new.is_temp_relation_name() {
        bail!(TempRelationNotRoutable(format!("{old} -> {new}")));
    }
    let new_row_key = SystemKey::Relation(&new.name).encode();
    if tx.exists(&new_row_key)? {
        bail!(RelNameConflictError(new.name.to_string()));
    }
    let mut rel = get_relation(tx, old)?;
    if !rel.has_no_index() {
        bail!(RelationHasIndices(old.to_string(), "rename"));
    }
    if let Some(c) = rel.constraints.first() {
        bail!(RelationHasConstraints(
            old.to_string(),
            "rename",
            c.name().to_string()
        ));
    }
    if rel.access_level < AccessLevel::Normal {
        bail!(InsufficientAccessLevel(
            rel.name.to_string(),
            "renaming relation".to_string(),
            rel.access_level
        ));
    }
    rel.name = new.name.clone();
    tx.del(&SystemKey::Relation(&old.name).encode())?;
    tx.put(&new_row_key, &rel.encode()?)
}

// ─────────────────────────────────────────────────────────────────────────
// Engine sys-op doors for catalog-facing system ops
// ─────────────────────────────────────────────────────────────────────────

impl<S: Storage> Engine<S> {
    /// `::relations` — list stored relations.
    pub(crate) fn sys_list_relations(&self) -> Result<NamedRows> {
        let tx = SessionTx::new_read(self.store.read_tx()?, ScriptOptions::default());
        let mut rows = vec![];
        for handle in list_relations(&tx.store)? {
            rows.push(Tuple::from_vec(vec![
                DataValue::from(handle.name.as_str()),
                DataValue::from(handle.arity() as i64),
                DataValue::from(format!("{:?}", handle.access_level)),
            ]));
        }
        Ok(NamedRows::try_new(
            vec!["name".into(), "arity".into(), "access_level".into()],
            rows,
        )?)
    }

    /// `::columns` — list key/non-key columns of a relation.
    pub(crate) fn sys_list_columns(&self, name: &Symbol) -> Result<NamedRows> {
        let tx = SessionTx::new_read(self.store.read_tx()?, ScriptOptions::default());
        let handle = get_relation(&tx.store, &name.name)?;
        let mut rows = vec![];
        for col in handle
            .metadata
            .keys
            .iter()
            .map(|c| (c, true))
            .chain(handle.metadata.non_keys.iter().map(|c| (c, false)))
        {
            rows.push(Tuple::from_vec(vec![
                DataValue::from(col.0.name.as_str()),
                DataValue::from(col.1),
            ]));
        }
        Ok(NamedRows::try_new(vec!["column".into(), "is_key".into()], rows)?)
    }

    /// `::fixed_rules` — names of registered fixed rules.
    pub(crate) fn sys_list_fixed_rules(&self) -> Result<NamedRows> {
        let rows = self
            .fixed_rules()
            .keys()
            .map(|k| Tuple::from_vec(vec![DataValue::from(k.as_str())]))
            .collect();
        Ok(NamedRows::try_new(vec!["name".into()], rows)?)
    }

    /// `::show_triggers` — put/rm/replace trigger sources on a relation.
    pub(crate) fn sys_show_trigger(&self, name: &Symbol) -> Result<NamedRows> {
        let tx = SessionTx::new_read(self.store.read_tx()?, ScriptOptions::default());
        let handle = get_relation(&tx.store, &name.name)?;
        let mut rows = vec![];
        for (kind, src) in handle
            .put_triggers
            .iter()
            .map(|s| ("on_put", s))
            .chain(handle.rm_triggers.iter().map(|s| ("on_rm", s)))
            .chain(handle.replace_triggers.iter().map(|s| ("on_replace", s)))
        {
            rows.push(Tuple::from_vec(vec![
                DataValue::from(kind),
                DataValue::from(src.program().to_string()),
            ]));
        }
        Ok(NamedRows::try_new(vec!["kind".into(), "source".into()], rows)?)
    }

    /// `::remove` — destroy one or more relations.
    pub(crate) fn sys_remove_relation(&self, names: Vec<Symbol>) -> Result<NamedRows> {
        self.sys_write(|tx| {
            for name in &names {
                tx.destroy_relation(&name.name)?;
            }
            Ok(status_ok())
        })
    }

    /// `::rename` — rename relation pairs.
    pub(crate) fn sys_rename_relation(
        &self,
        pairs: Vec<(Symbol, Symbol)>,
    ) -> Result<NamedRows> {
        self.sys_write(|tx| {
            for (old, new) in &pairs {
                rename_relation(&mut tx.store, old, new)?;
            }
            Ok(status_ok())
        })
    }

    /// `::describe` — set a relation's description.
    pub(crate) fn sys_describe_relation(
        &self,
        name: &Symbol,
        desc: &str,
    ) -> Result<NamedRows> {
        self.sys_write(|tx| {
            describe_relation(&mut tx.store, &name.name, desc)?;
            Ok(status_ok())
        })
    }

    /// `::set_triggers` — replace put/rm/replace trigger sources.
    pub(crate) fn sys_set_triggers(
        &self,
        name: Symbol,
        puts: Vec<String>,
        rms: Vec<String>,
        replaces: Vec<String>,
    ) -> Result<NamedRows> {
        self.sys_write(move |tx| {
            set_relation_triggers(&mut tx.store, &name, &puts, &rms, &replaces)?;
            Ok(status_ok())
        })
    }

    /// `::access` — set access level on named relations.
    pub(crate) fn sys_set_access_level(
        &self,
        names: Vec<Symbol>,
        level: ParseAccessLevel,
    ) -> Result<NamedRows> {
        let level = map_access_level(level);
        self.sys_write(move |tx| {
            for name in &names {
                set_access_level(&mut tx.store, &name.name, level)?;
            }
            Ok(status_ok())
        })
    }
}

// ---------------------------------------------------------------------------
// Tests: the catalog's executable law.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use rmp_serde::Serializer;
    use serde::Serialize;

    use super::*;
    use kyzo_model::schema::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
    use kyzo_model::value::ValidityTs;
    use kyzo_model::value::decode_tuple_from_key;
    use crate::store::fjall::new_fjall_storage;
    use crate::store::{ConflictError, Storage};

    fn col(name: &str, coltype: ColType) -> ColumnDef {
        ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType::required(coltype),
            default_gen: None,
        }
    }

    fn input_handle(
        name: &str,
        keys: Vec<ColumnDef>,
        non_keys: Vec<ColumnDef>,
    ) -> InputRelationHandle {
        let key_bindings = keys
            .iter()
            .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
            .collect();
        let dep_bindings = non_keys
            .iter()
            .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
            .collect();
        InputRelationHandle {
            name: Symbol::new(name, SourceSpan(0, 0)),
            metadata: StoredRelationMetadata { keys, non_keys },
            key_bindings,
            dep_bindings,
            span: SourceSpan(0, 0),
        }
    }

    fn simple_input(name: &str) -> InputRelationHandle {
        input_handle(
            name,
            vec![col("k", ColType::Int)],
            vec![col("v", ColType::String)],
        )
    }

    /// The typed system keys encode to exactly the ratified shapes: the id
    /// counter is `[Null]` and a relation row is `[Str name]`, both under
    /// the SYSTEM prefix — and the counter orders below every relation row
    /// (Null < Str in the memcmp encoding), which is what lets `list` scan
    /// the Str range without seeing it.
    #[test]
    fn system_key_shapes() {
        let counter = SystemKey::IdCounter.encode();
        assert_eq!(&counter[..8], &[0u8; 8], "SYSTEM prefix");
        let want_counter: Tuple = Tuple::from_vec(vec![DataValue::Null]);
        assert_eq!(decode_tuple_from_key(&counter, 1).unwrap(), want_counter);

        let rel = SystemKey::Relation("stored").encode();
        assert_eq!(&rel[..8], &[0u8; 8], "SYSTEM prefix");
        let want_rel: Tuple = Tuple::from_vec(vec![DataValue::from("stored")]);
        assert_eq!(decode_tuple_from_key(&rel, 1).unwrap(), want_rel);

        assert!(
            counter.as_bytes() < rel.as_bytes(),
            "the counter row sorts below every relation row"
        );
        // And below even the empty-named relation row (the list scan's
        // lower bound).
        assert!(counter.as_bytes() < SystemKey::Relation("").encode().as_bytes());
    }

    /// Catalog round trip against a real store: create, get, list, write a
    /// row through the handle, destroy — and destroy takes the data with it.
    #[test]
    fn catalog_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();

        // Create two relations.
        let mut tx = db.write_tx().unwrap();
        let a = create_relation(&mut tx, simple_input("alpha"), KeyspaceKind::Facts).unwrap();
        let b = create_relation(&mut tx, simple_input("beta"), KeyspaceKind::Facts).unwrap();
        assert_ne!(a.id, b.id);
        tx.commit().unwrap();

        // Get and list see them, in name order.
        let rtx = db.read_tx().unwrap();
        let got = get_relation(&rtx, "alpha").unwrap();
        assert_eq!(got, a);
        assert!(relation_exists(&rtx, "beta").unwrap());
        assert!(!relation_exists(&rtx, "gamma").unwrap());
        assert!(
            get_relation(&rtx, "gamma")
                .unwrap_err()
                .downcast_ref::<StoredRelationNotFoundError>()
                .is_some()
        );
        let listed = list_relations(&rtx).unwrap();
        assert_eq!(
            listed.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        drop(rtx);

        // Write and read a row through the handle.
        let span = SourceSpan(0, 0);
        let row: Tuple = Tuple::from_vec(vec![DataValue::from(1), DataValue::from("one")]);
        let mut tx = db.write_tx().unwrap();
        a.put_fact(&mut tx, row.as_slice(), ValidityTs::from_raw(0), span)
            .unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        assert!(a.exists(&rtx, &row.as_slice()[..1]).unwrap());
        assert_eq!(
            a.get(&rtx, &row.as_slice()[..1]).unwrap(),
            Some(row.clone())
        );
        assert_eq!(
            a.get_val_only(&rtx, &row.as_slice()[..1]).unwrap(),
            Some(Tuple::from_vec(vec![DataValue::from("one")]))
        );
        let scanned: Vec<Tuple> = a.scan_all(&rtx).map(|t| t.unwrap()).collect();
        assert_eq!(scanned, vec![row.clone()]);
        // The row is invisible from beta's keyspace.
        assert_eq!(b.scan_all(&rtx).count(), 0);
        drop(rtx);

        // Destroy alpha: catalog row and data both gone.
        let mut tx = db.write_tx().unwrap();
        destroy_relation(&mut tx, "alpha").unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        assert!(!relation_exists(&rtx, "alpha").unwrap());
        assert_eq!(a.scan_all(&rtx).count(), 0, "destroy deletes the keyspace");
        let listed = list_relations(&rtx).unwrap();
        assert_eq!(
            listed.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
            vec!["beta"]
        );
    }

    /// A corrupt catalog row cannot synthesize an out-of-range
    /// `RelationId` — the exact seam the bug fix closes.
    /// `RelationHandle::decode` is `rmp_serde::from_slice` straight over
    /// stored bytes, so a shadow struct with `id` as a bare (unvalidated)
    /// `u64` in place of `RelationId` stands in for what a corrupted store
    /// could hold. The real `RelationHandle` must refuse decoding it —
    /// typed error, never a panic, never a constructed handle.
    #[test]
    fn corrupt_catalog_row_refuses_out_of_range_relation_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let handle =
            create_relation(&mut tx, simple_input("hostile"), KeyspaceKind::Facts).unwrap();

        #[derive(serde_derive::Serialize)]
        struct ShadowHandle {
            name: SmartString<LazyCompact>,
            id: u64,
            metadata: StoredRelationMetadata,
            put_triggers: Vec<Trigger>,
            rm_triggers: Vec<Trigger>,
            replace_triggers: Vec<Trigger>,
            access_level: AccessLevel,
            indices: Vec<IndexRef>,
            description: SmartString<LazyCompact>,
            constraints: Vec<ConstraintRef>,
            keyspace_kind: KeyspaceKind,
        }

        let shadow = ShadowHandle {
            name: handle.name.clone(),
            id: kyzo_model::value::RelationId::CAP + 1, // out of the allocatable range
            metadata: handle.metadata.clone(),
            put_triggers: handle.put_triggers.clone(),
            rm_triggers: handle.rm_triggers.clone(),
            replace_triggers: handle.replace_triggers.clone(),
            access_level: handle.access_level,
            indices: handle.indices.clone(),
            description: handle.description.clone(),
            constraints: handle.constraints.clone(),
            keyspace_kind: handle.keyspace_kind,
        };
        let mut bytes = vec![];
        shadow
            .serialize(&mut Serializer::new(&mut bytes).with_struct_map())
            .unwrap();

        // Must refuse typed, not panic and not hand back a handle carrying
        // an out-of-range id.
        RelationHandle::decode(&bytes).unwrap_err();
    }

    /// `del_range` semantics through destroy: a relation created, filled,
    /// and destroyed within one transaction leaves nothing behind — the
    /// transaction's own writes die with the range.
    #[test]
    fn destroy_within_one_transaction_kills_own_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let rel = create_relation(&mut tx, simple_input("fleeting"), KeyspaceKind::Facts).unwrap();
        let span = SourceSpan(0, 0);
        let row = vec![DataValue::from(9), DataValue::from("gone")];
        rel.put_fact(&mut tx, row.as_slice(), ValidityTs::from_raw(0), span)
            .unwrap();
        destroy_relation(&mut tx, "fleeting").unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        assert!(!relation_exists(&rtx, "fleeting").unwrap());
        assert_eq!(rel.scan_all(&rtx).count(), 0);
    }

    /// Ids are allocated sequentially from the persisted counter, and the
    /// counter row holds the last id handed out.
    #[test]
    fn id_counter_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();

        let mut tx = db.write_tx().unwrap();
        let first = create_relation(&mut tx, simple_input("one"), KeyspaceKind::Facts).unwrap();
        let second = create_relation(&mut tx, simple_input("two"), KeyspaceKind::Facts).unwrap();
        tx.commit().unwrap();
        assert_eq!(first.id, RelationId::new(1).expect("below cap"));
        assert_eq!(second.id, RelationId::new(2).expect("below cap"));

        // A later transaction continues where the counter left off.
        let mut tx = db.write_tx().unwrap();
        let third = create_relation(&mut tx, simple_input("three"), KeyspaceKind::Facts).unwrap();
        tx.commit().unwrap();
        assert_eq!(third.id, RelationId::new(3).expect("below cap"));

        let rtx = db.read_tx().unwrap();
        let counter = rtx.get(&SystemKey::IdCounter.encode()).unwrap().unwrap();
        assert_eq!(
            RelationId::raw_decode(&counter).unwrap(),
            RelationId::new(3).expect("below cap")
        );
    }

    /// The concurrency story, proven: two transactions racing on the id
    /// counter conflict at commit; exactly one wins and the loser's error
    /// is the typed, retryable `ConflictError`.
    #[test]
    fn concurrent_creates_conflict_and_retry_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();

        let mut tx1 = db.write_tx().unwrap();
        let mut tx2 = db.write_tx().unwrap();
        create_relation(&mut tx1, simple_input("left"), KeyspaceKind::Facts).unwrap();
        create_relation(&mut tx2, simple_input("right"), KeyspaceKind::Facts).unwrap();
        tx1.commit().unwrap();
        let err = tx2
            .commit()
            .expect_err("racing counter writes must conflict");
        assert!(
            err.is_conflict(),
            "the loser gets the typed, retryable conflict: {err:?}"
        );

        // The retry (a fresh transaction) sees the committed counter and
        // allocates the next id.
        let mut tx3 = db.write_tx().unwrap();
        let right = create_relation(&mut tx3, simple_input("right"), KeyspaceKind::Facts).unwrap();
        tx3.commit().unwrap();
        assert_eq!(right.id, RelationId::new(2).expect("below cap"));
    }

    /// The access ladder gates destructive operations: `Ord` is the
    /// semantics.
    #[test]
    fn access_level_gates() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();

        let mut tx = db.write_tx().unwrap();
        create_relation(&mut tx, simple_input("guarded"), KeyspaceKind::Facts).unwrap();
        set_access_level(&mut tx, "guarded", AccessLevel::ReadOnly).unwrap();
        tx.commit().unwrap();

        let mut tx = db.write_tx().unwrap();
        // ReadOnly < Normal: destruction refused.
        let err = destroy_relation(&mut tx, "guarded").unwrap_err();
        assert!(err.downcast_ref::<InsufficientAccessLevel>().is_some());
        // ReadOnly < Protected: trigger changes refused.
        let err = set_relation_triggers(
            &mut tx,
            &Symbol::new("guarded", SourceSpan(0, 0)),
            &[],
            &[],
            &[],
        )
        .unwrap_err();
        assert!(err.downcast_ref::<InsufficientAccessLevel>().is_some());
        // Renaming refused too.
        let err = rename_relation(
            &mut tx,
            &Symbol::new("guarded", SourceSpan(0, 0)),
            &Symbol::new("freed", SourceSpan(0, 0)),
        )
        .unwrap_err();
        assert!(err.downcast_ref::<InsufficientAccessLevel>().is_some());

        // Restore Normal (setting the level itself is ungated by design)
        // and destruction proceeds.
        set_access_level(&mut tx, "guarded", AccessLevel::Normal).unwrap();
        destroy_relation(&mut tx, "guarded").unwrap();
        tx.commit().unwrap();
    }

    /// Exhausting the 48-bit id space is a typed error, not the original's
    /// panic.
    #[test]
    fn relation_id_overflow_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();

        let mut tx = db.write_tx().unwrap();
        // Force the counter to the last allocatable id, so the next
        // allocation crosses the ceiling.
        tx.put(
            &SystemKey::IdCounter.encode(),
            &(kyzo_model::value::RelationId::CAP - 1).to_be_bytes(),
        )
        .unwrap();
        let err =
            create_relation(&mut tx, simple_input("too_many"), KeyspaceKind::Facts).unwrap_err();
        assert!(
            err.downcast_ref::<RelationIdSpaceExhausted>().is_some(),
            "expected typed exhaustion, got: {err:?}"
        );
    }

    /// The persistent catalog refuses temp names (the session's temp store
    /// owns them — see the routing seam), and duplicate names.
    #[test]
    fn create_refuses_temp_names_and_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();

        let mut tx = db.write_tx().unwrap();
        let err =
            create_relation(&mut tx, simple_input("_scratch"), KeyspaceKind::Facts).unwrap_err();
        assert!(err.downcast_ref::<TempRelationNotRoutable>().is_some());

        create_relation(&mut tx, simple_input("once"), KeyspaceKind::Facts).unwrap();
        let err = create_relation(&mut tx, simple_input("once"), KeyspaceKind::Facts).unwrap_err();
        assert!(err.downcast_ref::<RelNameConflictError>().is_some());
    }

    /// Rename moves the catalog row and nothing else: same id, same data.
    #[test]
    fn rename_moves_the_row_and_keeps_the_keyspace() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let rel = create_relation(&mut tx, simple_input("before"), KeyspaceKind::Facts).unwrap();
        let span = SourceSpan(0, 0);
        let row: Tuple = Tuple::from_vec(vec![DataValue::from(5), DataValue::from("five")]);
        rel.put_fact(&mut tx, row.as_slice(), ValidityTs::from_raw(0), span)
            .unwrap();
        rename_relation(
            &mut tx,
            &Symbol::new("before", SourceSpan(0, 0)),
            &Symbol::new("after", SourceSpan(0, 0)),
        )
        .unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        assert!(!relation_exists(&rtx, "before").unwrap());
        let renamed = get_relation(&rtx, "after").unwrap();
        assert_eq!(renamed.id, rel.id, "the id — and the keyspace — survive");
        assert_eq!(renamed.get(&rtx, &row.as_slice()[..1]).unwrap(), Some(row));
    }

    /// Time travel through the handle's skip-scan surface: the newest
    /// version at or before the query time wins, and a retraction is an
    /// honest absence.
    #[test]
    fn skip_scan_sees_the_asserted_past() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let rel = create_relation(
            &mut tx,
            input_handle("beliefs", vec![col("k", ColType::Int)], vec![]),
            KeyspaceKind::Facts,
        )
        .unwrap();
        let vts_of = |ts: i64| ValidityTs::from_raw(ts);
        // k=1 asserted at valid t=10, retracted at valid t=20, through
        // the handle's own fact writers.
        rel.put_fact(&mut tx, &[DataValue::from(1)], vts_of(10), SourceSpan(0, 0))
            .unwrap();
        rel.retract_fact(&mut tx, &[DataValue::from(1)], vts_of(20), SourceSpan(0, 0))
            .unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let at = |ts: i64| -> Vec<Tuple> {
            rel.skip_scan_all(&rtx, AsOf::current(ValidityTs::from_raw(ts)))
                .map(|t| t.unwrap())
                .collect()
        };
        assert_eq!(at(5).len(), 0, "before the assertion: not yet believed");
        let hits = at(15);
        assert_eq!(hits.len(), 1, "between assert and retract: believed");
        assert_eq!(hits[0][0], DataValue::from(1));
        assert_eq!(at(25).len(), 0, "after the retraction: revised away");
    }

    /// Whole-schema compatibility proof: an input's *dependent* columns are
    /// type-checked as part of one constructor (the original compared the
    /// stored relation's non-keys against themselves and let any input
    /// dependent type through). Partial column approval is impossible —
    /// prove constructs whole or refuses whole.
    #[test]
    fn prove_compatible_input_checks_input_dependents_whole() {
        use kyzo_model::schema::RelationWriteShape::{Put, RemoveOrUpdate};

        let stored = RelationHandle::new_from_input(
            simple_input("s"),
            RelationId::new(1).expect("below cap"),
            KeyspaceKind::Facts,
        );
        // Compatible input: same shapes → branded proof.
        stored
            .prove_compatible_input(&simple_input("s"), Put)
            .expect("compatible whole schema");
        // Incompatible dependent type: v as Int against stored String.
        let bad = input_handle(
            "s",
            vec![col("k", ColType::Int)],
            vec![col("v", ColType::Int)],
        );
        assert!(
            stored.prove_compatible_input(&bad, Put).is_err(),
            "fix-on-port: dependent column types are checked in the whole-schema proof"
        );
        // Removal/update: dependents need not be provided...
        let keys_only = input_handle("s", vec![col("k", ColType::Int)], vec![]);
        stored
            .prove_compatible_input(&keys_only, RemoveOrUpdate)
            .expect("keys-only is enough for remove/update");
        // ...but a full put requires them (or defaults).
        assert!(stored.prove_compatible_input(&keys_only, Put).is_err());
    }

    /// The T2 closure: a malformed trigger source is refused at the store
    /// boundary and never persisted. `set_relation_triggers` lifts every
    /// source through `Trigger::parse` BEFORE writing the catalog row, so a
    /// source that cannot parse is a typed error here — no stored-but-
    /// unparseable trigger can exist. A valid source, by contrast, persists
    /// and decodes back to the same provenance, proving the refusal is
    /// specific to malformed input, not "triggers never store".
    #[test]
    fn malformed_trigger_source_is_refused_and_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();

        let mut tx = db.write_tx().unwrap();
        create_relation(&mut tx, simple_input("t"), KeyspaceKind::Facts).unwrap();
        set_access_level(&mut tx, "t", AccessLevel::Protected).unwrap();
        tx.commit().unwrap();

        // A source that is not a valid single program: refused, and because
        // the parse happens before any catalog write, nothing lands even
        // within the (uncommitted) transaction.
        let mut tx = db.write_tx().unwrap();
        set_relation_triggers(
            &mut tx,
            &Symbol::new("t", SourceSpan(0, 0)),
            &["this ][ is not valid kyzoscript".to_string()],
            &[],
            &[],
        )
        .unwrap_err();
        drop(tx);

        let tx = db.read_tx().unwrap();
        let handle = get_relation(&tx, "t").unwrap();
        assert!(
            handle.put_triggers.is_empty() && !handle.has_triggers(),
            "the malformed source never became a stored trigger"
        );
        drop(tx);

        // A valid source persists and survives the catalog round-trip as the
        // same provenance — the non-vacuity half of the closure.
        let mut tx = db.write_tx().unwrap();
        set_relation_triggers(
            &mut tx,
            &Symbol::new("t", SourceSpan(0, 0)),
            &["?[k, v] := *t[k, v]".to_string()],
            &[],
            &[],
        )
        .unwrap();
        tx.commit().unwrap();

        let tx = db.read_tx().unwrap();
        let handle = get_relation(&tx, "t").unwrap();
        assert_eq!(handle.put_triggers.len(), 1);
        // Display uses `:rel` for stored-relation atoms; substance is the program.
        assert!(
            handle.put_triggers[0]
                .program()
                .to_string()
                .contains("t[k, v]"),
            "trigger program retained stored-relation body"
        );
        assert!(handle.has_triggers());
    }

    /// The handle wire format round-trips, and its bytes are pinned: this
    /// IS an on-disk format now (each catalog row stores exactly these
    /// bytes), so any change to it must arrive together with a migration
    /// decision — this test failing is that conversation starting.
    #[test]
    fn handle_wire_format_round_trips_and_is_pinned() {
        let mut handle = RelationHandle::new_from_input(
            input_handle(
                "pin",
                vec![col("k", ColType::Int)],
                vec![col("v", ColType::String)],
            ),
            RelationId::new(7).expect("below cap"),
            KeyspaceKind::Facts,
        );
        handle.put_triggers = vec![
            Trigger::parse(
                "?[k, v] := *pin[k, v]",
                &DEFAULT_FIXED_RULES,
                trigger_decode_vld(),
            )
            .unwrap(),
        ];
        handle.access_level = AccessLevel::ReadOnly;
        handle.indices = vec![IndexRef {
            name: SmartString::from("by_v"),
            kind: IndexKind::Plain { mapper: vec![1, 0] },
        }];
        handle.description = SmartString::from("pinned");
        handle.constraints = vec![ConstraintRef::parse(
            "no_empty_v",
            "?[k] := *pin[k, v], v == ''",
            &DEFAULT_FIXED_RULES,
            trigger_decode_vld(),
        )
        .unwrap()];

        let bytes = handle.encode().unwrap();
        let decoded = RelationHandle::decode(&bytes).unwrap();
        assert_eq!(decoded, handle, "wire round trip");

        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, PINNED_HANDLE_HEX,
            "the catalog row wire format changed; this is an on-disk \
             format migration, not a refactor — see the module docs"
        );

        // Corrupt bytes are a typed decode error, never a panic (law 5).
        let err = RelationHandle::decode(&bytes.as_slice()[..bytes.len() / 2]).unwrap_err();
        assert!(err.downcast_ref::<RelationDeserError>().is_some());
        assert!(RelationHandle::decode(b"garbage").is_err());
    }

    /// The language surface's coordinate ORDER, pinned with a
    /// discriminating history (reviewer's probe): a retroactive write —
    /// valid instant far in the past, system stamp now — reads back only
    /// when the parser maps `@ first, second` to (system, valid). Swapped
    /// coordinates put the system cut before the write's stamp, where the
    /// record knew nothing.
    #[test]
    fn asof_clause_first_coordinate_is_system_time() {
        use std::collections::BTreeMap;
        use crate::session::db::Engine;
        use kyzo_model::SourceSpan;

        fn no_params() -> BTreeMap<String, DataValue> {
            BTreeMap::new()
        }
        fn open_engine<S: Storage>(store: S) -> Engine<S> {
            Engine::compose(store, Catalog::new()).expect("compose engine")
        }

        let dir = tempfile::tempdir().unwrap();
        let db = open_engine(new_fjall_storage(dir.path()).unwrap());
        db.run_script(
            "?[k, v] <- [[9, 'seed']] :create hist {k => v}",
            no_params(),
        )
        .expect("create");
        // The retroactive write: valid = 150 µs (ancient), sys = now.
        let mut tx = db.store.write_tx().unwrap();
        let handle = get_relation(&tx, "hist").unwrap();
        handle
            .put_fact(
                &mut tx,
                &[DataValue::from(1), DataValue::from("retro")],
                ValidityTs::from_raw(150),
                SourceSpan(0, 0),
            )
            .unwrap();
        tx.commit().unwrap();
        let now = crate::session::current_validity().unwrap().raw();

        // (sys=now, valid=200): the record NOW says the fact held at 200.
        let rows = db
            .run_script(&format!("?[v] := *hist[1, v @ {now}, 200]"), no_params())
            .expect("two-coordinate read");
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![DataValue::from("retro")])];
        assert_eq!(
            rows.rows(),
            want,
            "system-now, valid-200 must see the retroactive claim"
        );
        // Swapped (sys=200, valid=now): at system time 200 µs the record
        // did not exist; a parser that swapped coordinates would return
        // the row here and the empty set above.
        let rows = db
            .run_script(&format!("?[v] := *hist[1, v @ 200, {now}]"), no_params())
            .expect("swapped-coordinate read");
        assert!(
            rows.rows().is_empty(),
            "at system time 200µs the record knew nothing: {rows:?}"
        );
    }

    /// The `@` clause parses in both arities through the public script
    /// surface: `@ valid` (current belief) and `@ system, valid` (the
    /// record as it was). Resolution semantics are pinned by the
    /// time-travel trials; this pins the LANGUAGE surface.
    #[test]
    fn asof_clause_parses_one_and_two_coordinates() {
        use std::collections::BTreeMap;
        use crate::session::db::Engine;

        fn no_params() -> BTreeMap<String, DataValue> {
            BTreeMap::new()
        }
        fn open_engine<S: Storage>(store: S) -> Engine<S> {
            Engine::compose(store, Catalog::new()).expect("compose engine")
        }

        let dir = tempfile::tempdir().unwrap();
        let db = open_engine(new_fjall_storage(dir.path()).unwrap());
        db.run_script("?[k, v] <- [[1, 10]] :create hist {k => v}", no_params())
            .expect("create");
        db.run_script("?[k, v] := *hist[k, v @ 12345]", no_params())
            .expect("single-coordinate as-of parses and runs");
        db.run_script("?[k, v] := *hist[k, v @ 12345, 67890]", no_params())
            .expect("two-coordinate as-of parses and runs");
        db.run_script("?[k, v] := *hist{k, v @ 12345, 67890}", no_params())
            .expect("two-coordinate as-of parses in named form");
    }

    /// The pinned wire bytes of the canonical handle above (msgpack,
    /// struct maps; FormatVersion v6 — sealed InputProgram triggers/constraints).
    /// Regenerate ONLY as part of a deliberate format migration — the version
    /// stamp refuses mismatched stores, so there is exactly one catalog format
    /// per FormatVersion and no cross-version decode path.
    const PINNED_HANDLE_HEX: &str = include_str!("pinned_handle.hex");
}
