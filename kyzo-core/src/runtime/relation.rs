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
 * - The original's `amend_key_prefix` is deleted: it was dead code
 *   (`#[allow(dead_code)]` upstream) and splicing a different relation id
 *   into an [`EncodedKey`]'s bytes would launder unproven provenance into
 *   the typed key. If a real consumer appears it returns as an encoder.
 * - Fix on port: the original's `ensure_compatible` chained the *input*'s
 *   key columns with the *stored* relation's own non-key columns, so an
 *   input's dependent columns were never type-checked (a self-comparison
 *   that is trivially compatible). It now checks the input's keys and the
 *   input's non-keys, as the surrounding comments always claimed.
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
//!   functions here address the persistent store only; `SessionTx` (runtime
//!   tier) owns the routing decision, handing these same [`RelationHandle`]
//!   scan/write methods the temp store's transaction instead. Nothing here
//!   fakes a temp store; [`create_relation`] refuses temp names with a
//!   typed error until the router exists.
//! - **Triggers** are stored as raw KyzoScript source strings, re-parsed at
//!   fire time — exactly the original's shape. The ratified end state
//!   stores them as parsed substances with provenance once the parse tier's
//!   program types are consumable here (Phase C).
//! - **Index manifests** (HNSW / FTS / LSH) are operator-tier substances
//!   that have not landed. [`IndexKind`]'s non-plain variants are the typed
//!   attachment points; they gain manifest payloads when the operator tier
//!   lands, which is a catalog wire-format change that the pinned-bytes
//!   test below forces to be deliberate.
//! - **`as_named_rows`** (the original's handle-to-`NamedRows` view) lands
//!   with the runtime tier that owns `NamedRows`.
//! - **[`IndexPositionUse`]** is a compile-tier concept, homed here
//!   provisionally because `query/compile.rs` has not landed; it moves
//!   there unchanged when it does.
//!
//! ## Wire format
//!
//! A catalog row's value is the handle serialized as msgpack **with struct
//! maps** (self-describing field names, resilient to field reordering).
//! This is an on-disk format: the round-trip and pinned-bytes tests below
//! are its executable law, and changing it is a migration conversation, not
//! a refactor.

use std::collections::BTreeMap;
use std::fmt::{Debug, Display, Formatter};

use miette::{Diagnostic, Result, bail, ensure, miette};
use rmp_serde::Serializer;
use serde::Serialize;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::bitemporal::ClaimPolarity;
use crate::data::program::InputRelationHandle;
use crate::data::relation::StoredRelationMetadata;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::tuple::{
    EncodedKey, RelationId, Tuple, TupleT, decode_tuple_from_kv, extend_tuple_from_v,
};
use crate::data::tuple::{
    encode_key_with_suffix, encode_projected_key, encode_projected_key_with_suffix,
};
use crate::data::value::{AsOf, DataValue, MAX_VALIDITY_TS, Validity, ValidityTs};
use crate::storage::{ReadTx, WriteTx};

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
    /// proven [`EncodedKey`], like every encoder in the kernel.
    pub(crate) fn encode(&self) -> EncodedKey {
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
    let next = cur.0 + 1;
    RelationId::raw_decode(&next.to_be_bytes()).map_err(|_| RelationIdSpaceExhausted.into())
}

// ---------------------------------------------------------------------------
// Access levels.
// ---------------------------------------------------------------------------

/// What operations a stored relation admits. **The `Ord` derive IS the
/// semantics** (an existing type-driven win of the original, preserved
/// deliberately): every gate is a comparison, `Hidden < ReadOnly <
/// Protected < Normal`, so "at least protected" is `>= Protected` and
/// nothing re-encodes the ladder. Do not reorder the variants.
#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    PartialEq,
    serde_derive::Serialize,
    serde_derive::Deserialize,
    Default,
    Ord,
    PartialOrd,
)]
pub enum AccessLevel {
    Hidden,
    ReadOnly,
    Protected,
    #[default]
    Normal,
}

