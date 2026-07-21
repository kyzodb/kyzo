/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Leave-is-free packs + dump/restore interchange (decisions.md §64, §65, §79, §80).
//!
//! Owns: leave-is-free pack builder (seal+suffix+objects | full WAL+objects),
//! pack hygiene scrub point, import verify ceremony; plus the portable
//! length-prefixed dump format (`KYZODMP2`).
//!
//! Bans: WA / KEK / plaintext salt / AuditKey / MintCap in packs; packs
//! omitting [`WrappedShredSalt`] or [`IncarnationId`] history; green
//! incomplete restore (a crash-interrupted [`restore_storage`] leaves a
//! durable in-progress mark; [`admit_complete_store`] / [`open_complete_store`]
//! refuse rather than presenting a partial prefix as a smaller complete store).
//!
//! Dump format: 8-byte magic `KYZODMP2`, then for each pair a u64-BE key
//! length, the key bytes, a u64-BE value length, the value bytes. Pairs appear
//! in ascending key order (`total_scan` order). Restore applies them under a
//! durable in-progress mark (cleared only after the final sync).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::Path;

use miette::{Diagnostic, IntoDiagnostic, Result, bail, miette};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::session::catalog::{KeyspaceKind, list_relations};
use crate::store::authority::IncarnationId;
use crate::store::crypto::{ShredLedger, WrappedShredSalt};
use crate::store::merkle::{
    ChainLinkKind, GENESIS_ROOT, ReplicaCutRecompute, StateRoot, roots_equal_at_cut,
};
use crate::store::open::StoreId;
use crate::store::sweep::CommitOrdinal;
use crate::store::time::system_stamp_of_key;
use crate::store::fjall::FjallStorage;
use crate::store::fjall::{StorageOptions, new_fjall_storage, new_fjall_storage_with};
use crate::store::tx::WriteTx;
use crate::store::{FormatVersion, ReadTx, Storage};
use kyzo_model::value::ValidityTs;
use kyzo_model::value::{RelationId, StorageKey};

const MAGIC: &[u8; 8] = b"KYZODMP2";

/// Durable in-progress mark written before any dump pairs land, cleared only
/// after the final [`Storage::sync`] of a successful restore (seat 26 / #374 T11).
///
/// Non-empty reserved key (fjall/lsm-tree reject empty keys). The leading NUL
/// plus `kyzo.` namespace keeps it outside normal relation-prefixed dump pairs
/// (8-byte relation id prefix) so it never collides with restored data.
const RESTORE_IN_PROGRESS_KEY: &[u8] = b"\0kyzo.restore.in_progress.v1";
const RESTORE_IN_PROGRESS_VAL: &[u8] = b"kyzo.restore.in_progress.v1";

/// Typed refuse when a store still carries a restore-in-progress mark —
/// crash-interrupted or not yet cleared after the final sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error, Diagnostic)]
#[error(
    "IncompleteRestore: restore in progress or crash-interrupted; refusing to present as a complete store"
)]
#[diagnostic(code(store::backup::incomplete_restore))]
pub struct IncompleteRestore;

/// Chunk size for restore pair applies after the in-progress mark is durable.
/// Kept modest so a poisoned iterator in tests can interrupt mid-import without
/// buffering tens of thousands of pairs (batch_put cannot run once the mark
/// occupies the target).
const RESTORE_PUT_CHUNK: usize = 64;

/// A dumped fact row's system stamp exceeds the clock floor this dump
/// itself recorded — the exact shape of the historical lost-update bug
/// (see the module-level contract history, `storage/mod.rs`): restoring
/// this dump would let the target re-mint a stamp at or below a row
/// already in the imported history, silently shadowing it. Refuse the
/// dump outright rather than hand a restorer a file that lies about its
/// own floor.
#[derive(Debug, Error, Diagnostic)]
#[error(
    "dump invariant violated: relation {relation_id} row stamped {stamp} exceeds this \
     dump's recorded clock floor {floor} — refusing to write a dump that would let a \
     restore re-mint at or below an already-imported stamp"
)]
#[diagnostic(code(storage::dump_clock_floor_violation))]
pub struct DumpClockFloorViolation {
    relation_id: RelationId,
    stamp: i64,
    floor: i64,
}

/// This dump's clock floor. Takes `_snapshot` — the open read transaction
/// whose rows this dump is about to scan — by reference: there is no way
/// to call this before a snapshot exists, so the snapshot-then-floor
/// order the proof below depends on cannot be reordered by a future edit,
/// exactly like [`FjallStorage::stamp_after_snapshot`](crate::store::fjall::FjallStorage).
///
/// PROOF the recorded floor bounds every stamp `_snapshot` can show: a
/// write transaction mints its system stamp — and bumps the clock's
/// floor-tracking atomic to that value — strictly before its commit can
/// complete (`write_tx` mints right after opening its own snapshot, well
/// before any later call to `commit`). A row is visible in `_snapshot`
/// only if its writer's commit fully preceded `_snapshot`'s own creation;
/// so that writer's mint, and the atomic's bump to that writer's stamp,
/// also preceded `_snapshot`'s creation — and therefore preceded this
/// floor read, which happens strictly after. The floor only rises
/// (`raise_floor` is a `fetch_max`), so a floor read at any point after
/// the snapshot opens is `>=` every stamp that snapshot can show. (A floor
/// that ends up higher than strictly necessary is harmless: the restore
/// target just starts its own minting further ahead than it had to.)
///
/// Reading the floor BEFORE the snapshot broke this: a writer could mint
/// AND commit entirely between the two reads, landing a row the snapshot
/// (opened after) correctly includes while carrying a stamp the
/// already-read, now-stale floor never accounted for — the dump then
/// advertises a floor too low for its own contents, and a restore's
/// `raise_clock_floor` lets the target re-mint at or below that row's real
/// stamp: a silent collision.
fn floor_after_snapshot<S: Storage>(db: &S, _snapshot: &S::ReadTx) -> Result<ValidityTs> {
    db.clock_floor()
}

/// Every currently-cataloged relation's storage kind, keyed by relation
/// id — the backstop's map from "which relation does this row belong to"
/// to "does that relation's keyspace carry the bitemporal tail at all"
/// (`KeyspaceKind::Facts`) or not (`AlgorithmState`: exact-key,
/// current-only engine-index state with no time slots to check).
fn relation_kinds(tx: &impl ReadTx) -> Result<BTreeMap<RelationId, KeyspaceKind>> {
    Ok(list_relations(tx)?
        .into_iter()
        .map(|h| (h.id, h.keyspace_kind))
        .collect())
}

/// Peek a row's leading relation-id prefix without validating it as a
/// well-formed id in isolation: `dump_storage`'s documented job is dumping
/// the WHOLE store, including raw non-relation-shaped key-value data (see
/// `backup_round_trip`), so misreading a short or out-of-catalog-range key
/// as garbage must never abort the dump — only a prefix that matches a
/// cataloged `Facts` relation triggers the stamp check.
fn relation_prefix(key: &[u8]) -> Option<RelationId> {
    let bytes: [u8; 8] = key
        .get(0..StorageKey::RELATION_PREFIX_LEN)?
        .try_into()
        .ok()?;
    RelationId::new(u64::from_be_bytes(bytes))
}

