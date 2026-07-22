/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Tag`: the single type-discriminant and cross-type order authority every encoding derives from.
//!
//! ## Canonical format v1 — the tag table (spec artifact, not folklore)
//!
//! The tag byte is the first byte of every canonical value encoding, so
//! the numeric order of tag bytes **is** the cross-type sort order. This
//! table is format v1 and is permanent: a test fails if any byte or any
//! ordering rule moves. FormatVersion is a plane/store-level contract and
//! is **never encoded inside comparable bytes** — mixed-version values in
//! one keyspace would sort by version, not by value. Evolution happens
//! through the reserved ranges below, which extend the order without
//! moving anything.
//!
//! | byte | kind     | reserved gap after |
//! |------|----------|--------------------|
//! | 0x05 | Null     | 0x06..=0x07        |
//! | 0x08 | Bool     | 0x09..=0x0F        |
//! | 0x10 | Num      | 0x11..=0x17        |
//! | 0x18 | Str      | 0x19..=0x1F        |
//! | 0x20 | Bytes    | 0x21..=0x27        |
//! | 0x28 | Uuid     | 0x29..=0x2F        |
//! | 0x30 | Regex    | 0x31..=0x37        |
//! | 0x38 | Json     | 0x39..=0x3F        |
//! | 0x40 | Vector   | 0x41..=0x47        |
//! | 0x48 | List     | 0x49..=0x4F        |
//! | 0x50 | Set      | 0x51..=0x57        |
//! | 0x58 | Validity | 0x59..=0x5F        |
//! | 0x60 | Interval | 0x61..=0x67        |
//! | 0x68 | Geometry | 0x69..=0xFF        |
//!
//! **Structural bytes**: `0x00` and `0x01` are reserved for the encoding
//! grammar (`0x00` opens string escapes/terminators, `0x01` closes
//! sequences) and are never valid tag bytes; `0x02..=0x04` are reserved
//! below every kind. This is what makes the sequence grammar prefix-safe:
//! no value's encoding begins with a byte less than `0x05`, so a sequence
//! terminator (`0x01`) sorts below any continuation — a prefix list sorts
//! before its extensions, exactly as the semantic law requires.
//!
//! **Map (v1)**: there is no first-class Map kind. Keyed documents
//! are `Json` values (whose identity law canonicalizes key order); a
//! relation is the engine's native keyed structure. If a first-class Map
//! ever earns existence, it takes a reserved tag; nothing moves.
//!
//! **Reserved-tag activation rule**: a store's plane FormatVersion (store
//! metadata, outside comparable bytes) gates decoding. Activating any
//! reserved tag requires a version bump, and a reader refuses a store
//! whose plane version exceeds what it knows — an old reader rejects the
//! store at open, never mid-keyspace on an unknown tag.
//!
//! **The cross-type order (v1, permanent)**: `Null < Bool < Num <
//! Str < Bytes < Uuid < Regex < Json < Vector < List < Set < Validity <
//! Interval < Geometry` — scalars, then identifiers, then documents, then
//! sequences, then the temporal kinds, then geometry. This is the
//! *storage* total order over kinds; expression-level comparability is a
//! separate, refusable authority and is not implied by this table.

/// Structural byte: opens string escapes and terminates string payloads.
/// Never a tag.
pub const STRUCT_STRING: u8 = 0x00;
/// Structural byte: terminates sequence payloads. Never a tag.
pub const STRUCT_SEQ_END: u8 = 0x01;

/// The 14 value kinds in fixed cross-type byte order: the sole
/// discriminant and order authority every encoding derives from. The
/// discriminant *is* the canonical tag byte.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum Tag {
    Null = 0x05,
    Bool = 0x08,
    Num = 0x10,
    Str = 0x18,
    Bytes = 0x20,
    Uuid = 0x28,
    Regex = 0x30,
    Json = 0x38,
    Vector = 0x40,
    List = 0x48,
    Set = 0x50,
    Validity = 0x58,
    Interval = 0x60,
    Geometry = 0x68,
}

impl Tag {
    /// The canonical tag byte (the discriminant).
    #[inline]
    pub fn byte(self) -> u8 {
        self as u8
    }

    /// Decode a tag byte. Total: reserved and structural bytes are `None`,
    /// never a panic.
    pub fn from_byte(b: u8) -> Option<Tag> {
        Some(match b {
            0x05 => Tag::Null,
            0x08 => Tag::Bool,
            0x10 => Tag::Num,
            0x18 => Tag::Str,
            0x20 => Tag::Bytes,
            0x28 => Tag::Uuid,
            0x30 => Tag::Regex,
            0x38 => Tag::Json,
            0x40 => Tag::Vector,
            0x48 => Tag::List,
            0x50 => Tag::Set,
            0x58 => Tag::Validity,
            0x60 => Tag::Interval,
            0x68 => Tag::Geometry,
            _other => return None,
        })
    }

    /// All 14 kinds in cross-type order.
    pub const ALL: [Tag; 14] = [
        Tag::Null,
        Tag::Bool,
        Tag::Num,
        Tag::Str,
        Tag::Bytes,
        Tag::Uuid,
        Tag::Regex,
        Tag::Json,
        Tag::Vector,
        Tag::List,
        Tag::Set,
        Tag::Validity,
        Tag::Interval,
        Tag::Geometry,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The v1 table, pinned byte-for-byte: this test failing means the
    /// format moved, which is forbidden.
    #[test]
    fn format_v1_tag_table_is_pinned() {
        let expected: [(Tag, u8); 14] = [
            (Tag::Null, 0x05),
            (Tag::Bool, 0x08),
            (Tag::Num, 0x10),
            (Tag::Str, 0x18),
            (Tag::Bytes, 0x20),
            (Tag::Uuid, 0x28),
            (Tag::Regex, 0x30),
            (Tag::Json, 0x38),
            (Tag::Vector, 0x40),
            (Tag::List, 0x48),
            (Tag::Set, 0x50),
            (Tag::Validity, 0x58),
            (Tag::Interval, 0x60),
            (Tag::Geometry, 0x68),
        ];
        for (tag, byte) in expected {
            assert_eq!(tag.byte(), byte, "format v1 violation: {tag:?} moved");
            assert_eq!(Tag::from_byte(byte), Some(tag));
        }
    }

    /// Tag order (the cross-type order authority) equals tag-byte order.
    #[test]
    fn cross_type_order_is_byte_order() {
        for w in Tag::ALL.windows(2) {
            assert!(w[0] < w[1], "ALL not in cross-type order");
            assert!(w[0].byte() < w[1].byte(), "tag byte order diverged");
        }
    }

    /// Structural bytes and reserved ranges never decode as tags; every
    /// tag byte round-trips; decode is total over all 256 bytes.
    #[test]
    fn structural_and_reserved_bytes_are_not_tags() {
        assert_eq!(Tag::from_byte(STRUCT_STRING), None);
        assert_eq!(Tag::from_byte(STRUCT_SEQ_END), None);
        let mut tags = 0;
        for b in 0..=255u8 {
            if let Some(t) = Tag::from_byte(b) {
                assert_eq!(t.byte(), b);
                assert!(b >= 0x05, "tag below the structural floor");
                tags += 1;
            }
        }
        assert_eq!(tags, 14, "exactly the 14 kinds decode");
    }
}