impl Display for AccessLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessLevel::Normal => f.write_str("normal"),
            AccessLevel::Protected => f.write_str("protected"),
            AccessLevel::ReadOnly => f.write_str("read_only"),
            AccessLevel::Hidden => f.write_str("hidden"),
        }
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("Insufficient access level {2} for {1} on stored relation '{0}'")]
#[diagnostic(code(tx::insufficient_access_level))]
pub(crate) struct InsufficientAccessLevel(
    pub(crate) String,
    pub(crate) String,
    pub(crate) AccessLevel,
);

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
    /// Vector proximity (HNSW): the persisted manifest that rebuilds the
    /// index's parameters and extractor.
    Hnsw(crate::engines::hnsw::HnswIndexManifest),
    /// Full-text search: the persisted manifest (fields, tokenizer,
    /// filters), re-`build()`-able at any later time.
    Fts(crate::engines::text::FtsIndexManifest),
    /// MinHash-LSH: the persisted manifest plus the name of the second,
    /// inverse relation (`{base}:{index}:inv`) the engine maintains.
    Lsh {
        manifest: crate::engines::lsh::MinHashLshIndexManifest,
        inverse: SmartString<LazyCompact>,
    },
}

// ---------------------------------------------------------------------------
// Constraints, mirrored onto every relation they read.
// ---------------------------------------------------------------------------

/// An integrity constraint attached to a stored relation: a named denial
/// rule. The body is a pure query whose non-empty result at commit time
/// denies the transaction; its satisfying rows are the violation witnesses.
///
/// The same `ConstraintRef` (same name, same source) is written into the
/// catalog row of **every** stored relation the body reads — an FK
/// constraint `deny child-without-parent` sits on both the child and the
/// parent, so deleting a parent row triggers the check exactly like
/// inserting a child row. The constraint's identity is its globally unique
/// name; the mutation pipeline dedups by name when several touched
/// relations carry the same constraint.
///
/// The body is raw KyzoScript source, parsed once per session (the trigger
/// convention; parsed substances in the catalog are the Phase C end state).
#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct ConstraintRef {
    /// The constraint's name, unique across the whole database.
    pub(crate) name: SmartString<LazyCompact>,
    /// The denial rule's body: a full query script, stored as source.
    pub(crate) source: String,
}

/// How the compile tier uses each argument position of a stored-relation
/// atom, for index selection.
///
/// PROVISIONAL HOME: this is a compile-tier concept (the original defines
/// it in `query/compile.rs`); it lives here only because that tier has not
/// landed, and it moves there unchanged when it does.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum IndexPositionUse {
    /// The position is bound and can seed an index prefix scan.
    Join,
    /// The position is needed later but not bound for the scan.
    BindForLater,
    /// The position is not used at all.
    Ignored,
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

#[derive(Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct RelationHandle {
    pub(crate) name: SmartString<LazyCompact>,
    pub(crate) id: RelationId,
    pub(crate) metadata: StoredRelationMetadata,
    /// SEAM (parse tier, Phase C): triggers are raw KyzoScript source,
    /// re-parsed when fired — the original's shape. The ratified end state
    /// is parsed substances with stored provenance, once the parse tier's
    /// program types can be embedded here.
    pub(crate) put_triggers: Vec<String>,
    pub(crate) rm_triggers: Vec<String>,
    pub(crate) replace_triggers: Vec<String>,
    pub(crate) access_level: AccessLevel,
    /// Whether this relation lives in the session's temp store. Handles in
    /// the persistent catalog always have `false`; the field exists so the
    /// session tier (which owns the temp store) can construct temp handles
    /// with the same type. See the module docs' temp-routing seam.
    pub(crate) is_temp: bool,
    /// Attached indices, by reference, sorted by name (the attach hook —
    /// operator tier — maintains the ordering; names are unique).
    pub(crate) indices: Vec<IndexRef>,
    pub(crate) description: SmartString<LazyCompact>,
    /// Integrity constraints whose bodies read this relation, sorted by
    /// name (`::constraint create` maintains the ordering; names are
    /// globally unique).
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

impl RelationHandle {
    /// The pure half of relation creation: shape a handle from the parsed
    /// input declaration and an allocated id. Storage writes happen in
    /// [`create_relation`]; the session tier reuses this constructor for
    /// temp relations against its own store.
    pub(crate) fn new_from_input(
        input: InputRelationHandle,
        id: RelationId,
        is_temp: bool,
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
            is_temp,
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
        let mut ret = vec![];
        self.serialize(&mut Serializer::new(&mut ret).with_struct_map())
            .map_err(|e| miette!("cannot serialize relation handle for {}: {e}", self.name))?;
        Ok(ret)
    }

