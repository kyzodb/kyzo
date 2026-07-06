/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Integrity verification: walk the whole store and check every invariant
//! that can be checked offline. A suspect store gets a *report*, not a chain
//! of mystery query failures — corruption is diagnosed, never discovered.
//!
//! Checked per pair: the key decodes as a memcmp tuple key, the value payload
//! decodes, and keys arrive in strictly ascending order (a store-level
//! ordering violation means the storage engine itself is unwell).

use fjall::Slice;
use miette::Result;

use crate::data::tuple::{decode_tuple_from_key, extend_tuple_from_v};
use crate::storage::{ReadTx, Storage};

/// Cap on recorded corrupt entries: the report proves and locates corruption
/// without itself growing unboundedly on a badly damaged store.
const MAX_RECORDED: usize = 100;

/// One corrupt pair: where and why.
#[derive(Debug)]
pub struct CorruptEntry {
    /// The raw key, hex-encoded, truncated to 64 bytes.
    pub key_hex: String,
    /// What failed to decode.
    pub error: String,
}

/// The result of a full-store verification walk.
#[derive(Debug, Default)]
pub struct VerifyReport {
    /// Total key-value pairs examined.
    pub checked: u64,
    /// Pairs whose key or value failed to decode (capped; see `truncated`).
    pub corrupt: Vec<CorruptEntry>,
    /// Count of adjacent key pairs violating ascending order.
    pub ordering_violations: u64,
    /// True if more corrupt entries existed than were recorded.
    pub truncated: bool,
}

impl VerifyReport {
    /// A store passes verification iff nothing was found.
    pub fn is_clean(&self) -> bool {
        self.corrupt.is_empty() && self.ordering_violations == 0 && !self.truncated
    }
}

fn hex_prefix(bytes: &[u8]) -> String {
    let take = bytes.len().min(64);
    let mut s = String::with_capacity(take * 2 + 1);
    for b in &bytes[..take] {
        s.push_str(&format!("{b:02x}"));
    }
    if bytes.len() > 64 {
        s.push('…');
    }
    s
}

/// Walk every pair in the store and verify decodability and ordering.
///
/// Read-only, snapshot-consistent, and total: corrupt pairs are recorded and
/// the walk continues — one bad page must not hide the rest of the damage.
pub fn verify_storage<S: Storage>(db: &S) -> Result<VerifyReport> {
    let tx = db.read_tx()?;
    let mut report = VerifyReport::default();
    let mut prev_key: Option<Slice> = None;

    for pair in tx.total_scan() {
        let (k, v) = pair?;
        report.checked += 1;

        if let Some(prev) = &prev_key
            && k.as_slice() <= prev.as_slice()
        {
            report.ordering_violations += 1;
        }

        let decode_result = decode_tuple_from_key(&k, 16)
            .and_then(|mut tup| extend_tuple_from_v(&mut tup, &v).map(|()| tup));
        if let Err(e) = decode_result {
            if report.corrupt.len() < MAX_RECORDED {
                report.corrupt.push(CorruptEntry {
                    key_hex: hex_prefix(&k),
                    error: e.to_string(),
                });
            } else {
                report.truncated = true;
            }
        }
        prev_key = Some(k);
    }
    Ok(report)
}
