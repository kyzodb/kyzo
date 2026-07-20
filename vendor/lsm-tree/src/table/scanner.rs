// Copyright (c) 2024-present, fjall-rs
// This source code is licensed under both the Apache 2.0 and MIT License
// (found in the LICENSE-* files in the repository)

use super::{Block, DataBlock};
use crate::{
    table::{
        block::{BlockIdentity, BlockOffset, BlockType, Level},
        iter::OwnedDataBlockIter,
    },
    CompressionType, InternalValue, SeqNo, TableId,
};
use std::{fs::File, io::BufReader, path::Path};

/// Table reader that is optimized for consuming an entire table
pub struct Scanner {
    reader: BufReader<File>,
    iter: OwnedDataBlockIter,

    compression: CompressionType,
    block_count: usize,
    read_count: usize,

    global_seqno: SeqNo,

    table_id: TableId,
    level: Level,
    next_offset: BlockOffset,
}

impl Scanner {
    pub fn new(
        path: &Path,
        block_count: usize,
        compression: CompressionType,
        global_seqno: SeqNo,
        table_id: TableId,
        level: Level,
    ) -> crate::Result<Self> {
        // TODO: a larger buffer size may be better for HDD, maybe make this configurable
        // TODO: benchmarks were inconclusive on SSD, not much difference between 4KB - 2MB
        let mut reader = BufReader::with_capacity(8 * 4_096, File::open(path)?);

        let mut next_offset = BlockOffset(0);
        let block = Self::fetch_next_block(
            &mut reader,
            compression,
            &BlockIdentity::new(table_id, level, next_offset),
            &mut next_offset,
        )?;
        let iter = OwnedDataBlockIter::new(block, DataBlock::iter);

        Ok(Self {
            reader,
            iter,

            compression,
            block_count,
            read_count: 1,

            global_seqno,

            table_id,
            level,
            next_offset,
        })
    }

    fn fetch_next_block(
        reader: &mut BufReader<File>,
        compression: CompressionType,
        identity: &BlockIdentity,
        next_offset: &mut BlockOffset,
    ) -> crate::Result<DataBlock> {
        let block = Block::from_reader(reader, compression, identity);

        match block {
            Ok(block) => {
                if block.header.block_type != BlockType::Data {
                    return Err(crate::Error::InvalidTag((
                        "BlockType",
                        block.header.block_type.into(),
                    )));
                }

                *next_offset += u64::from(
                    crate::table::block::Header::serialized_len() as u32 + block.header.data_length,
                );

                Ok(DataBlock::new(block))
            }
            Err(e) => Err(e),
        }
    }
}

impl Iterator for Scanner {
    type Item = crate::Result<InternalValue>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(mut item) = self.iter.next() {
                item.key.seqno += self.global_seqno;
                return Some(Ok(item));
            }

            if self.read_count >= self.block_count {
                return None;
            }

            // Init new block
            let identity = BlockIdentity::new(self.table_id, self.level, self.next_offset);
            let block = fail_iter!(Self::fetch_next_block(
                &mut self.reader,
                self.compression,
                &identity,
                &mut self.next_offset,
            ));
            self.iter = OwnedDataBlockIter::new(block, DataBlock::iter);

            self.read_count += 1;
        }
    }
}
