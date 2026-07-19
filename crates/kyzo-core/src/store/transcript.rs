/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! CanonicalTranscript: the one signed-byte law (decisions.md §59, §81).
//!
//! Owns: [`CanonicalTranscript`], golden vector fixtures under `store/golden/`,
//! unknown-version refuse door.
//!
//! Bans: sealed Unicode normalization surfaces (bytes and typed ids only);
//! map encodings without duplicate-key refusal; allocation before bounds
//! checks; a second serialization path for sealed artifacts.
//!
//! Encoding checklist as law: field ids and ordering; integer width /
//! signedness / endianness; optional/default encoding; length bounds and
//! recursion limits before allocation; hash / signature / key-id / domain-label
//! encodings; AEAD nonce and AAD transcript fields; unknown version refuses;
//! migration epochs anchor upgrades via [`FormatVersion`].

use super::contract::FormatVersion;

/// Wire magic for sealed transcript bytes.
const MAGIC: &[u8; 4] = b"KTX1";

/// Maximum bytes accepted in one length-prefixed field (checked before alloc).
const MAX_BYTES_FIELD: u32 = 1 << 20;

/// Maximum fields in one transcript.
const MAX_FIELDS: u32 = 4096;

/// Maximum map entries in one map field.
const MAX_MAP_ENTRIES: u32 = 1024;

/// Maximum nested map depth.
const MAX_MAP_DEPTH: u8 = 8;

/// Closed sealed-artifact kinds that ship golden vectors (§59).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u64)]
pub enum SealedArtifactKind {
    /// CheckpointSeal binding digest (§26).
    CheckpointSeal = 1,
    /// AdmissionCertificate envelope (§69).
    AdmissionCertificate = 2,
    /// ForkGrant seed payload (§68).
    ForkGrant = 3,
    /// RecoveryGrant seed payload (§2/§68).
    RecoveryGrant = 4,
    /// MergeProof header (§66).
    MergeProofHeader = 5,
    /// AuditKey leaf MAC input (§59).
    AuditKeyLeaf = 6,
    /// WAL segment / record header (§21).
    WalHeader = 7,
}

impl SealedArtifactKind {
    /// Stable discriminant written into golden / sealed transcripts.
    pub fn tag(self) -> u64 {
        self as u64
    }
}

/// Stable field identifier — ordering is part of the transcript law.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FieldId(u16);

impl FieldId {
    /// Artifact-kind discriminant field (always first).
    pub const ARTIFACT_KIND: FieldId = FieldId(1);
    /// FormatVersion bytes field.
    pub const FORMAT_VERSION: FieldId = FieldId(2);
    /// Primary digest / identity binding.
    pub const PRIMARY_DIGEST: FieldId = FieldId(3);
    /// Secondary digest binding (predecessor, lineage, …).
    pub const SECONDARY_DIGEST: FieldId = FieldId(4);
    /// Domain label bytes (typed id — never Unicode-normalized text).
    pub const DOMAIN_LABEL: FieldId = FieldId(5);
    /// AEAD nonce bytes when the transcript is used as AAD.
    pub const AEAD_NONCE: FieldId = FieldId(6);
    /// Ordered map of typed bindings.
    pub const BINDINGS_MAP: FieldId = FieldId(7);

    /// Construct a field id from its wire value (decode / fixture sites).
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    /// Wire value.
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Field wire tags — width and endianness are law.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum FieldTag {
    U64 = 1,
    Bytes = 2,
    Digest32 = 3,
    Map = 4,
    /// Optional field present with empty payload meaning "default / absent".
    OptionalAbsent = 5,
}

/// Sealed canonical bytes produced by the one transcript constructor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalTranscript {
    bytes: Vec<u8>,
}