    /// Parse a claimed catalog row. Fallible: the bytes may be corrupt.
    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        Ok(rmp_serde::from_slice(data).map_err(|_| RelationDeserError)?)
    }

    // -- key/value encoding for this relation's keyspace ------------------

    /// Encode the key columns of `tuple` as this relation's storage key.
    pub(crate) fn encode_key_for_store(
        &self,
        tuple: &[DataValue],
        span: SourceSpan,
    ) -> Result<EncodedKey> {
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
        Ok((&tuple[0..len]).encode_as_key(self.id))
    }

    /// Encode a key prefix (fewer columns than the full key) for prefix
    /// scans. Infallible: any prefix length is a valid scan seed.
    pub(crate) fn encode_partial_key_for_store(&self, tuple: &[DataValue]) -> EncodedKey {
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
    ) -> Result<EncodedKey> {
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
        let slot = |ts: ValidityTs| {
            DataValue::Validity(Validity {
                timestamp: ts,
                is_assert: std::cmp::Reverse(true),
            })
        };
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
        let mut ret = Vec::with_capacity(9 + 16 * (tuple.len() - start));
        ret.extend(self.id.raw_encode());
        ret.push(polarity.encode());
        if polarity == ClaimPolarity::Assert {
            crate::data::fact_payload::encode_fact_payload(&tuple[start..], &mut ret)
                .map_err(|e| miette!("cannot serialize row payload for {}: {e}", self.name))?;
        }
        Ok(ret)
    }

    /// Encode the non-key columns of `tuple` as this relation's stored
    /// value: an 8-byte header (the relation id — carried because the
    /// kernel's value decoder, `extend_tuple_from_v`, is pinned to skip
    /// exactly `VALUE_HEADER_LEN` bytes) followed by the msgpack payload.
    pub(crate) fn encode_val_for_store(
        &self,
        tuple: &[DataValue],
        span: SourceSpan,
    ) -> Result<Vec<u8>> {
        let start = self.metadata.keys.len();
        // The original sliced `tuple[start..]` unchecked and panicked on a
        // short tuple (law 5); arity is a caller error, reported as one.
        ensure!(
            tuple.len() >= start,
            StoredRelArityMismatch {
                name: self.name.to_string(),
                expect_arity: self.arity(),
                actual_arity: tuple.len(),
                span
            }
        );
        let mut ret = Vec::with_capacity(8 + 8 * (tuple.len() - start));
        ret.extend(self.id.raw_encode());
        tuple[start..]
            .serialize(&mut Serializer::new(&mut ret))
            .map_err(|e| miette!("cannot serialize row payload for {}: {e}", self.name))?;
        Ok(ret)
    }

    /// Encode an arbitrary tuple as a stored value (used where the caller
    /// has already split keys from dependents).
    pub(crate) fn encode_val_only_for_store(
        &self,
        tuple: &[DataValue],
        _span: SourceSpan,
    ) -> Result<Vec<u8>> {
        let mut ret = Vec::with_capacity(8 + 8 * tuple.len());
        ret.extend(self.id.raw_encode());
        tuple
            .serialize(&mut Serializer::new(&mut ret))
            .map_err(|e| miette!("cannot serialize row payload for {}: {e}", self.name))?;
        Ok(ret)
    }

    /// Check that an input declaration is compatible with this relation:
    /// every input column exists here with a compatible type, and every
    /// required stored column is provided (or has a default). For removals
    /// and updates only the key columns must be provided.
    ///
    /// Fix on port: the original chained the input's keys with **this
    /// relation's own** non-keys (a trivially-true self-comparison), so an
    /// input's dependent columns were never type-checked. The input's
    /// non-keys are checked now.
    pub(crate) fn ensure_compatible(
        &self,
        inp: &InputRelationHandle,
        is_remove_or_update: bool,
    ) -> Result<()> {
        let InputRelationHandle { metadata, .. } = inp;
        // Every given column must be found here and be type-compatible.
        for col in metadata.keys.iter().chain(metadata.non_keys.iter()) {
            self.metadata.compatible_with_col(col)?
        }
        // Every key must be provided or have a default.
        for col in &self.metadata.keys {
            metadata.satisfied_by_required_col(col)?;
        }
        if !is_remove_or_update {
            for col in &self.metadata.non_keys {
                metadata.satisfied_by_required_col(col)?;
            }
        }
        Ok(())
    }

    /// Choose the plain index whose column mapper matches the longest
    /// prefix of bound (`Join`) argument positions. Returns the chosen
    /// index reference and whether a back-join to the base relation is
    /// still required (some needed position is not covered by the index).
    ///
    /// By reference: the caller resolves the index relation's handle via
    /// [`IndexRef::relation_name`] and [`get_relation`] — this handle does
    /// not embed a copy of it. Manifest-backed indices (HNSW/FTS/LSH) are
    /// never chosen here; their own operators own their access paths.
    pub(crate) fn choose_index(
        &self,
        arg_uses: &[IndexPositionUse],
        validity_query: bool,
    ) -> Option<(IndexRef, bool)> {
        // Law 5: the original `unwrap`ped `first()`; a zero-arity atom
        // simply has no index to choose.
        let first = arg_uses.first()?;
        if *first == IndexPositionUse::Join {
            // The base relation's own key prefix is already usable.
            return None;
        }
        let required_positions: Vec<usize> = arg_uses
            .iter()
            .enumerate()
            .filter_map(|(i, pos_use)| (*pos_use != IndexPositionUse::Ignored).then_some(i))
            .collect();
        let mut max_prefix_len = 0usize;
        let mut chosen = None;
        for index in &self.indices {
            let IndexKind::Plain { mapper } = &index.kind else {
                continue;
            };
            // As-of queries use plain indexes freely: every plain index
            // row carries the base row's bitemporal coordinate and
            // polarity (the mutation tier mirrors them), so an index scan
            // resolves at any coordinate exactly like the base.
            let _ = validity_query;
            let mut cur_prefix_len = 0usize;
            for i in mapper {
                // A mapper position beyond the argument list would mean a
                // stale catalog row; it ends the usable prefix rather than
                // panicking (law 5: the original indexed unchecked).
                match arg_uses.get(*i) {
                    Some(IndexPositionUse::Join) => cur_prefix_len += 1,
                    _ => break,
                }
            }
            if cur_prefix_len > max_prefix_len {
                max_prefix_len = cur_prefix_len;
                let requires_back_join =
                    required_positions.iter().any(|need| !mapper.contains(need));
                chosen = Some((index.clone(), requires_back_join));
            }
        }
        chosen
    }

    // -- the scan surface, against the kernel's transaction species -------
    //
    // Every method takes the transaction to read; none of them routes.
    // Temp-store routing is the session tier's job (see the module docs):
    // for a temp handle the session passes its temp store's transaction to
    // these same methods. `is_temp` on the handle is the routing *datum*,
    // not the router.

    /// The inclusive lower bound of this relation's keyspace: the empty
    /// tuple under its prefix.
    fn keyspace_lower(&self) -> EncodedKey {
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
        (self.id.0 + 1).to_be_bytes()
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
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
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
        let lower = key_cols.encode_as_key(self.id);
        let upper = encode_key_with_suffix(self.id, key_cols, &[DataValue::Bot]);
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
                    .map(|val_data| decode_tuple_from_kv(&key_data, &val_data, Some(self.arity())))
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
                .map(|row| row[self.metadata.keys.len()..].to_vec())),
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
        self.scan_prefix_projected(tx, prefix, &cols)
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
                let lower = encode_projected_key(self.id, row, cols);
                let upper = encode_projected_key_with_suffix(self.id, row, cols, &[DataValue::Bot]);
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
        self.skip_scan_prefix_projected(tx, prefix, &cols, as_of)
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
        let lower = encode_projected_key(self.id, row, cols);
        let upper = encode_projected_key_with_suffix(self.id, row, cols, &[DataValue::Bot]);
        Box::new(
            tx.range_skip_scan_tuple(&lower, &upper, as_of)
                .map(move |r| r.map(|t| Self::strip_time_slots(keys_len, t))),
        )
    }

    /// Scan CURRENT rows under `prefix` whose next key column lies in
    /// `[lower, upper]`.
    pub(crate) fn scan_bounded_prefix<'a>(
        &self,
        tx: &'a impl ReadTx,
        prefix: &[DataValue],
        lower: &[DataValue],
        upper: &[DataValue],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let mut lower_t = prefix.to_vec();
        lower_t.extend_from_slice(lower);
        let mut upper_t = prefix.to_vec();
        upper_t.extend_from_slice(upper);
        upper_t.push(DataValue::Bot);
        let lower_encoded = lower_t.encode_as_key(self.id);
        let upper_encoded = upper_t.encode_as_key(self.id);
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
        lower: &[DataValue],
        upper: &[DataValue],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let lower_encoded = encode_projected_key_with_suffix(self.id, row, cols, lower);
        let mut upper_owned = upper.to_vec();
        upper_owned.push(DataValue::Bot);
        let upper_encoded = encode_projected_key_with_suffix(self.id, row, cols, &upper_owned);
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
        lower: &[DataValue],
        upper: &[DataValue],
        as_of: AsOf,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let lower_encoded = encode_projected_key_with_suffix(self.id, row, cols, lower);
        let mut upper_owned = upper.to_vec();
        upper_owned.push(DataValue::Bot);
        let upper_encoded = encode_projected_key_with_suffix(self.id, row, cols, &upper_owned);
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
        let lower = encode_projected_key(self.id, row, cols);
        let upper = encode_projected_key_with_suffix(self.id, row, cols, &[DataValue::Bot]);
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
        lower: &[DataValue],
        upper: &[DataValue],
        as_of: AsOf,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let mut lower_t = prefix.clone();
        lower_t.extend_from_slice(lower);
        let mut upper_t = prefix.clone();
        upper_t.extend_from_slice(upper);
        upper_t.push(DataValue::Bot);
        let lower_encoded = lower_t.encode_as_key(self.id);
        let upper_encoded = upper_t.encode_as_key(self.id);
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
    let upper = RelationId::SYSTEM.next().raw_encode();
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
    let handle = RelationHandle::new_from_input(input, id, false, keyspace_kind);
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
            c.name.to_string()
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
    let upper = (store.id.0 + 1).to_be_bytes();
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

