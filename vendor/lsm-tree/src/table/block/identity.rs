// Copyright (c) 2025-present, fjall-rs
// This source code is licensed under both the Apache 2.0 and MIT License
// (found in the LICENSE-* files in the repository)

use super::BlockOffset;
use crate::{checksum::Checksum, TableId};

/// Write-time LSM level bound into a data-block checksum (§49).
///
/// Distinct from the version-map [`crate::version`] level collection: this is the
/// scalar level identity baked into [`BlockIdentity`], never a meta sentinel.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct Level(u8);

impl Level {
    /// Construct a write-time level identity.
    #[must_use]
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Borrow the inner level ordinal.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Little-endian bytes for meta-property persistence.
    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; 1] {
        self.0.to_le_bytes()
    }
}

impl From<u8> for Level {
    fn from(value: u8) -> Self {
        Self::new(value)
    }
}

/// Kind of block bound into the data checksum — data at a [`Level`], or meta.
///
/// Meta is a variant, not a level sentinel (`u8::MAX` is banned).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum BlockKind {
    /// Data / index / filter block written at this LSM level.
    Data(Level),
    /// Table or blob-file meta block (no LSM level).
    Meta,
}

/// Logical block identity bound into the block data checksum (§49).
///
/// The checksum covers `table_id` / [`BlockKind`] / `offset` plus payload bytes,
/// so a byte-identical block relocated to a different identity fails verification.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct BlockIdentity {
    /// Table that owns the block.
    pub table_id: TableId,
    /// Data-at-level or meta — checksum binds the variant.
    pub kind: BlockKind,
    /// Logical file offset of the block.
    pub offset: BlockOffset,
}

impl BlockIdentity {
    /// Construct a logical identity for a data block at `level`.
    #[must_use]
    pub const fn new(table_id: TableId, level: Level, offset: BlockOffset) -> Self {
        Self {
            table_id,
            kind: BlockKind::Data(level),
            offset,
        }
    }

    /// Identity for a meta block: table id + offset, [`BlockKind::Meta`].
    #[must_use]
    pub const fn meta(table_id: TableId, offset: BlockOffset) -> Self {
        Self {
            table_id,
            kind: BlockKind::Meta,
            offset,
        }
    }

    /// Checksum over logical block identity and payload bytes.
    #[must_use]
    pub fn checksum(self, data: &[u8]) -> Checksum {
        let mut hasher = xxhash_rust::xxh3::Xxh3Default::new();
        hasher.update(&self.table_id.to_le_bytes());
        match self.kind {
            BlockKind::Data(level) => {
                // Discriminant 0 + level ordinal — binds Data(level), not Meta.
                hasher.update(&[0]);
                hasher.update(&[level.get()]);
            }
            BlockKind::Meta => {
                // Discriminant 1 alone — Meta is a variant, never u8::MAX-as-level.
                hasher.update(&[1]);
            }
        }
        hasher.update(&self.offset.0.to_le_bytes());
        hasher.update(data);
        Checksum::from_raw(hasher.digest128())
    }
}
