// Copyright (c) 2025-present, fjall-rs
// This source code is licensed under both the Apache 2.0 and MIT License
// (found in the LICENSE-* files in the repository)

use super::BlockOffset;
use crate::{checksum::Checksum, TableId};

/// Logical block identity bound into the block data checksum (§49).
///
/// The checksum covers `table_id` / `level` / `offset` plus payload bytes, so a
/// byte-identical block relocated to a different identity fails verification.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct BlockIdentity {
    /// Table that owns the block.
    pub table_id: TableId,
    /// Write-time LSM level (`initial_level`), or [`BlockIdentity::META_LEVEL`] for meta.
    pub level: u8,
    /// Logical file offset of the block.
    pub offset: BlockOffset,
}

impl BlockIdentity {
    /// Sentinel level for meta blocks (avoids chicken-egg with `initial_level` in meta).
    pub const META_LEVEL: u8 = u8::MAX;

    /// Construct a logical block identity from table id, level, and offset.
    #[must_use]
    pub const fn new(table_id: TableId, level: u8, offset: BlockOffset) -> Self {
        Self {
            table_id,
            level,
            offset,
        }
    }

    /// Identity for a meta block: table id + offset, meta-level sentinel.
    #[must_use]
    pub const fn meta(table_id: TableId, offset: BlockOffset) -> Self {
        Self::new(table_id, Self::META_LEVEL, offset)
    }

    /// Checksum over logical block identity and payload bytes.
    #[must_use]
    pub fn checksum(self, data: &[u8]) -> Checksum {
        let mut hasher = xxhash_rust::xxh3::Xxh3Default::new();
        hasher.update(&self.table_id.to_le_bytes());
        hasher.update(&[self.level]);
        hasher.update(&self.offset.0.to_le_bytes());
        hasher.update(data);
        Checksum::from_raw(hasher.digest128())
    }
}