/// The dump backstop: a `Facts` row's system stamp must be `<=` the floor
/// this dump is about to advertise. Cheap by construction (see
/// [`system_stamp_of_key`]'s no-allocation guarantee), so it runs on every
/// fact row unconditionally, not just under test — converting ANY future
/// reintroduction of the snapshot/floor race (or a genuine on-disk stamp
/// anomaly) from a silent lost-update into a loud, typed refusal.
fn verify_stamp_within_floor(id: RelationId, key: &[u8], floor: ValidityTs) -> Result<()> {
    let stamp = system_stamp_of_key(key)?;
    if stamp.raw() > floor.raw() {
        return Err(DumpClockFloorViolation {
            relation_id: id,
            stamp: stamp.raw(),
            floor: floor.raw(),
        }
        .into());
    }
    Ok(())
}

/// Dump every key-value pair of the storage to the file at `path`.
pub fn dump_storage<S: Storage>(db: &S, path: impl AsRef<Path>) -> Result<()> {
    // An in-progress / interrupted restore is not a complete store — refuse
    // to costume it as dump source material.
    admit_complete_store(db)?;
    let file = File::create(path).into_diagnostic()?;
    let mut w = BufWriter::new(file);
    w.write_all(MAGIC).into_diagnostic()?;
    // The dump carries the store's on-disk format version: a dump of one
    // format can never silently restore into a store of another.
    let version = FormatVersion::CURRENT.as_bytes();
    w.write_all(&(version.len() as u64).to_be_bytes())
        .into_diagnostic()?;
    w.write_all(&version).into_diagnostic()?;
    // SNAPSHOT FIRST, FLOOR SECOND — see `floor_after_snapshot`'s doc
    // comment for the full proof. Reversing this order (floor, then
    // snapshot) is the historical bug: a writer landing between the two
    // reads could commit a row the snapshot includes while carrying a
    // stamp the already-read floor never accounted for.
    let tx = db.read_tx()?;
    // The dump carries the source's system-clock floor: system stamps in
    // the dumped history must never be re-mintable by the restore
    // target, or new writes could land AT or BELOW imported instants and
    // be shadowed by history.
    let floor = floor_after_snapshot(db, &tx)?;
    w.write_all(&floor.raw().to_be_bytes()).into_diagnostic()?;
    let kinds = relation_kinds(&tx)?;
    for pair in tx.total_scan() {
        let (k, v) = pair?;
        // Never emit the restore-in-progress mark into a dump.
        if &*k == RESTORE_IN_PROGRESS_KEY {
            continue;
        }
        if let Some(id) = relation_prefix(&k)
            && kinds.get(&id) == Some(&KeyspaceKind::Facts)
        {
            verify_stamp_within_floor(id, &k, floor)?;
        }
        w.write_all(&(k.len() as u64).to_be_bytes())
            .into_diagnostic()?;
        w.write_all(&k).into_diagnostic()?;
        w.write_all(&(v.len() as u64).to_be_bytes())
            .into_diagnostic()?;
        w.write_all(&v).into_diagnostic()?;
    }
    w.flush().into_diagnostic()?;
    Ok(())
}

/// Refuse when `db` still carries a restore-in-progress mark.
///
/// This is the completeness gate: a crash-interrupted restore reopened by a
/// plain admit/open must not be presented as a smaller complete store.
pub fn admit_complete_store<S: Storage>(db: &S) -> Result<()> {
    let tx = db.read_tx()?;
    if tx.exists(RESTORE_IN_PROGRESS_KEY)? {
        return Err(IncompleteRestore.into());
    }
    Ok(())
}

/// Open a fjall store and refuse if a restore-in-progress mark is present.
///
/// Prefer this over bare [`new_fjall_storage`] when admitting a path that may
/// have been a restore target — bare open alone cannot see the mark's meaning.
pub fn open_complete_store(path: impl AsRef<Path>) -> Result<FjallStorage> {
    open_complete_store_with(path, StorageOptions::default())
}

/// Open a fjall store with resource options and refuse if a restore-in-progress
/// mark is present — the production host open door (kyzo-bin `engine::open`).
pub fn open_complete_store_with(
    path: impl AsRef<Path>,
    opts: StorageOptions,
) -> Result<FjallStorage> {
    let db = new_fjall_storage_with(path, opts)?;
    admit_complete_store(&db)?;
    Ok(db)
}

/// Host open sites: expand to the crate-root [`admit_complete_store`] re-export
/// so production callers outside this `pub(crate)` module share the completeness
/// gate (seat 26 / #375 T2). Macros must not name `$crate::store::…` — `store`
/// is `pub(crate)` and is invisible at expansion sites in kyzo-bin.
///
/// `$crate` resolves inside kyzo-core, so the gate stays one function — not a
/// duplicated mark-key check in the binary.
#[macro_export]
macro_rules! admit_complete_store {
    ($storage:expr) => {
        $crate::admit_complete_store(&$storage)
    };
}

/// Host open sites: expand to the crate-root [`open_complete_store_with`] re-export.
#[macro_export]
macro_rules! open_complete_store_with {
    ($path:expr, $opts:expr) => {
        $crate::open_complete_store_with($path, $opts)
    };
}

/// Typed IncompleteRestore probe for host nasty tests (crate-root path law).
#[macro_export]
macro_rules! is_incomplete_restore {
    ($err:expr) => {
        ($err)
            .downcast_ref::<$crate::IncompleteRestore>()
            .is_some()
    };
}

fn mark_restore_in_progress<S: Storage>(db: &S) -> Result<()> {
    let mut tx = db.write_tx()?;
    tx.put(RESTORE_IN_PROGRESS_KEY, RESTORE_IN_PROGRESS_VAL)?;
    tx.commit()?;
    Ok(())
}

fn clear_restore_in_progress<S: Storage>(db: &S) -> Result<()> {
    let mut tx = db.write_tx()?;
    tx.del(RESTORE_IN_PROGRESS_KEY)?;
    tx.commit()?;
    Ok(())
}

/// Apply dump pairs after the in-progress mark is already durable.
///
/// Not [`Storage::batch_put`]: that door refuses non-empty targets, and the
/// mark must land *before* any pair so a mid-import crash stays distinguishable.
fn put_restore_pairs<'a, S: Storage>(
    db: &'a S,
    data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
) -> Result<()> {
    let mut data = data.peekable();
    while data.peek().is_some() {
        let mut tx = db.write_tx()?;
        for pair in data.by_ref().take(RESTORE_PUT_CHUNK) {
            let (k, v) = match pair {
                Ok(kv) => kv,
                Err(e) => {
                    let _ = tx.abort();
                    return Err(e);
                }
            };
            if let Err(e) = tx.put(&k, &v) {
                let _ = tx.abort();
                return Err(e);
            }
        }
        tx.commit()?;
    }
    Ok(())
}