/// Typed refuse from transcript construction / decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum TranscriptRefuse {
    #[error("CanonicalTranscript: unknown FormatVersion — refuse (no silent decode)")]
    #[diagnostic(code(store::transcript::unknown_version))]
    UnknownVersion,
    #[error("CanonicalTranscript: corrupt or truncated encoding")]
    #[diagnostic(code(store::transcript::corrupt))]
    Corrupt,
    #[error("CanonicalTranscript: length bound exceeded before allocation")]
    #[diagnostic(code(store::transcript::length_bound))]
    LengthBoundExceeded,
    #[error("CanonicalTranscript: field count bound exceeded")]
    #[diagnostic(code(store::transcript::field_bound))]
    FieldBoundExceeded,
    #[error("CanonicalTranscript: map duplicate key refused")]
    #[diagnostic(code(store::transcript::duplicate_map_key))]
    DuplicateMapKey,
    #[error("CanonicalTranscript: map entries must be strictly ascending by key")]
    #[diagnostic(code(store::transcript::map_order))]
    MapOrderViolated,
    #[error("CanonicalTranscript: map recursion limit exceeded")]
    #[diagnostic(code(store::transcript::recursion_limit))]
    RecursionLimitExceeded,
    #[error("CanonicalTranscript: field id ordering violated (ids must be non-decreasing)")]
    #[diagnostic(code(store::transcript::field_order))]
    FieldOrderViolated,
}

/// In-progress builder — the sole construction path for sealed transcript bytes.
///
/// A second serialization path for sealed artifacts is Unconstructible: types
/// that seal under this law offer no serde / alternate encoder surface.
#[derive(Debug)]
pub struct CanonicalTranscriptBuilder {
    buf: Vec<u8>,
    field_count_at: usize,
    field_count: u32,
    last_field_id: Option<FieldId>,
    map_depth: u8,
}

impl CanonicalTranscriptBuilder {
    /// The one versioned constructor. Unknown [`FormatVersion`] refuses.
    pub fn new(version: FormatVersion) -> Result<Self, TranscriptRefuse> {
        if !is_known_version(version) {
            return Err(TranscriptRefuse::UnknownVersion);
        }
        let ver = version.as_bytes();
        if ver.len() > u8::MAX as usize {
            return Err(TranscriptRefuse::LengthBoundExceeded);
        }
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(MAGIC);
        buf.push(ver.len() as u8);
        buf.extend_from_slice(&ver);
        let field_count_at = buf.len();
        buf.extend_from_slice(&0u32.to_be_bytes());
        Ok(Self {
            buf,
            field_count_at,
            field_count: 0,
            last_field_id: None,
            map_depth: 0,
        })
    }

    /// Append a u64 field (big-endian).
    pub fn append_u64(&mut self, id: FieldId, value: u64) -> Result<(), TranscriptRefuse> {
        self.begin_field(id)?;
        self.buf.push(FieldTag::U64 as u8);
        self.buf.extend_from_slice(&value.to_be_bytes());
        Ok(())
    }

