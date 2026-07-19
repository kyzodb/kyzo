/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Pure-Rust backup/interchange: dump the entire key-value store to a portable
//! file and restore it into a fresh store. (The CozoDB base used SQLite for
//! this role; KyzoDB's format is a simple length-prefixed binary file.)
//!
//! Format: 8-byte magic `KYZODMP2`, then for each pair a u64-BE key length,
//! the key bytes, a u64-BE value length, the value bytes. Pairs appear in
//! ascending key order (`total_scan` order), which is exactly what
//! [`Storage::batch_put`](crate::Storage::batch_put) requires on restore.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::Path;

use miette::{Diagnostic, IntoDiagnostic, Result, bail, miette};
use thiserror::Error;

use crate::data::bitemporal::system_stamp_of_key;
use kyzo_model::value::ValidityTs;
use kyzo_model::value::{StorageKey, RelationId};
use crate::runtime::relation::{KeyspaceKind, list_relations};
use crate::storage::{FormatVersion, ReadTx, Storage};

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
/// exactly like [`FjallStorage::stamp_after_snapshot`](crate::storage::fjall::FjallStorage).
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
    db.raise_clock_floor(kyzo_model::value::ValidityTs::from_raw(
        i64::from_be_bytes(floor_bytes),
    ))?;
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