/// Restore a dump produced by [`dump_storage`] into the storage.
///
/// This door restores a **KYZODMP2** portable dump only. Foreign leave-is-free
/// packs are not admitted here — that path is [`import_leave_is_free`] under
/// [`ImportCapability`] + chain/root verify (seat 80 / #359). Blind leave-is-free
/// import without the ceremony is Unconstructible on this door.
///
/// The target must be **empty** and must not be accessed concurrently. Before
/// any dump pair is applied, a durable in-progress mark is synced; that mark
/// is cleared only after the restored pairs are synced. A crash-interrupted
/// restore therefore reopens as incomplete ([`IncompleteRestore`] via
/// [`admit_complete_store`] / [`open_complete_store`]) — never as a silent
/// smaller complete store. Recovery is discard-and-re-run; a partial restore
/// never merges into existing data.
pub fn restore_storage<S: Storage>(db: &S, path: impl AsRef<Path>) -> Result<()> {
    {
        let tx = db.read_tx()?;
        if tx.exists(RESTORE_IN_PROGRESS_KEY)? {
            return Err(IncompleteRestore.into());
        }
        if tx.total_scan().next().is_some() {
            bail!("restore target is not empty: restore only into a fresh store");
        }
    }
    let file = File::open(path).into_diagnostic()?;
    let mut r = BufReader::new(file);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).into_diagnostic()?;
    if &magic != MAGIC {
        bail!("not a KyzoDB dump file: bad magic");
    }
    let Some((version, _)) = read_len_prefixed(&mut r)? else {
        bail!("truncated dump: missing format version");
    };
    let found = FormatVersion::parse(&version)?;
    if found != FormatVersion::CURRENT {
        bail!(
            "dump format version mismatch: dump is {found}, this build reads {}",
            FormatVersion::CURRENT,
        );
    }
    // Raise the target's clock floor to the source's BEFORE importing:
    // the target must never mint a stamp at or below any instant in the
    // imported history, or new writes could be shadowed by it.
    let mut floor_bytes = [0u8; 8];
    r.read_exact(&mut floor_bytes)
        .map_err(|_| miette!("truncated dump: missing clock floor"))?;
    db.raise_clock_floor(kyzo_model::value::ValidityTs::from_raw(i64::from_be_bytes(
        floor_bytes,
    )))?;
    // Mark durable *before* any dump pair — crash after this point leaves a
    // store that admit_complete_store refuses, not a costume-complete prefix.
    mark_restore_in_progress(db)?;
    db.sync()?;
    let iter = std::iter::from_fn(move || read_pair(&mut r).transpose());
    put_restore_pairs(db, Box::new(iter))?;
    // Pairs applied; sync before clearing the mark so a crash between apply
    // and clear still reopens as incomplete (conservative; discard-and-re-run).
    db.sync()?;
    clear_restore_in_progress(db)?;
    db.sync()
}

/// Test / harness door: restore from an already-decoded pair iterator with the
/// same in-progress mark law as [`restore_storage`]. Used to inject a mid-put
/// interrupt without corrupting a dump file.
#[cfg(test)]
fn restore_pairs_for_test<'a, S: Storage>(
    db: &'a S,
    data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
) -> Result<()> {
    {
        let tx = db.read_tx()?;
        if tx.exists(RESTORE_IN_PROGRESS_KEY)? {
            return Err(IncompleteRestore.into());
        }
        if tx.total_scan().next().is_some() {
            bail!("restore target is not empty: restore only into a fresh store");
        }
    }
    mark_restore_in_progress(db)?;
    db.sync()?;
    put_restore_pairs(db, data)?;
    db.sync()?;
    clear_restore_in_progress(db)?;
    db.sync()
}

/// Read one length-prefixed field. Incremental (`take` + `read_to_end`), so
/// a corrupt length prefix yields a truncation error — never a giant
/// pre-allocation that aborts the process. Returns Ok(None) on clean EOF at
/// the prefix boundary.
fn read_len_prefixed(r: &mut impl Read) -> Result<Option<(Vec<u8>, u64)>> {
    let mut len_buf = [0u8; 8];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(miette!("reading dump: {e}")),
    }
    let len = u64::from_be_bytes(len_buf);
    let mut buf = Vec::new();
    r.take(len).read_to_end(&mut buf).into_diagnostic()?;
    if buf.len() as u64 != len {
        bail!("truncated dump: field shorter than its length prefix");
    }
    Ok(Some((buf, len)))
}

fn read_pair(r: &mut impl Read) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let Some((k, _)) = read_len_prefixed(r)? else {
        return Ok(None);
    };
    let Some((v, _)) = read_len_prefixed(r)? else {
        bail!("truncated dump: key without a value");
    };
    Ok(Some((k, v)))
}

// ── Leave-is-free pack (§79 / §65) ──────────────────────────────────────────

/// Leave-is-free pack shape: seal+suffix+objects, or full WAL+objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaveIsFreeKind {
    /// CheckpointSeal + retained WAL suffix + retained objects.
    SealAndSuffix,
    /// Full WAL + retained objects at the cut.
    FullWal,
}

/// Inputs required to build a leave-is-free pack. Omitting wrapped salts or
/// incarnation history is Unconstructible as leave-is-free.
#[derive(Debug, Clone)]
pub struct LeaveIsFreeParts {
    /// Pack shape.
    pub kind: LeaveIsFreeKind,
    /// FormatVersion stamped into the pack.
    pub format_version: FormatVersion,
    /// Wrapped shred salts for every retained encrypted segment (§64/§65).
    pub wrapped_shred_salts: Vec<WrappedShredSalt>,
    /// IncarnationId history required for restore verification (§62/§65).
    pub incarnation_history: Vec<IncarnationId>,
    /// Opaque retained object / WAL / seal payload bytes (adapter currency).
    pub payload: Vec<u8>,
}

/// Sealed leave-is-free pack. WriteAuthority / KEK / plaintext ShredSalt /
/// AuditKey / IncarnationMintCap are absent by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveIsFreePack {
    kind: LeaveIsFreeKind,
    format_version: FormatVersion,
    wrapped_shred_salts: Vec<WrappedShredSalt>,
    incarnation_history: Vec<IncarnationId>,
    payload: Vec<u8>,
}

/// Typed refuse from pack build / hygiene / import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum PackRefuse {
    #[error("leave-is-free pack missing WrappedShredSalt for retained encrypted segments")]
    #[diagnostic(code(store::backup::missing_wrapped_shred_salt))]
    MissingWrappedShredSalt,
    #[error("leave-is-free pack missing IncarnationId history")]
    #[diagnostic(code(store::backup::missing_incarnation_history))]
    MissingIncarnationHistory,
    #[error("pack hygiene: forbidden secret material at scrub point")]
    #[diagnostic(code(store::backup::hygiene_secret))]
    HygieneSecretMaterial,
    #[error("ForeignHistoryUnverified: blind import refused")]
    #[diagnostic(code(store::backup::foreign_unverified))]
    ForeignHistoryUnverified,
    #[error("import ceremony: incomplete restore refused (never green-incomplete)")]
    #[diagnostic(code(store::backup::incomplete_restore))]
    IncompleteRestore,
    #[error("post-shred restore of shredded segment")]
    #[diagnostic(code(store::backup::shredded))]
    Shredded,
    /// Origin StoreId already has a sealed trusted root; a different root
    /// cannot overwrite it.
    ///
    /// First registration wins. Same root re-register is idempotent Ok on
    /// [`OriginRootRegistry::insert`]. Rotation is a separate explicit verb
    /// — never silent overwrite via insert.
    #[error(
        "TrustRootAlreadySealed: StoreId {store_id:?} already has a sealed origin trust root"
    )]
    #[diagnostic(code(store::backup::trust_root_already_sealed))]
    TrustRootAlreadySealed { store_id: StoreId },
}