/// Replace a relation's triggers. Requires at least
/// [`AccessLevel::Protected`].
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
    original.put_triggers = puts.to_vec();
    original.rm_triggers = rms.to_vec();
    original.replace_triggers = replaces.to_vec();
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
            c.name.to_string()
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

// ---------------------------------------------------------------------------
// Tests: the catalog's executable law.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::cmp::Reverse;

    use super::*;
    use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
    use crate::data::tuple::decode_tuple_from_key;
    use crate::data::value::{Validity, ValidityTs};
    use crate::storage::fjall::new_fjall_storage;
    use crate::storage::{ConflictError, Storage};

    fn col(name: &str, coltype: ColType) -> ColumnDef {
        ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType {
                coltype,
                nullable: false,
            },
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
        assert_eq!(
            decode_tuple_from_key(&counter, 1).unwrap(),
            vec![DataValue::Null]
        );

        let rel = SystemKey::Relation("stored").encode();
        assert_eq!(&rel[..8], &[0u8; 8], "SYSTEM prefix");
        assert_eq!(
            decode_tuple_from_key(&rel, 1).unwrap(),
            vec![DataValue::from("stored")]
        );

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
        let row = vec![DataValue::from(1), DataValue::from("one")];
        let mut tx = db.write_tx().unwrap();
        a.put_fact(&mut tx, &row, ValidityTs(std::cmp::Reverse(0)), span)
            .unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        assert!(a.exists(&rtx, &row[..1]).unwrap());
        assert_eq!(a.get(&rtx, &row[..1]).unwrap(), Some(row.clone()));
        assert_eq!(
            a.get_val_only(&rtx, &row[..1]).unwrap(),
            Some(vec![DataValue::from("one")])
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
        rel.put_fact(&mut tx, &row, ValidityTs(std::cmp::Reverse(0)), span)
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
        assert_eq!(first.id, RelationId(1));
        assert_eq!(second.id, RelationId(2));

        // A later transaction continues where the counter left off.
        let mut tx = db.write_tx().unwrap();
        let third = create_relation(&mut tx, simple_input("three"), KeyspaceKind::Facts).unwrap();
        tx.commit().unwrap();
        assert_eq!(third.id, RelationId(3));

        let rtx = db.read_tx().unwrap();
        let counter = rtx.get(&SystemKey::IdCounter.encode()).unwrap().unwrap();
        assert_eq!(RelationId::raw_decode(&counter).unwrap(), RelationId(3));
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
            err.downcast_ref::<ConflictError>().is_some(),
            "the loser gets the typed, retryable conflict: {err:?}"
        );

        // The retry (a fresh transaction) sees the committed counter and
        // allocates the next id.
        let mut tx3 = db.write_tx().unwrap();
        let right = create_relation(&mut tx3, simple_input("right"), KeyspaceKind::Facts).unwrap();
        tx3.commit().unwrap();
        assert_eq!(right.id, RelationId(2));
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
        // Force the counter to the last allocatable id.
        tx.put(&SystemKey::IdCounter.encode(), &(1u64 << 48).to_be_bytes())
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
        let row = vec![DataValue::from(5), DataValue::from("five")];
        rel.put_fact(&mut tx, &row, ValidityTs(std::cmp::Reverse(0)), span)
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
        assert_eq!(renamed.get(&rtx, &row[..1]).unwrap(), Some(row));
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
        let slot = |ts: i64| {
            DataValue::Validity(Validity {
                timestamp: ValidityTs(Reverse(ts)),
                is_assert: Reverse(true),
            })
        };
        let vts_of = |ts: i64| ValidityTs(Reverse(ts));
        // k=1 asserted at valid t=10, retracted at valid t=20, through
        // the handle's own fact writers.
        let _ = slot;
        rel.put_fact(&mut tx, &[DataValue::from(1)], vts_of(10), SourceSpan(0, 0))
            .unwrap();
        rel.retract_fact(&mut tx, &[DataValue::from(1)], vts_of(20), SourceSpan(0, 0))
            .unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let at = |ts: i64| -> Vec<Tuple> {
            rel.skip_scan_all(&rtx, AsOf::current(ValidityTs(Reverse(ts))))
                .map(|t| t.unwrap())
                .collect()
        };
        assert_eq!(at(5).len(), 0, "before the assertion: not yet believed");
        let hits = at(15);
        assert_eq!(hits.len(), 1, "between assert and retract: believed");
        assert_eq!(hits[0][0], DataValue::from(1));
        assert_eq!(at(25).len(), 0, "after the retraction: revised away");
    }

    /// Index selection over by-reference plain indices: longest bound
    /// prefix wins, coverage decides the back-join, and the law-5 edges
    /// (empty argument list, stale mapper) degrade to "no index".
    #[test]
    fn choose_index_prefers_longest_prefix_and_survives_edges() {
        let mut handle = RelationHandle::new_from_input(
            input_handle(
                "base",
                vec![col("a", ColType::Int), col("b", ColType::Int)],
                vec![col("c", ColType::Int)],
            ),
            RelationId(7),
            false,
            KeyspaceKind::Facts,
        );
        handle.indices = vec![
            IndexRef {
                name: SmartString::from("by_b"),
                kind: IndexKind::Plain { mapper: vec![1, 0] },
            },
            IndexRef {
                name: SmartString::from("by_c_b"),
                kind: IndexKind::Plain { mapper: vec![2, 1] },
            },
        ];

        use IndexPositionUse::*;
        // First position bound: the base relation's own prefix serves.
        assert!(handle.choose_index(&[Join, Join, Ignored], false).is_none());
        // b and c bound: by_c_b matches a 2-long prefix and covers all
        // required positions — no back-join.
        let (chosen, back_join) = handle
            .choose_index(&[Ignored, Join, Join], false)
            .expect("an index applies");
        assert_eq!(chosen.name, "by_c_b");
        assert!(!back_join);
        // Only b bound, everything needed later: by_b wins its 1-long
        // prefix (by_c_b's mapper starts at unbound position 2) and a
        // back-join is required because position 2 is not covered by by_b.
        let (chosen, back_join) = handle
            .choose_index(&[BindForLater, Join, BindForLater], false)
            .expect("an index applies");
        assert_eq!(chosen.name, "by_b");
        assert!(back_join, "position 2 is not covered by by_b");
        // Law 5 edges: empty argument list, and a mapper pointing past the
        // argument list, both mean "no index", never a panic.
        assert!(handle.choose_index(&[], false).is_none());
        assert!(handle.choose_index(&[Ignored], false).is_none());
        // Validity queries demand the last key column terminate the mapper.
        let (chosen, _) = handle
            .choose_index(&[Ignored, Join, Join], true)
            .expect("by_c_b ends on the validity (last key) column");
        assert_eq!(chosen.name, "by_c_b");
    }

    /// The input-compatibility contract, including the ported fix: an
    /// input's *dependent* columns are type-checked (the original compared
    /// the stored relation's non-keys against themselves and let any input
    /// dependent type through).
    #[test]
    fn ensure_compatible_checks_input_dependents() {
        let stored = RelationHandle::new_from_input(
            simple_input("s"),
            RelationId(1),
            false,
            KeyspaceKind::Facts,
        );
        // Compatible input: same shapes.
        stored.ensure_compatible(&simple_input("s"), false).unwrap();
        // Incompatible dependent type: v as Int against stored String.
        let bad = input_handle(
            "s",
            vec![col("k", ColType::Int)],
            vec![col("v", ColType::Int)],
        );
        assert!(
            stored.ensure_compatible(&bad, false).is_err(),
            "fix-on-port: dependent column types are checked now"
        );
        // Removal/update: dependents need not be provided...
        let keys_only = input_handle("s", vec![col("k", ColType::Int)], vec![]);
        stored.ensure_compatible(&keys_only, true).unwrap();
        // ...but a full put requires them (or defaults).
        assert!(stored.ensure_compatible(&keys_only, false).is_err());
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
            RelationId(7),
            false,
            KeyspaceKind::Facts,
        );
        handle.put_triggers = vec!["?[k, v] := *pin[k, v]".to_string()];
        handle.access_level = AccessLevel::ReadOnly;
        handle.indices = vec![IndexRef {
            name: SmartString::from("by_v"),
            kind: IndexKind::Plain { mapper: vec![1, 0] },
        }];
        handle.description = SmartString::from("pinned");
        handle.constraints = vec![ConstraintRef {
            name: SmartString::from("no_empty_v"),
            source: "?[k] := *pin[k, v], v == ''".to_string(),
        }];

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
        let err = RelationHandle::decode(&bytes[..bytes.len() / 2]).unwrap_err();
        assert!(err.downcast_ref::<RelationDeserError>().is_some());
        assert!(RelationHandle::decode(b"garbage").is_err());
    }

    /// The pinned wire bytes of the canonical handle above (msgpack,
    /// struct maps; FormatVersion v2). Regenerate ONLY as part of a
    /// deliberate format migration — the version stamp refuses mismatched
    /// stores, so there is exactly one catalog format per FormatVersion
    /// and no cross-version decode path.
    const PINNED_HANDLE_HEX: &str = "8ca46e616d65a370696ea2696407a86d6574616461746182a46b6579739183a46e616d65a16ba6747970696e6782a7636f6c74797065a3496e74a86e756c6c61626c65c2ab64656661756c745f67656ec0a86e6f6e5f6b6579739183a46e616d65a176a6747970696e6782a7636f6c74797065a6537472696e67a86e756c6c61626c65c2ab64656661756c745f67656ec0ac7075745f747269676765727391b53f5b6b2c20765d203a3d202a70696e5b6b2c20765dab726d5f747269676765727390b07265706c6163655f747269676765727390ac6163636573735f6c6576656ca8526561644f6e6c79a769735f74656d70c2a7696e64696365739182a46e616d65a462795f76a46b696e6481a5506c61696e81a66d6170706572920100ab6465736372697074696f6ea670696e6e6564ab636f6e73747261696e74739182a46e616d65aa6e6f5f656d7074795f76a6736f75726365bb3f5b6b5d203a3d202a70696e5b6b2c20765d2c2076203d3d202727ad6b657973706163655f6b696e64a54661637473";
}
