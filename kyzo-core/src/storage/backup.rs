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
//! Format: 8-byte magic `KYZODMP1`, then for each pair a u64-BE key length,
//! the key bytes, a u64-BE value length, the value bytes. Pairs appear in
//! ascending key order (`total_scan` order), which is exactly what
//! [`Storage::batch_put`](crate::Storage::batch_put) requires on restore.

use std::fs::File;
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::Path;

use miette::{IntoDiagnostic, Result, bail, miette};

use crate::storage::{FormatVersion, ReadTx, Storage};

const MAGIC: &[u8; 8] = b"KYZODMP1";

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
    let tx = db.read_tx()?;
    for pair in tx.total_scan() {
        let (k, v) = pair?;
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