impl LeaveIsFreePack {
    /// Build a leave-is-free pack. Requires non-empty WrappedShredSalt list and
    /// IncarnationId history — omitting either is Unconstructible as leave-is-free.
    pub fn build(parts: LeaveIsFreeParts) -> Result<Self, PackRefuse> {
        if parts.wrapped_shred_salts.is_empty() {
            return Err(PackRefuse::MissingWrappedShredSalt);
        }
        if parts.incarnation_history.is_empty() {
            return Err(PackRefuse::MissingIncarnationHistory);
        }
        let pack = Self {
            kind: parts.kind,
            format_version: parts.format_version,
            wrapped_shred_salts: parts.wrapped_shred_salts,
            incarnation_history: parts.incarnation_history,
            payload: parts.payload,
        };
        pack_hygiene_scrub(&pack)?;
        Ok(pack)
    }

    /// Pack shape.
    pub fn kind(&self) -> LeaveIsFreeKind {
        self.kind
    }

    /// FormatVersion stamped into the pack.
    pub fn format_version(&self) -> FormatVersion {
        self.format_version
    }

    /// Wrapped shred salts included in the pack (required restore inputs).
    pub fn wrapped_shred_salts(&self) -> &[WrappedShredSalt] {
        &self.wrapped_shred_salts
    }

    /// IncarnationId history included in the pack.
    pub fn incarnation_history(&self) -> &[IncarnationId] {
        &self.incarnation_history
    }

    /// Opaque retained payload.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Deterministic content digest of this pack's sealed bytes/fields.
    ///
    /// Hashes kind, format_version, wrapped salts, incarnation history, and
    /// payload under a domain separator — never a caller-supplied root.
    fn pack_content_root(&self) -> StateRoot {
        let mut h = Sha256::new();
        h.update(b"kyzo.leave_is_free.pack.root.v1");
        h.update(match self.kind {
            LeaveIsFreeKind::SealAndSuffix => b"seal_and_suffix".as_slice(),
            LeaveIsFreeKind::FullWal => b"full_wal".as_slice(),
        });
        h.update(self.format_version.as_bytes());
        h.update(u64::to_be_bytes(self.wrapped_shred_salts.len() as u64));
        for wrapped in &self.wrapped_shred_salts {
            let domain = wrapped.crypto_domain();
            h.update(domain.store_id().as_bytes());
            h.update(u64::to_be_bytes(domain.fence_epoch().get()));
            h.update(u64::to_be_bytes(wrapped.segment().get()));
            h.update(u64::to_be_bytes(wrapped.ciphertext().len() as u64));
            h.update(wrapped.ciphertext());
        }
        h.update(u64::to_be_bytes(self.incarnation_history.len() as u64));
        for incarnation in &self.incarnation_history {
            h.update(u64::to_be_bytes(incarnation.open_ordinal().get()));
            h.update(incarnation.entropy().as_bytes());
        }
        h.update(u64::to_be_bytes(self.payload.len() as u64));
        h.update(&self.payload);
        let dig = h.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&dig);
        StateRoot::from_digest(out)
    }

    /// Claimed origin [`StoreId`] carried by this pack (first wrapped salt domain).
    ///
    /// Ceremony trust is never taken from this claim alone — [`OriginRootRegistry`]
    /// must already hold a trusted root for this id (seat 80 / #374 T7).
    pub fn claimed_origin_store_id(&self) -> StoreId {
        self.wrapped_shred_salts
            .first()
            .expect("LeaveIsFreePack: non-empty wrapped_shred_salts by build invariant")
            .crypto_domain()
            .store_id()
    }

    /// Independent [`ReplicaCutRecompute`] derived solely from this pack's
    /// sealed fields — never from a peer-delivered root (seat 80).
    ///
    /// Crate-visible recompute only — unusable as the ceremony `local` anchor
    /// ([`ImportCapability::after_chain_root_verify`] always refuses bare cuts).
    /// Mint via [`OriginRootRegistry::after_chain_root_verify`].
    pub(crate) fn replica_cut_recompute(&self) -> ReplicaCutRecompute {
        let content = self.pack_content_root();
        let store_id = self.claimed_origin_store_id();
        let fence = self
            .wrapped_shred_salts
            .first()
            .map(|w| w.crypto_domain().fence_epoch())
            .unwrap_or_else(|| crate::store::epoch::FenceEpoch::genesis(store_id));
        // Leave-is-free cut ordinal is the first dense successor of ZERO —
        // pack identity is content-bound; ordinal is cut protocol, not history length.
        let ordinal = CommitOrdinal::ZERO
            .successor()
            .expect("CommitOrdinal::ZERO always has a successor");
        ReplicaCutRecompute::from_local(
            store_id,
            fence,
            ordinal,
            content,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        )
    }

    /// Independently recompute this pack's chain root at the leave-is-free cut.
    ///
    /// [`import_verify`] gates admission on `observed == capability.bound_root()`.
    pub fn recompute_root(&self) -> StateRoot {
        self.replica_cut_recompute().recompute()
    }
}

/// Receiver-held sealed registry: origin [`StoreId`] → trusted chain root at a
/// known cut (seat 80 / #374 T7).
///
/// Established out-of-band when the receiver first accepts that origin. The
/// leave-is-free PACK never supplies the trust root — ceremony `local` resolves
/// here only.
///
/// [`Self::insert`] is operator/genesis-scoped: untrusted or peer input must
/// never reach it unvalidated. Seal-once per StoreId — first root wins;
/// same root re-register is idempotent; a different root refuses
/// [`PackRefuse::TrustRootAlreadySealed`] (rotation is a separate explicit
/// verb, not insert).
#[derive(Debug, Default, Clone)]
pub struct OriginRootRegistry {
    /// origin StoreId bytes → trusted chain root at the known cut.
    roots: BTreeMap<[u8; 32], StateRoot>,
}

impl OriginRootRegistry {
    /// Empty sealed origin-root registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the trusted chain root for an origin StoreId (operator/genesis door).
    ///
    /// Seal-once per StoreId: first root wins; same root → idempotent Ok;
    /// different root → [`PackRefuse::TrustRootAlreadySealed`] (never silent
    /// overwrite). Untrusted/peer input must not reach this door unvalidated.
    pub fn insert(
        &mut self,
        origin: StoreId,
        trusted_root: StateRoot,
    ) -> Result<(), PackRefuse> {
        match self.roots.get(origin.as_bytes()) {
            None => {
                self.roots.insert(*origin.as_bytes(), trusted_root);
                Ok(())
            }
            Some(existing) if *existing == trusted_root => Ok(()),
            Some(_) => Err(PackRefuse::TrustRootAlreadySealed { store_id: origin }),
        }
    }