    /// Append a length-prefixed bytes field. Bounds checked before allocation.
    pub fn append_bytes(&mut self, id: FieldId, bytes: &[u8]) -> Result<(), TranscriptRefuse> {
        let len = u32::try_from(bytes.len()).map_err(|_| TranscriptRefuse::LengthBoundExceeded)?;
        if len > MAX_BYTES_FIELD {
            return Err(TranscriptRefuse::LengthBoundExceeded);
        }
        self.begin_field(id)?;
        self.buf.push(FieldTag::Bytes as u8);
        self.buf.extend_from_slice(&len.to_be_bytes());
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// Append a fixed 32-byte digest / key-id / hash field.
    pub fn append_digest32(
        &mut self,
        id: FieldId,
        digest: &[u8; 32],
    ) -> Result<(), TranscriptRefuse> {
        self.begin_field(id)?;
        self.buf.push(FieldTag::Digest32 as u8);
        self.buf.extend_from_slice(digest);
        Ok(())
    }

    /// Append an optional field encoded as absent (default encoding).
    pub fn append_optional_absent(&mut self, id: FieldId) -> Result<(), TranscriptRefuse> {
        self.begin_field(id)?;
        self.buf.push(FieldTag::OptionalAbsent as u8);
        Ok(())
    }

    /// Append an ordered map. Keys must be strictly ascending; duplicates refuse.
    pub fn append_map(
        &mut self,
        id: FieldId,
        entries: &[(Vec<u8>, MapValue)],
    ) -> Result<(), TranscriptRefuse> {
        if entries.len() as u32 > MAX_MAP_ENTRIES {
            return Err(TranscriptRefuse::LengthBoundExceeded);
        }
        if self.map_depth >= MAX_MAP_DEPTH {
            return Err(TranscriptRefuse::RecursionLimitExceeded);
        }
        self.begin_field(id)?;
        self.buf.push(FieldTag::Map as u8);
        self.buf
            .extend_from_slice(&(entries.len() as u32).to_be_bytes());
        self.map_depth = self.map_depth.saturating_add(1);
        let mut prev: Option<&[u8]> = None;
        for (key, value) in entries {
            if key.len() as u32 > MAX_BYTES_FIELD {
                return Err(TranscriptRefuse::LengthBoundExceeded);
            }
            if let Some(p) = prev {
                match key.as_slice().cmp(p) {
                    std::cmp::Ordering::Equal => {
                        return Err(TranscriptRefuse::DuplicateMapKey);
                    }
                    std::cmp::Ordering::Less => {
                        return Err(TranscriptRefuse::MapOrderViolated);
                    }
                    std::cmp::Ordering::Greater => {}
                }
            }
            let key_len =
                u16::try_from(key.len()).map_err(|_| TranscriptRefuse::LengthBoundExceeded)?;
            self.buf.extend_from_slice(&key_len.to_be_bytes());
            self.buf.extend_from_slice(key);
            self.write_map_value(value)?;
            prev = Some(key.as_slice());
        }
        self.map_depth -= 1;
        Ok(())
    }

    /// Seal the builder into immutable canonical bytes.
    pub fn seal(mut self) -> CanonicalTranscript {
        let count = self.field_count.to_be_bytes();
        self.buf[self.field_count_at..self.field_count_at + 4].copy_from_slice(&count);
        CanonicalTranscript { bytes: self.buf }
    }

    fn begin_field(&mut self, id: FieldId) -> Result<(), TranscriptRefuse> {
        if self.field_count >= MAX_FIELDS {
            return Err(TranscriptRefuse::FieldBoundExceeded);
        }
        if let Some(last) = self.last_field_id {
            if id.get() < last.get() {
                return Err(TranscriptRefuse::FieldOrderViolated);
            }
        }
        self.buf.extend_from_slice(&id.get().to_be_bytes());
        self.field_count += 1;
        self.last_field_id = Some(id);
        Ok(())
    }

    fn write_map_value(&mut self, value: &MapValue) -> Result<(), TranscriptRefuse> {
        match value {
            MapValue::U64(v) => {
                self.buf.push(FieldTag::U64 as u8);
                self.buf.extend_from_slice(&v.to_be_bytes());
            }
            MapValue::Bytes(b) => {
                let len =
                    u32::try_from(b.len()).map_err(|_| TranscriptRefuse::LengthBoundExceeded)?;
                if len > MAX_BYTES_FIELD {
                    return Err(TranscriptRefuse::LengthBoundExceeded);
                }
                self.buf.push(FieldTag::Bytes as u8);
                self.buf.extend_from_slice(&len.to_be_bytes());
                self.buf.extend_from_slice(b);
            }
            MapValue::Digest32(d) => {
                self.buf.push(FieldTag::Digest32 as u8);
                self.buf.extend_from_slice(d);
            }
        }
        Ok(())
    }
}

/// Value carried inside a transcript map entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapValue {
    /// Big-endian u64.
    U64(u64),
    /// Length-bounded bytes.
    Bytes(Vec<u8>),
    /// Fixed 32-byte digest.
    Digest32([u8; 32]),
}

