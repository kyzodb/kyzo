// Copyright (c) 2025-present, fjall-rs
// This source code is licensed under both the Apache 2.0 and MIT License
// (found in the LICENSE-* files in the repository)

pub(crate) mod binary_index;
pub mod decoder;
mod encoder;
pub mod hash_index;
mod header;
mod identity;
mod offset;
mod trailer;
mod r#type;

pub(crate) use decoder::{Decodable, Decoder, ParsedItem};
pub(crate) use encoder::{Encodable, Encoder};
pub use header::Header;
pub use identity::{BlockIdentity, BlockKind, Level};
pub use offset::BlockOffset;
pub use r#type::BlockType;
pub(crate) use trailer::{Trailer, TRAILER_START_MARKER};

use crate::{
    coding::{Decode, Encode},
    table::BlockHandle,
    Checksum, CompressionType, Slice, TableId,
};
use std::fs::File;

/// A block on disk
///
/// Consists of a fixed-size header and some bytes (the data/payload).
#[derive(Clone)]
pub struct Block {
    pub header: Header,
    pub data: Slice,
}

impl Block {
    /// Returns the uncompressed block size in bytes.
    #[must_use]
    pub fn size(&self) -> usize {
        self.data.len()
    }

    /// Encodes a block into a writer.
    ///
    /// The data checksum covers [`BlockIdentity`] (table id / kind / offset) plus payload.
    pub fn write_into<W: std::io::Write>(
        mut writer: &mut W,
        data: &[u8],
        block_type: BlockType,
        compression: CompressionType,
        identity: &BlockIdentity,
    ) -> crate::Result<Header> {
        let mut header = Header {
            block_type,
            checksum: Checksum::from_raw(0), // <-- NOTE: Is set later on
            data_length: 0,                  // <-- NOTE: Is set later on

            #[expect(clippy::cast_possible_truncation, reason = "blocks are limited to u32")]
            uncompressed_length: data.len() as u32,
        };

        let data = match compression {
            CompressionType::None => data,

            #[cfg(feature = "lz4")]
            CompressionType::Lz4 => &lz4_flex::compress(data),
        };

        #[expect(clippy::cast_possible_truncation, reason = "blocks are limited to u32")]
        {
            header.data_length = data.len() as u32;
            header.checksum = identity.checksum(data);
        }

        header.encode_into(&mut writer)?;
        writer.write_all(data)?;

        log::trace!(
            "Writing block with size {}B (compressed: {}B) (excluding header of {}B) identity={identity:?}",
            header.uncompressed_length,
            header.data_length,
            Header::serialized_len(),
        );

        Ok(header)
    }

    /// Rebind each block's data checksum in a concatenated buffer to absolute offsets.
    ///
    /// Used by partitioned index/filter writers that first encode at relative offsets.
    pub(crate) fn rebind_checksums(
        buffer: &mut [u8],
        table_id: TableId,
        level: Level,
        base_offset: BlockOffset,
    ) -> crate::Result<()> {
        let mut pos = 0usize;

        while pos < buffer.len() {
            let abs_offset = BlockOffset(base_offset.0 + pos as u64);
            let identity = BlockIdentity::new(table_id, level, abs_offset);

            let header = Header::decode_from(&mut &buffer[pos..])?;
            let data_start = pos + Header::serialized_len();
            let data_end = data_start + header.data_length as usize;

            if data_end > buffer.len() {
                return Err(crate::Error::InvalidHeader("Block"));
            }

            let new_header = Header {
                block_type: header.block_type,
                checksum: identity.checksum(&buffer[data_start..data_end]),
                data_length: header.data_length,
                uncompressed_length: header.uncompressed_length,
            };

            let encoded = new_header.encode_into_vec();
            debug_assert_eq!(encoded.len(), Header::serialized_len());
            buffer[pos..data_start].copy_from_slice(&encoded);

            pos = data_end;
        }

        Ok(())
    }

    /// Reads a block from a reader.
    pub fn from_reader<R: std::io::Read>(
        reader: &mut R,
        compression: CompressionType,
        identity: &BlockIdentity,
    ) -> crate::Result<Self> {
        let header = Header::decode_from(reader)?;
        let raw_data = Slice::from_reader(reader, header.data_length as usize)?;

        let checksum = identity.checksum(&raw_data);

        checksum.check(header.checksum).inspect_err(|_| {
            log::error!(
                "Checksum mismatch for identity={identity:?}, got={checksum}, expected={}",
                header.checksum,
            );
        })?;

        let data = match compression {
            CompressionType::None => raw_data,

            #[cfg(feature = "lz4")]
            CompressionType::Lz4 => {
                #[warn(unsafe_code)]
                let mut builder =
                    unsafe { Slice::builder_unzeroed(header.uncompressed_length as usize) };

                lz4_flex::decompress_into(&raw_data, &mut builder)
                    .map_err(|_| crate::Error::Decompress(compression))?;

                builder.freeze().into()
            }
        };

        debug_assert_eq!(header.uncompressed_length, {
            #[expect(clippy::cast_possible_truncation, reason = "values are u32 length max")]
            {
                data.len() as u32
            }
        });

        Ok(Self { header, data })
    }