    /// Lookup the sealed trusted chain root for `origin`, if registered.
    pub fn get(&self, origin: StoreId) -> Option<StateRoot> {
        self.roots.get(origin.as_bytes()).copied()
    }

    /// Import ceremony: `local` from this registry for the pack's claimed origin;
    /// `peer` is the pack's independently recomputed root; require equivalence.
    ///
    /// - origin not registered → [`PackRefuse::ForeignHistoryUnverified`]
    /// - pack root ≠ registered trusted root → [`PackRefuse::ForeignHistoryUnverified`]
    pub fn after_chain_root_verify(
        &self,
        pack: &LeaveIsFreePack,
    ) -> Result<ImportCapability, PackRefuse> {
        let origin = pack.claimed_origin_store_id();
        let Some(trusted) = self.get(origin) else {
            return Err(PackRefuse::ForeignHistoryUnverified);
        };
        // peer: pack's own independently recomputed root (never the local anchor).
        let peer = pack.recompute_root();
        if !roots_equal_at_cut(trusted, peer) {
            return Err(PackRefuse::ForeignHistoryUnverified);
        }
        Ok(ImportCapability {
            bound_root: Some(trusted),
        })
    }
}

/// Forbidden secret markers scrubbed from leave-is-free payload bytes (§65).
const HYGIENE_FORBIDDEN_MARKERS: &[&[u8]] = &[
    b"kyzo.write_authority.",
    b"kyzo.kek.",
    b"kyzo.shred_salt.plaintext.",
    b"kyzo.audit_key.",
    b"kyzo.incarnation_mint_cap.",
];

/// Pack hygiene scrub point (§65): Store/Engine bundle emit and leave-is-free
/// boundaries. WA / KEK / plaintext salt / AuditKey / MintCap presence after
/// this point is a Spec violation — those types have no field on the pack, and
/// payload bytes are scanned for their domain markers.
fn pack_hygiene_scrub(pack: &LeaveIsFreePack) -> Result<(), PackRefuse> {
    if pack.wrapped_shred_salts.is_empty() || pack.incarnation_history.is_empty() {
        return Err(PackRefuse::HygieneSecretMaterial);
    }
    for marker in HYGIENE_FORBIDDEN_MARKERS {
        if contains_slice(pack.payload.as_slice(), marker) {
            return Err(PackRefuse::HygieneSecretMaterial);
        }
    }
    Ok(())
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Import verify ceremony (§80): foreign dumps only under capability + chain /
/// root verify. Blind import is a second write door for forged belief.
///
/// Sealed struct — never a public enum variant or bool standing in for verify
/// authority. A verified capability is reachable only through
/// [`OriginRootRegistry::after_chain_root_verify`], which binds the receiver's
/// sealed trusted origin root (never the pack's self-computed cut, never two
/// bare caller-supplied [`StateRoot`]s). Ambient / silent verified is
/// Unconstructible. Verified without a bound root is Unconstructible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImportCapability {
    /// Trusted origin root bound at mint. `None` = unverified.
    bound_root: Option<StateRoot>,
}

impl ImportCapability {
    /// Two bare [`ReplicaCutRecompute`]s are never a trust source (seat 80 / #374 T7).
    ///
    /// Self-anchoring a pack by comparing its own cut to itself is
    /// Unconstructible as Verified — always [`PackRefuse::ForeignHistoryUnverified`].
    /// The ceremony door is [`OriginRootRegistry::after_chain_root_verify`].
    pub fn after_chain_root_verify(
        local: ReplicaCutRecompute,
        peer: ReplicaCutRecompute,
    ) -> Result<Self, PackRefuse> {
        let _ = (local, peer);
        Err(PackRefuse::ForeignHistoryUnverified)
    }

    /// Unverified foreign import — [`import_verify`] refuses
    /// [`PackRefuse::ForeignHistoryUnverified`]. Constructible without a bound
    /// root; verified without a bound root is Unconstructible.
    pub fn unverified() -> Self {
        Self { bound_root: None }
    }

    /// Whether this capability was minted by chain/root verify.
    pub fn is_verified(self) -> bool {
        self.bound_root.is_some()
    }

    /// Root bound at mint — `None` when unverified.
    pub fn bound_root(self) -> Option<StateRoot> {
        self.bound_root
    }
}

/// Whether retained objects named by the cut are present for restore (§79/§80).
///
/// Closed sum — green-incomplete restore is Unconstructible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectsCompleteness {
    /// Every retained object named by the cut is present.
    Complete,
    /// Objects missing — import refuses IncompleteRestore.
    Incomplete,
}

/// Run the import verify ceremony over a leave-is-free pack.
///
/// Requires [`ImportCapability`] minted by [`OriginRootRegistry::after_chain_root_verify`]
/// and bound to this pack's independently recomputed root; unverified, forged,
/// or unbound-to-pack → [`PackRefuse::ForeignHistoryUnverified`]. [`ShredLedger`]
/// is consulted so post-shred restore of a pack that still carries a shredded
/// segment's `WrappedShredSalt` converges to [`PackRefuse::Shredded`] (§64 / §80)
/// — not silent unreadability.
pub fn import_verify(
    pack: &LeaveIsFreePack,
    cap: ImportCapability,
    objects: ObjectsCompleteness,
    shred_ledger: &ShredLedger,
) -> Result<(), PackRefuse> {
    let Some(bound) = cap.bound_root() else {
        return Err(PackRefuse::ForeignHistoryUnverified);
    };
    let observed = pack.recompute_root();
    if !roots_equal_at_cut(observed, bound) {
        return Err(PackRefuse::ForeignHistoryUnverified);
    }
    if pack.wrapped_shred_salts.is_empty() {
        return Err(PackRefuse::MissingWrappedShredSalt);
    }
    if pack.incarnation_history.is_empty() {
        return Err(PackRefuse::MissingIncarnationHistory);
    }
    if matches!(objects, ObjectsCompleteness::Incomplete) {
        return Err(PackRefuse::IncompleteRestore);
    }
    for wrapped in &pack.wrapped_shred_salts {
        if shred_ledger.is_shredded(wrapped) {
            return Err(PackRefuse::Shredded);
        }
    }
    pack_hygiene_scrub(pack)
}

/// Production leave-is-free import door (seat 80 / #359).
///
/// Runs [`import_verify`] then admits the pack under the verified ceremony.
/// There is no blind side door — unverified / unbound capability refuses
/// [`PackRefuse::ForeignHistoryUnverified`]. Payload materialization stays
/// adapter-side; the ceremony gate is the Store production path.
pub fn import_leave_is_free(
    pack: &LeaveIsFreePack,
    cap: ImportCapability,
    objects: ObjectsCompleteness,
    shred_ledger: &ShredLedger,
) -> Result<(), PackRefuse> {
    import_verify(pack, cap, objects, shred_ledger)
}