impl CanonicalTranscript {
    /// Borrow the sealed canonical bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Decode sealed bytes. Unknown version refuses; corrupt input refuses.
    pub fn parse(bytes: &[u8]) -> Result<Self, TranscriptRefuse> {
        let mut i = 0usize;
        if bytes.len() < MAGIC.len() + 1 + 4 {
            return Err(TranscriptRefuse::Corrupt);
        }
        if &bytes[i..i + MAGIC.len()] != MAGIC {
            return Err(TranscriptRefuse::Corrupt);
        }
        i += MAGIC.len();
        let ver_len = bytes[i] as usize;
        i += 1;
        if bytes.len() < i + ver_len + 4 {
            return Err(TranscriptRefuse::Corrupt);
        }
        let ver_bytes = &bytes[i..i + ver_len];
        i += ver_len;
        let version = FormatVersion::parse(ver_bytes).map_err(|_| TranscriptRefuse::Corrupt)?;
        if !is_known_version(version) {
            return Err(TranscriptRefuse::UnknownVersion);
        }
        let field_count = read_u32(bytes, &mut i)?;
        if field_count > MAX_FIELDS {
            return Err(TranscriptRefuse::FieldBoundExceeded);
        }
        let mut last_id: Option<u16> = None;
        for _ in 0..field_count {
            let id = read_u16(bytes, &mut i)?;
            if let Some(prev) = last_id {
                if id < prev {
                    return Err(TranscriptRefuse::FieldOrderViolated);
                }
            }
            last_id = Some(id);
            let tag = *bytes.get(i).ok_or(TranscriptRefuse::Corrupt)?;
            i += 1;
            match tag {
                t if t == FieldTag::U64 as u8 => {
                    i = i.checked_add(8).ok_or(TranscriptRefuse::Corrupt)?;
                    if i > bytes.len() {
                        return Err(TranscriptRefuse::Corrupt);
                    }
                }
                t if t == FieldTag::Bytes as u8 => {
                    let len = read_u32(bytes, &mut i)?;
                    if len > MAX_BYTES_FIELD {
                        return Err(TranscriptRefuse::LengthBoundExceeded);
                    }
                    i = i
                        .checked_add(len as usize)
                        .ok_or(TranscriptRefuse::Corrupt)?;
                    if i > bytes.len() {
                        return Err(TranscriptRefuse::Corrupt);
                    }
                }
                t if t == FieldTag::Digest32 as u8 => {
                    i = i.checked_add(32).ok_or(TranscriptRefuse::Corrupt)?;
                    if i > bytes.len() {
                        return Err(TranscriptRefuse::Corrupt);
                    }
                }
                t if t == FieldTag::OptionalAbsent as u8 => {}
                t if t == FieldTag::Map as u8 => {
                    skip_map(bytes, &mut i, 0)?;
                }
                _ => return Err(TranscriptRefuse::Corrupt),
            }
        }
        if i != bytes.len() {
            return Err(TranscriptRefuse::Corrupt);
        }
        Ok(Self {
            bytes: bytes.to_vec(),
        })
    }
}

fn is_known_version(version: FormatVersion) -> bool {
    version == FormatVersion::CURRENT
}

fn read_u16(bytes: &[u8], i: &mut usize) -> Result<u16, TranscriptRefuse> {
    let end = i.checked_add(2).ok_or(TranscriptRefuse::Corrupt)?;
    let slice = bytes.get(*i..end).ok_or(TranscriptRefuse::Corrupt)?;
    *i = end;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], i: &mut usize) -> Result<u32, TranscriptRefuse> {
    let end = i.checked_add(4).ok_or(TranscriptRefuse::Corrupt)?;
    let slice = bytes.get(*i..end).ok_or(TranscriptRefuse::Corrupt)?;
    *i = end;
    Ok(u32::from_be_bytes([
        slice[0], slice[1], slice[2], slice[3],
    ]))
}

