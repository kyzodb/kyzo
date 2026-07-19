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
//! incomplete restore.
//!
//! Dump format: 8-byte magic `KYZODMP2`, then for each pair a u64-BE key
//! length, the key bytes, a u64-BE value length, the value bytes. Pairs appear
//! in ascending key order (`total_scan` order), which is exactly what
//! [`Storage::batch_put`](crate::Storage::batch_put) requires on restore.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::Path;

use miette::{Diagnostic, IntoDiagnostic, Result, bail, miette};
use thiserror::Error;

use crate::session::catalog::{KeyspaceKind, list_relations};
use crate::store::authority::IncarnationId;
use crate::store::crypto::{ShredLedger, WrappedShredSalt};
use crate::store::time::system_stamp_of_key;
use crate::store::{FormatVersion, ReadTx, Storage};
use kyzo_model::value::ValidityTs;
use kyzo_model::value::{RelationId, StorageKey};

const MAGIC: &[u8; 8] = b"KYZODMP2";

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

/// Restore a dump produced by [`dump_storage`] into the storage.
///
/// The target must be **empty** and must not be accessed concurrently: an
/// interrupted restore leaves a clean prefix of the dump (see
/// [`Storage::batch_put`]), and requiring an empty target means recovery is
/// always "discard and re-run" — a partial restore can never be mistaken for
/// a complete store, and never merges into existing data. The restored data
/// is fsynced before this returns.
pub fn restore_storage<S: Storage>(db: &S, path: impl AsRef<Path>) -> Result<()> {
    {
        let tx = db.read_tx()?;
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
    let iter = std::iter::from_fn(move || read_pair(&mut r).transpose());
    db.batch_put(Box::new(iter))?;
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
    #[error("import ceremony: foreign history unverified")]
    #[diagnostic(code(store::backup::foreign_unverified))]
    ForeignHistoryUnverified,
    #[error("import ceremony: incomplete restore refused (never green-incomplete)")]
    #[diagnostic(code(store::backup::incomplete_restore))]
    IncompleteRestore,
    #[error("post-shred restore of shredded segment")]
    #[diagnostic(code(store::backup::shredded))]
    Shredded,
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
}

/// Pack hygiene scrub point (§65): Store/Engine bundle emit and leave-is-free
/// boundaries. WA / KEK / plaintext salt / AuditKey / MintCap presence after
/// this point is a Spec violation — those types have no field on the pack.
fn pack_hygiene_scrub(pack: &LeaveIsFreePack) -> Result<(), PackRefuse> {
    // Structural scrub: required handles present; forbidden secrets have no
    // constructor into LeaveIsFreePack. Empty payload alone is still a pack
    // (objects may be backend-retained under the cut certificate).
    if pack.wrapped_shred_salts.is_empty() || pack.incarnation_history.is_empty() {
        return Err(PackRefuse::HygieneSecretMaterial);
    }
    Ok(())
}

/// Import verify ceremony (§80): foreign dumps only under capability + chain /
/// root verify. Blind import is a second write door for forged belief.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImportCapability {
    /// Caller presented chain/root verify authority.
    verified: bool,
}

impl ImportCapability {
    /// Mint after chain/root verification succeeded.
    pub fn after_chain_verify() -> Self {
        Self { verified: true }
    }

    /// Unverified foreign import — ceremony will refuse.
    pub fn unverified() -> Self {
        Self { verified: false }
    }
}

/// Run the import verify ceremony over a leave-is-free pack.
///
/// [`ShredLedger`] is consulted so post-shred restore of a pack that still
/// carries a shredded segment's `WrappedShredSalt` converges to
/// [`PackRefuse::Shredded`] (§64 / §80) — not silent unreadability.
pub fn import_verify(
    pack: &LeaveIsFreePack,
    cap: ImportCapability,
    objects_complete: bool,
    shred_ledger: &ShredLedger,
) -> Result<(), PackRefuse> {
    if !cap.verified {
        return Err(PackRefuse::ForeignHistoryUnverified);
    }
    if pack.wrapped_shred_salts.is_empty() {
        return Err(PackRefuse::MissingWrappedShredSalt);
    }
    if pack.incarnation_history.is_empty() {
        return Err(PackRefuse::MissingIncarnationHistory);
    }
    if !objects_complete {
        return Err(PackRefuse::IncompleteRestore);
    }
    for wrapped in &pack.wrapped_shred_salts {
        if shred_ledger.is_shredded(wrapped) {
            return Err(PackRefuse::Shredded);
        }
    }
    pack_hygiene_scrub(pack)
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
            ImportCapability, LeaveIsFreeKind, LeaveIsFreePack, LeaveIsFreeParts, PackRefuse,
            import_verify,
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
        assert!(
            import_verify(
                &pack,
                ImportCapability::after_chain_verify(),
                true,
                &empty_ledger
            )
            .is_ok()
        );
        assert!(matches!(
            import_verify(&pack, ImportCapability::unverified(), true, &empty_ledger),
            Err(PackRefuse::ForeignHistoryUnverified)
        ));

        // Post-shred restore of a pack that still carries the wrap → Shredded.
        let (_receipt, tombstone) = shred(wrapped);
        let mut shredded = ShredLedger::new();
        shredded.record(tombstone);
        assert!(matches!(
            import_verify(
                &pack,
                ImportCapability::after_chain_verify(),
                true,
                &shredded
            ),
            Err(PackRefuse::Shredded)
        ));
    }
}