#[cfg(test)]
mod pins {
    /// Backup floor law pins (re-homed from storage/tests.rs).
    use kyzo_model::TupleT;
    use kyzo_model::value::{DataValue, RelationId, StorageKey, Tuple, ValiditySlot, ValidityTs};

    use crate::session::access::AccessLevel;
    use crate::session::catalog::{KeyspaceKind, RelationHandle, SystemKey};
    use crate::store::backup::{DumpClockFloorViolation, dump_storage};
    use crate::store::fjall::new_fjall_storage;
    use crate::store::time::ClaimPolarity;
    use crate::store::{Storage, WriteTx};
    use kyzo_model::schema::StoredRelationMetadata;

    fn facts_handle(id: RelationId, name: &str) -> RelationHandle {
        use smartstring::{LazyCompact, SmartString};
        RelationHandle {
            name: SmartString::<LazyCompact>::from(name),
            id,
            metadata: StoredRelationMetadata {
                keys: vec![],
                non_keys: vec![],
            },
            put_triggers: vec![],
            rm_triggers: vec![],
            replace_triggers: vec![],
            access_level: AccessLevel::default(),
            indices: vec![],
            description: SmartString::default(),
            constraints: vec![],
            keyspace_kind: KeyspaceKind::Facts,
        }
    }

    fn stamped_row(
        rel: RelationId,
        name: &str,
        valid_ts: i64,
        sys: ValidityTs,
    ) -> (StorageKey, Vec<u8>) {
        let slot = |ts: ValidityTs| DataValue::Validity(ValiditySlot::from_stored(ts, true));
        let tuple: Tuple = Tuple::from_vec(vec![
            DataValue::Str(name.into()),
            slot(ValidityTs::from_raw(valid_ts)),
            slot(sys),
        ]);
        (
            tuple.encode_as_key(rel),
            vec![ClaimPolarity::Assert.encode()],
        )
    }

    #[test]
    fn dump_refuses_a_row_stamped_above_its_own_floor() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rel = RelationId::new(100).expect("below cap");
        let handle = facts_handle(rel, "floor_test");
        let bad_sys =
            ValidityTs::from_raw(crate::session::current_validity().unwrap().raw() + 1_000_000_000);
        let (key, val) = stamped_row(rel, "evil", 1, bad_sys);
        let mut tx = db.write_tx().unwrap();
        tx.put(
            &SystemKey::Relation("floor_test").encode(),
            &handle.encode().unwrap(),
        )
        .unwrap();
        tx.put(&key, &val).unwrap();
        tx.commit().unwrap();
        let dump = dir.path().join("dump.kyzo");
        let err = dump_storage(&db, &dump).unwrap_err();
        assert!(
            err.downcast_ref::<DumpClockFloorViolation>().is_some(),
            "expected a typed DumpClockFloorViolation, got: {err}"
        );
    }

    #[test]
    fn leave_is_free_pack_requires_wrapped_shred_salt() {
        use crate::store::FormatVersion;
        use crate::store::authority::{Entropy, IncarnationMintCap, OpenOrdinal};
        use crate::store::backup::{
            ImportCapability, LeaveIsFreeKind, LeaveIsFreePack, LeaveIsFreeParts,
            ObjectsCompleteness, OriginRootRegistry, PackRefuse, import_verify,
        };
        use crate::store::crypto::{
            Kek, KekUnwrapCap, SegmentCounter, ShredLedger, ShredSalt, shred, wrap_shred_salt,
        };
        use crate::store::epoch::{CryptoDomain, FenceEpoch};
        use crate::store::open::StoreId;

        let store = StoreId::from_digest([0xCD; 32]);
        let domain = CryptoDomain::new(store, FenceEpoch::genesis(store));
        let cap = KekUnwrapCap::from_kek(Kek::from_bytes([0x55; 32]));
        let wrapped = wrap_shred_salt(
            &cap,
            &ShredSalt::from_bytes([0x66; 32]),
            SegmentCounter::ZERO,
            domain,
        )
        .expect("wrap");
        let mint = IncarnationMintCap::issue(store, OpenOrdinal::ZERO);
        let incarnation = mint.mint(Entropy::from_bytes([0x77; 32])).unwrap();

        let missing_salt = LeaveIsFreePack::build(LeaveIsFreeParts {
            kind: LeaveIsFreeKind::SealAndSuffix,
            format_version: FormatVersion::CURRENT,
            wrapped_shred_salts: vec![],
            incarnation_history: vec![incarnation],
            payload: vec![1, 2, 3],
        });
        assert!(matches!(
            missing_salt,
            Err(PackRefuse::MissingWrappedShredSalt)
        ));

        let pack = LeaveIsFreePack::build(LeaveIsFreeParts {
            kind: LeaveIsFreeKind::FullWal,
            format_version: FormatVersion::CURRENT,
            wrapped_shred_salts: vec![wrapped.clone()],
            incarnation_history: vec![incarnation],
            payload: vec![1, 2, 3],
        })
        .expect("pack with WrappedShredSalt + IncarnationId");
        assert!(!pack.wrapped_shred_salts().is_empty());
        let empty_ledger = ShredLedger::new();
        let mut registry = OriginRootRegistry::new();
        registry
            .insert(pack.claimed_origin_store_id(), pack.recompute_root())
            .expect("first origin-root registration seals");
        let verified = registry
            .after_chain_root_verify(&pack)
            .expect("trusted-origin ceremony");
        assert!(
            import_verify(
                &pack,
                verified,
                ObjectsCompleteness::Complete,
                &empty_ledger
            )
            .is_ok()
        );
        assert!(matches!(
            import_verify(
                &pack,
                ImportCapability::unverified(),
                ObjectsCompleteness::Complete,
                &empty_ledger
            ),
            Err(PackRefuse::ForeignHistoryUnverified)
        ));

        // Post-shred restore of a pack that still carries the wrap → Shredded.
        let (_receipt, tombstone) = shred(wrapped);
        let mut shredded = ShredLedger::new();
        shredded.record(tombstone);
        assert!(matches!(
            import_verify(
                &pack,
                verified,
                ObjectsCompleteness::Complete,
                &shredded
            ),
            Err(PackRefuse::Shredded)
        ));
    }
}

/// Seat 80 — foreign-dump import verify ceremony.
///
/// Board Check filters `backup::import_verify`: capability + trusted-origin
/// chain/root verify, forged/unanchored → [`PackRefuse::ForeignHistoryUnverified`],
/// silent import Unconstructible (no free Verified mint from pack self-cut).
#[cfg(test)]
mod import_verify {
    use super::{
        ImportCapability, LeaveIsFreeKind, LeaveIsFreePack, LeaveIsFreeParts, ObjectsCompleteness,
        OriginRootRegistry, PackRefuse, import_leave_is_free, import_verify,
    };
    use crate::store::FormatVersion;
    use crate::store::authority::{Entropy, IncarnationMintCap, OpenOrdinal};
    use crate::store::crypto::{
        Kek, KekUnwrapCap, SegmentCounter, ShredLedger, ShredSalt, shred, wrap_shred_salt,
    };
    use crate::store::epoch::{CryptoDomain, FenceEpoch};
    use crate::store::merkle::{
        ChainLinkKind, GENESIS_ROOT, ReplicaCutRecompute, StateRoot,
    };
    use crate::store::open::StoreId;
    use crate::store::sweep::CommitOrdinal;