fn skip_map(bytes: &[u8], i: &mut usize, depth: u8) -> Result<(), TranscriptRefuse> {
    if depth >= MAX_MAP_DEPTH {
        return Err(TranscriptRefuse::RecursionLimitExceeded);
    }
    let count = read_u32(bytes, i)?;
    if count > MAX_MAP_ENTRIES {
        return Err(TranscriptRefuse::LengthBoundExceeded);
    }
    let mut prev: Option<Vec<u8>> = None;
    for _ in 0..count {
        let key_len = read_u16(bytes, i)? as usize;
        if key_len as u32 > MAX_BYTES_FIELD {
            return Err(TranscriptRefuse::LengthBoundExceeded);
        }
        let end = i.checked_add(key_len).ok_or(TranscriptRefuse::Corrupt)?;
        let key = bytes.get(*i..end).ok_or(TranscriptRefuse::Corrupt)?.to_vec();
        *i = end;
        if let Some(ref p) = prev {
            match key.as_slice().cmp(p.as_slice()) {
                std::cmp::Ordering::Equal => return Err(TranscriptRefuse::DuplicateMapKey),
                std::cmp::Ordering::Less => return Err(TranscriptRefuse::MapOrderViolated),
                std::cmp::Ordering::Greater => {}
            }
        }
        prev = Some(key);
        let tag = *bytes.get(*i).ok_or(TranscriptRefuse::Corrupt)?;
        *i += 1;
        match tag {
            t if t == FieldTag::U64 as u8 => {
                *i = i.checked_add(8).ok_or(TranscriptRefuse::Corrupt)?;
            }
            t if t == FieldTag::Bytes as u8 => {
                let len = read_u32(bytes, i)?;
                if len > MAX_BYTES_FIELD {
                    return Err(TranscriptRefuse::LengthBoundExceeded);
                }
                *i = i
                    .checked_add(len as usize)
                    .ok_or(TranscriptRefuse::Corrupt)?;
            }
            t if t == FieldTag::Digest32 as u8 => {
                *i = i.checked_add(32).ok_or(TranscriptRefuse::Corrupt)?;
            }
            t if t == FieldTag::Map as u8 => {
                skip_map(bytes, i, depth + 1)?;
            }
            _ => return Err(TranscriptRefuse::Corrupt),
        }
        if *i > bytes.len() {
            return Err(TranscriptRefuse::Corrupt);
        }
    }
    Ok(())
}

/// Encode the normative fixture transcript for a sealed artifact kind.
///
/// Golden vectors under `store/golden/` are the authority; this encoder must
/// match them. Changing fixture bytes requires a FormatVersion decision in the
/// vector file header (§81) — never a silent test-fix commit.
pub fn encode_golden_fixture(kind: SealedArtifactKind) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = CanonicalTranscriptBuilder::new(FormatVersion::CURRENT)?;
    b.append_u64(FieldId::ARTIFACT_KIND, kind.tag())?;
    b.append_bytes(FieldId::FORMAT_VERSION, &FormatVersion::CURRENT.as_bytes())?;
    let primary = fixture_digest(kind, 1);
    let secondary = fixture_digest(kind, 2);
    b.append_digest32(FieldId::PRIMARY_DIGEST, &primary)?;
    b.append_digest32(FieldId::SECONDARY_DIGEST, &secondary)?;
    b.append_bytes(FieldId::DOMAIN_LABEL, kind_domain_label(kind))?;
    Ok(b.seal())
}

fn kind_domain_label(kind: SealedArtifactKind) -> &'static [u8] {
    match kind {
        SealedArtifactKind::CheckpointSeal => b"checkpoint-seal",
        SealedArtifactKind::AdmissionCertificate => b"admission-certificate",
        SealedArtifactKind::ForkGrant => b"fork-grant",
        SealedArtifactKind::RecoveryGrant => b"recovery-grant",
        SealedArtifactKind::MergeProofHeader => b"merge-proof-header",
        SealedArtifactKind::AuditKeyLeaf => b"audit-key-leaf",
        SealedArtifactKind::WalHeader => b"wal-header",
    }
}

fn fixture_digest(kind: SealedArtifactKind, lane: u8) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[0] = kind.tag() as u8;
    out[1] = lane;
    for (i, b) in out.iter_mut().enumerate().skip(2) {
        *b = ((kind.tag() as u8).wrapping_mul(17))
            .wrapping_add(lane)
            .wrapping_add(i as u8);
    }
    out
}