    /// Reads a block from a file.
    pub fn from_file(
        file: &File,
        handle: BlockHandle,
        compression: CompressionType,
        identity: &BlockIdentity,
    ) -> crate::Result<Self> {
        let buf = crate::file::read_exact(file, *handle.offset(), handle.size() as usize)?;

        let header = Header::decode_from(&mut &buf[..])?;

        #[expect(clippy::indexing_slicing)]
        let checksum = identity.checksum(&buf[Header::serialized_len()..]);

        checksum.check(header.checksum).inspect_err(|_| {
            log::error!(
                "Checksum mismatch for block {handle:?} identity={identity:?}, got={checksum}, expected={}",
                header.checksum,
            );
        })?;

        let buf = match compression {
            CompressionType::None => {
                let value = buf.slice(Header::serialized_len()..);

                #[expect(clippy::cast_possible_truncation, reason = "values are u32 length max")]
                {
                    debug_assert_eq!(header.uncompressed_length, value.len() as u32);
                }

                value
            }

            #[cfg(feature = "lz4")]
            CompressionType::Lz4 => {
                // NOTE: We know that a header always exists and data is never empty
                // So the slice is fine
                #[expect(clippy::indexing_slicing)]
                let raw_data = &buf[Header::serialized_len()..];

                #[warn(unsafe_code)]
                let mut builder =
                    unsafe { Slice::builder_unzeroed(header.uncompressed_length as usize) };

                lz4_flex::decompress_into(raw_data, &mut builder)
                    .map_err(|_| crate::Error::Decompress(compression))?;

                builder.freeze().into()
            }
        };

        Ok(Self { header, data: buf })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_log::test;

    // TODO: Block::from_file roundtrips

    #[test]
    fn block_roundtrip_uncompressed() -> crate::Result<()> {
        let identity = BlockIdentity::new(1, Level::new(0), BlockOffset(0));
        let mut writer = vec![];

        Block::write_into(
            &mut writer,
            b"abcdefabcdefabcdef",
            BlockType::Data,
            CompressionType::None,
            &identity,
        )?;

        {
            let mut reader = &writer[..];
            let block = Block::from_reader(&mut reader, CompressionType::None, &identity)?;
            assert_eq!(b"abcdefabcdefabcdef", &*block.data);
        }

        Ok(())
    }

    #[test]
    #[cfg(feature = "lz4")]
    fn block_roundtrip_lz4() -> crate::Result<()> {
        let identity = BlockIdentity::new(1, Level::new(0), BlockOffset(0));
        let mut writer = vec![];

        Block::write_into(
            &mut writer,
            b"abcdefabcdefabcdef",
            BlockType::Data,
            CompressionType::Lz4,
            &identity,
        )?;

        {
            let mut reader = &writer[..];
            let block = Block::from_reader(&mut reader, CompressionType::Lz4, &identity)?;
            assert_eq!(b"abcdefabcdefabcdef", &*block.data);
        }

        Ok(())
    }

    /// A relocated-but-intact block must fail verification: checksum covers
    /// logical block identity (table id / kind / offset), not content alone.
    #[test]
    fn relocated_but_intact_block_fails_verification() -> crate::Result<()> {
        let original = BlockIdentity::new(7, Level::new(2), BlockOffset(4_096));
        let relocated = BlockIdentity::new(7, Level::new(2), BlockOffset(8_192));

        let mut bytes = vec![];
        Block::write_into(
            &mut bytes,
            b"intact-payload-bytes",
            BlockType::Data,
            CompressionType::None,
            &original,
        )?;

        // Same bytes, different logical identity → must refuse.
        match Block::from_reader(&mut &bytes[..], CompressionType::None, &relocated) {
            Err(crate::Error::ChecksumMismatch { .. }) => {}
            Err(err) => panic!("expected ChecksumMismatch, got {err:?}"),
            Ok(_) => panic!("relocated-but-intact block must fail verification"),
        }

        // Wrong table id is also caught (swap-two-tables).
        let other_table = BlockIdentity::new(8, Level::new(2), BlockOffset(4_096));
        match Block::from_reader(&mut &bytes[..], CompressionType::None, &other_table) {
            Err(crate::Error::ChecksumMismatch { .. }) => {}
            Err(err) => panic!("expected ChecksumMismatch, got {err:?}"),
            Ok(_) => panic!("cross-table relocated block must fail verification"),
        }

        // Content-only checksum would accept the relocated bytes; identity binding must not.
        let content_only = Checksum::from_raw(crate::hash::hash128(b"intact-payload-bytes"));
        let header = Header::decode_from(&mut &bytes[..])?;
        assert_ne!(
            header.checksum, content_only,
            "data checksum must bind logical block identity, not content alone"
        );

        // Original identity still verifies.
        let block = Block::from_reader(&mut &bytes[..], CompressionType::None, &original)?;
        assert_eq!(b"intact-payload-bytes", &*block.data);

        Ok(())
    }
}