    fn sample_pack() -> (LeaveIsFreePack, crate::store::crypto::WrappedShredSalt) {
        let store = StoreId::from_digest([0x80; 32]);
        let domain = CryptoDomain::new(store, FenceEpoch::genesis(store));
        let cap = KekUnwrapCap::from_kek(Kek::from_bytes([0x81; 32]));
        let wrapped = wrap_shred_salt(
            &cap,
            &ShredSalt::from_bytes([0x82; 32]),
            SegmentCounter::ZERO,
            domain,
        )
        .expect("wrap");
        let mint = IncarnationMintCap::issue(store, OpenOrdinal::ZERO);
        let incarnation = mint.mint(Entropy::from_bytes([0x83; 32])).unwrap();
        let pack = LeaveIsFreePack::build(LeaveIsFreeParts {
            kind: LeaveIsFreeKind::SealAndSuffix,
            format_version: FormatVersion::CURRENT,
            wrapped_shred_salts: vec![wrapped.clone()],
            incarnation_history: vec![incarnation],
            payload: b"leave-is-free-payload".to_vec(),
        })
        .expect("pack");
        (pack, wrapped)
    }

    fn registry_trusting(pack: &LeaveIsFreePack) -> OriginRootRegistry {
        let mut registry = OriginRootRegistry::new();
        registry
            .insert(pack.claimed_origin_store_id(), pack.recompute_root())
            .expect("first origin-root registration seals");
        registry
    }

    fn attacker_cut(content_tag: u8) -> ReplicaCutRecompute {
        let store = StoreId::from_digest([content_tag; 32]);
        ReplicaCutRecompute::from_local(
            store,
            FenceEpoch::genesis(store),
            CommitOrdinal::ZERO.successor().unwrap(),
            StateRoot::from_digest([content_tag; 32]),
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        )
    }

    /// NASTY (#374 T7): minting a capability from the pack's OWN cut must refuse
    /// — self-anchor forge is Unconstructible as Verified.
    #[test]
    fn self_anchor_forge_from_pack_own_cut_refuses() {
        let (pack, _) = sample_pack();
        let cut = pack.replica_cut_recompute();
        // Old forge: compare pack cut to itself → always equal → free Verified.
        assert!(matches!(
            ImportCapability::after_chain_root_verify(cut, cut),
            Err(PackRefuse::ForeignHistoryUnverified)
        ));
        // No verified cap exists to pass import_verify; unverified still refuses.
        let ledger = ShredLedger::new();
        assert!(matches!(
            import_verify(
                &pack,
                ImportCapability::unverified(),
                ObjectsCompleteness::Complete,
                &ledger
            ),
            Err(PackRefuse::ForeignHistoryUnverified)
        ));
    }

    /// NASTY (#374 T7): pack origin StoreId absent from sealed registry → refuse.
    #[test]
    fn unregistered_origin_refuses_foreign_history() {
        let (pack, _) = sample_pack();
        let empty = OriginRootRegistry::new();
        assert!(matches!(
            empty.after_chain_root_verify(&pack),
            Err(PackRefuse::ForeignHistoryUnverified)
        ));
    }

    /// NASTY (#374 T7): registered origin but attacker-chosen root ≠ trusted → refuse.
    #[test]
    fn wrong_registered_root_refuses_foreign_history() {
        let (pack, _) = sample_pack();
        let mut registry = OriginRootRegistry::new();
        registry
            .insert(
                pack.claimed_origin_store_id(),
                StateRoot::from_digest([0xAD; 32]),
            )
            .expect("first origin-root registration seals");
        assert_ne!(
            registry.get(pack.claimed_origin_store_id()),
            Some(pack.recompute_root()),
            "control: attacker root must differ from pack root"
        );
        assert!(matches!(
            registry.after_chain_root_verify(&pack),
            Err(PackRefuse::ForeignHistoryUnverified)
        ));
    }

    /// NASTY (#375 T3): register trusted root A for victim StoreId, then
    /// attacker root B(!=A) for the same StoreId on the production
    /// [`OriginRootRegistry::insert`] door → typed refuse (never silent
    /// overwrite of the sealed origin trust root).
    #[test]
    fn origin_root_registration_attacker_root_refuses_overwrite() {
        let (pack, _) = sample_pack();
        let victim = pack.claimed_origin_store_id();
        let root_a = pack.recompute_root();
        let root_b = StateRoot::from_digest([0xBE; 32]);
        assert_ne!(
            root_a, root_b,
            "control: attacker root must differ from sealed root A"
        );

        let mut registry = OriginRootRegistry::new();
        registry
            .insert(victim, root_a)
            .expect("first registration of root A seals the StoreId");
        assert_eq!(registry.get(victim), Some(root_a));

        // Same root re-register → idempotent Ok.
        registry
            .insert(victim, root_a)
            .expect("same root re-registration must be idempotent Ok");
        assert_eq!(registry.get(victim), Some(root_a));

        assert_eq!(
            registry.insert(victim, root_b),
            Err(PackRefuse::TrustRootAlreadySealed { store_id: victim })
        );
        assert_eq!(
            registry.get(victim),
            Some(root_a),
            "sealed root A must survive the refused overwrite attempt"
        );
    }

    /// Positive (#374 T7): pack root matches registered trusted root → admit.
    #[test]
    fn trusted_origin_matching_root_admits() {
        let (pack, _) = sample_pack();
        let registry = registry_trusting(&pack);
        let cap = registry
            .after_chain_root_verify(&pack)
            .expect("matching trusted root mints Verified");
        assert!(cap.is_verified());
        assert_eq!(cap.bound_root(), Some(pack.recompute_root()));
        let ledger = ShredLedger::new();
        assert!(
            import_verify(&pack, cap, ObjectsCompleteness::Complete, &ledger).is_ok(),
            "trusted-origin verified + complete objects must admit"
        );
        assert!(
            import_leave_is_free(&pack, cap, ObjectsCompleteness::Complete, &ledger).is_ok(),
            "production import_leave_is_free door must admit after ceremony"
        );
    }

    #[test]
    fn unverified_capability_refuses_foreign_history() {
        let (pack, _) = sample_pack();
        let ledger = ShredLedger::new();
        assert!(matches!(
            import_verify(
                &pack,
                ImportCapability::unverified(),
                ObjectsCompleteness::Complete,
                &ledger
            ),
            Err(PackRefuse::ForeignHistoryUnverified)
        ));
        assert!(!ImportCapability::unverified().is_verified());
        assert_eq!(ImportCapability::unverified().bound_root(), None);
    }