/// Parse a golden vector file body (header lines + hex payload).
pub fn parse_golden_hex(file: &str) -> Result<Vec<u8>, TranscriptRefuse> {
    let mut hex = String::new();
    for line in file.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        for ch in line.chars() {
            if !ch.is_ascii_whitespace() {
                hex.push(ch);
            }
        }
    }
    if hex.len() % 2 != 0 {
        return Err(TranscriptRefuse::Corrupt);
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let hi = from_hex(bytes[i])?;
        let lo = from_hex(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn from_hex(b: u8) -> Result<u8, TranscriptRefuse> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(TranscriptRefuse::Corrupt),
    }
}

#[cfg(test)]
mod pins {
    use super::*;

    fn assert_matches_golden(kind: SealedArtifactKind, file: &str) {
        let encoded = encode_golden_fixture(kind).expect("fixture encodes");
        let expected = parse_golden_hex(file).expect("golden parses");
        assert_eq!(
            encoded.as_bytes(),
            expected.as_slice(),
            "implementation must match golden vector for {kind:?} — not the reverse"
        );
        let parsed = CanonicalTranscript::parse(encoded.as_bytes()).expect("round-trip");
        assert_eq!(parsed.as_bytes(), encoded.as_bytes());
    }

    #[test]
    fn golden_vectors_match_encoder() {
        assert_matches_golden(
            SealedArtifactKind::CheckpointSeal,
            include_str!("golden/checkpoint_seal.vec"),
        );
        assert_matches_golden(
            SealedArtifactKind::AdmissionCertificate,
            include_str!("golden/admission_certificate.vec"),
        );
        assert_matches_golden(
            SealedArtifactKind::ForkGrant,
            include_str!("golden/fork_grant.vec"),
        );
        assert_matches_golden(
            SealedArtifactKind::RecoveryGrant,
            include_str!("golden/recovery_grant.vec"),
        );
        assert_matches_golden(
            SealedArtifactKind::MergeProofHeader,
            include_str!("golden/merge_proof_header.vec"),
        );
        assert_matches_golden(
            SealedArtifactKind::AuditKeyLeaf,
            include_str!("golden/audit_key_leaf.vec"),
        );
        assert_matches_golden(SealedArtifactKind::WalHeader, include_str!("golden/wal_header.vec"));
    }

    #[test]
    fn unknown_version_refuses() {
        // Craft a transcript header with version "999" (canonical spelling, unknown).
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"KTX1");
        bytes.push(3);
        bytes.extend_from_slice(b"999");
        bytes.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(
            CanonicalTranscript::parse(&bytes),
            Err(TranscriptRefuse::UnknownVersion)
        );
    }

    #[test]
    fn duplicate_map_key_refuses() {
        let mut b = CanonicalTranscriptBuilder::new(FormatVersion::CURRENT).unwrap();
        let entries = vec![
            (b"a".to_vec(), MapValue::U64(1)),
            (b"a".to_vec(), MapValue::U64(2)),
        ];
        assert_eq!(
            b.append_map(FieldId::BINDINGS_MAP, &entries),
            Err(TranscriptRefuse::DuplicateMapKey)
        );
    }

    #[test]
    fn length_bound_checked_before_alloc() {
        let mut b = CanonicalTranscriptBuilder::new(FormatVersion::CURRENT).unwrap();
        // Claim would require > MAX_BYTES_FIELD — refuse without allocating that.
        // We simulate by appending a small slice then checking the bound path via
        // a hand-crafted corrupt length on parse.
        let mut bytes = encode_golden_fixture(SealedArtifactKind::WalHeader)
            .unwrap()
            .as_bytes()
            .to_vec();
        // Corrupt: turn into a Bytes field with huge length at end — easier to
        // unit-test the builder bound directly:
        let huge = vec![0u8; (MAX_BYTES_FIELD as usize) + 1];
        assert_eq!(
            b.append_bytes(FieldId::DOMAIN_LABEL, &huge),
            Err(TranscriptRefuse::LengthBoundExceeded)
        );
        let _ = bytes; // silence
    }
}