    /// NASTY (guardian, seat 80): bare two-cut mint is Unconstructible — attacker
    /// self-comparing an arbitrary cut never yields a verified capability.
    #[test]
    fn verified_capability_unbound_to_pack_must_not_import_it() {
        let (pack, _) = sample_pack();
        let ledger = ShredLedger::new();
        let attacker = attacker_cut(0x00);
        assert!(matches!(
            ImportCapability::after_chain_root_verify(attacker, attacker),
            Err(PackRefuse::ForeignHistoryUnverified)
        ));
        // Only unverified remains; import still refuses.
        assert!(matches!(
            import_verify(
                &pack,
                ImportCapability::unverified(),
                ObjectsCompleteness::Complete,
                &ledger
            ),
            Err(PackRefuse::ForeignHistoryUnverified)
        ));
    }

    #[test]
    fn forged_root_never_reaches_import_verify_as_verified() {
        let (pack, _) = sample_pack();
        let expected = attacker_cut(0x01);
        let forged = attacker_cut(0x02);
        // Bare two-cut ceremony is Unconstructible as Verified.
        let refuse = ImportCapability::after_chain_root_verify(expected, forged);
        assert!(matches!(refuse, Err(PackRefuse::ForeignHistoryUnverified)));
        let ledger = ShredLedger::new();
        assert!(matches!(
            import_verify(
                &pack,
                ImportCapability::unverified(),
                ObjectsCompleteness::Complete,
                &ledger
            ),
            Err(PackRefuse::ForeignHistoryUnverified)
        ));
    }

    #[test]
    fn incomplete_objects_refuse_even_when_verified() {
        let (pack, _) = sample_pack();
        let registry = registry_trusting(&pack);
        let cap = registry.after_chain_root_verify(&pack).unwrap();
        let ledger = ShredLedger::new();
        assert!(matches!(
            import_verify(&pack, cap, ObjectsCompleteness::Incomplete, &ledger),
            Err(PackRefuse::IncompleteRestore)
        ));
    }

    #[test]
    fn post_shred_restore_refuses_shredded() {
        let (pack, wrapped) = sample_pack();
        let registry = registry_trusting(&pack);
        let cap = registry.after_chain_root_verify(&pack).unwrap();
        let (_receipt, tombstone) = shred(wrapped);
        let mut ledger = ShredLedger::new();
        ledger.record(tombstone);
        assert!(matches!(
            import_verify(&pack, cap, ObjectsCompleteness::Complete, &ledger),
            Err(PackRefuse::Shredded)
        ));
    }

    #[test]
    fn store_refuse_foreign_history_unverified_tag_matches_pack() {
        // Seat 80 ledger tag must exist on the closed StoreRefuse sum and on
        // PackRefuse — same refuse name, no reshape into RetentionDeclined.
        let pack_tag = format!("{}", PackRefuse::ForeignHistoryUnverified);
        let store_tag = format!("{}", crate::store::failure::StoreRefuse::ForeignHistoryUnverified);
        assert!(
            pack_tag.contains("ForeignHistoryUnverified"),
            "pack refuse must name ForeignHistoryUnverified: {pack_tag}"
        );
        assert!(
            store_tag.contains("ForeignHistoryUnverified"),
            "store refuse must name ForeignHistoryUnverified: {store_tag}"
        );
    }
}

/// Seat 26 / #374 T11 — partial restore distinguishable from a complete store.
#[cfg(test)]
mod restore_integrity {
    use super::{
        IncompleteRestore, admit_complete_store, dump_storage, open_complete_store,
        restore_pairs_for_test, restore_storage,
    };
    use crate::store::fjall::new_fjall_storage;
    use crate::store::{ReadTx, Storage, WriteTx};
    use miette::miette;

    /// NASTY (#374 T11): interrupt mid-pair put after the in-progress mark is
    /// durable; reopen via plain complete-store open and assert typed refuse —
    /// never a silent smaller complete store.
    #[test]
    fn interrupted_restore_reopen_refuses_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let tgt_path = dir.path().join("restore_tgt");
        let db = new_fjall_storage(&tgt_path).unwrap();

        // More than one restore chunk so the poison fires after at least one
        // committed apply of dump pairs (mark already durable from phase 1).
        let n_pairs = super::RESTORE_PUT_CHUNK + 8;
        let mut yielded = 0usize;
        let poison = (0..n_pairs).map(move |i| {
            yielded += 1;
            // Fail on the first pair of the second chunk — mid-import.
            if yielded > super::RESTORE_PUT_CHUNK {
                return Err(miette!("injected interrupt mid-batch_put"));
            }
            let mut key = 1u64.to_be_bytes().to_vec();
            key.extend_from_slice(&(i as u64).to_be_bytes());
            Ok((key, vec![0xAB]))
        });

        let err = restore_pairs_for_test(&db, Box::new(poison)).unwrap_err();
        assert!(
            err.to_string().contains("injected interrupt"),
            "control: restore must fail from the injected interrupt, got: {err}"
        );
        drop(db);

        // Bare fjall open still binds the directory (bytes are there) —
        // completeness is the admit door, not substrate open.
        let bare = new_fjall_storage(&tgt_path).unwrap();
        {
            let tx = bare.read_tx().unwrap();
            assert!(
                tx.exists(super::RESTORE_IN_PROGRESS_KEY).unwrap(),
                "in-progress mark must survive the interrupt"
            );
            assert!(
                tx.total_scan().next().is_some(),
                "control: partial pairs landed — without the mark this would costume as a smaller complete store"
            );
        }
        let admit_err = admit_complete_store(&bare).unwrap_err();
        assert!(
            admit_err.downcast_ref::<IncompleteRestore>().is_some(),
            "admit_complete_store must typed-refuse IncompleteRestore, got: {admit_err}"
        );
        drop(bare);

        // Plain reopen-as-complete refuses.
        // match (not unwrap_err): Ok(FjallStorage) is not Debug.
        let reopen_err = match open_complete_store(&tgt_path) {
            Err(e) => e,
            Ok(_) => panic!("open_complete_store must refuse a partial restore"),
        };
        assert!(
            reopen_err.downcast_ref::<IncompleteRestore>().is_some(),
            "open_complete_store must typed-refuse IncompleteRestore, got: {reopen_err}"
        );
    }

    #[test]
    fn successful_restore_clears_mark_and_admits() {
        let dir = tempfile::tempdir().unwrap();
        let src = new_fjall_storage(dir.path().join("src")).unwrap();
        {
            let mut tx = src.write_tx().unwrap();
            let mut key = 1u64.to_be_bytes().to_vec();
            key.extend_from_slice(&0u64.to_be_bytes());
            tx.put(&key, b"v").unwrap();
            tx.commit().unwrap();
        }
        let dump = dir.path().join("d.kyzo");
        dump_storage(&src, &dump).unwrap();

        let tgt_path = dir.path().join("tgt");
        let tgt = new_fjall_storage(&tgt_path).unwrap();
        restore_storage(&tgt, &dump).unwrap();
        admit_complete_store(&tgt).expect("complete restore must admit");
        assert!(
            !tgt
                .read_tx()
                .unwrap()
                .exists(super::RESTORE_IN_PROGRESS_KEY)
                .unwrap(),
            "in-progress mark must be cleared after successful restore"
        );
        drop(tgt);
        open_complete_store(&tgt_path).expect("reopen after complete restore must admit");
    }
}
