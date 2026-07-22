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
//! unknown-version refuse door, deep sealed-artifact residual-secret scrub
//! ([`refuse_residual_secret_bytes`] / §64/§65).
//!
//! Bans: sealed Unicode normalization surfaces (bytes and typed ids only);
//! map encodings without duplicate-key refusal; allocation before bounds
//! checks; a second serialization path for sealed artifacts; residual
//! DEK / KEK / plaintext ShredSalt bytes surviving in sealed transcript bytes
//! after shred.
//!
//! Encoding checklist as law: field ids and ordering; integer width /
//! signedness / endianness; optional/default encoding; length bounds and
//! recursion limits before allocation; hash / signature / key-id / domain-label
//! encodings; AEAD nonce and AAD transcript fields; unknown version refuses;
//! migration epochs anchor upgrades via [`FormatVersion`].

use super::contract::FormatVersion;
use super::epoch::{CryptoDomain, FenceEpoch};
use super::open::StoreId;

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

/// Closed sealed-artifact kinds under the one CanonicalTranscript law (§59).
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
    /// AEAD key-commitment (CMT-1) input — domain-label + key-id + CryptoDomain (§59).
    KeyCommit = 8,
    /// Compact StateRootHead for STH gossip / signing body (§2/§56/§58/§69).
    StateRootHead = 9,
    /// Leave-is-free pack content root (§65/§79).
    LeaveIsFreePack = 10,
    /// Chained state-root bind (predecessor ‖ content ‖ link ‖ ordinal).
    ChainedStateRoot = 11,
    /// AncestorReadGrant decrypt-scope payload (§68).
    AncestorReadGrant = 12,
    /// WrappedShredSalt KEK-wrap AAD (§59 / shred-salt wrap).
    WrappedShredSalt = 13,
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
    /// Ordered map of typed bindings.
    pub const BINDINGS_MAP: FieldId = FieldId(7);
    /// FenceEpoch counter half of [`CryptoDomain`] (key-commitment / domain bind).
    pub const FENCE_EPOCH: FieldId = FieldId(8);

    // ── CheckpointSeal real schema (kyzo.checkpoint_seal.v1 field order) ──
    /// StoreId bound into the seal.
    pub const STORE_ID: FieldId = FieldId(9);
    /// CryptoDomain.store_id.
    pub const CRYPTO_DOMAIN_STORE_ID: FieldId = FieldId(10);
    /// CryptoDomain.fence_epoch counter.
    pub const CRYPTO_DOMAIN_FENCE_EPOCH: FieldId = FieldId(11);
    /// CryptoDomain.fence_epoch.store_id.
    pub const CRYPTO_DOMAIN_FENCE_STORE_ID: FieldId = FieldId(12);
    /// Cut FenceEpoch counter (must equal CryptoDomain fence under seal law).
    pub const CUT_FENCE_EPOCH: FieldId = FieldId(13);
    /// Cut FenceEpoch.store_id.
    pub const CUT_FENCE_STORE_ID: FieldId = FieldId(14);
    /// Dense CommitOrdinal at the cut.
    pub const CUT_ORDINAL: FieldId = FieldId(15);
    /// Plaintext-canonical state root at the cut.
    pub const STATE_ROOT: FieldId = FieldId(16);
    /// Final WAL hash of the covered prefix.
    pub const FINAL_WAL_HASH: FieldId = FieldId(17);
    /// Checkpoint manifest digest.
    pub const CHECKPOINT_MANIFEST: FieldId = FieldId(18);
    /// Catalog generation at cut.
    pub const CATALOG_GENERATION: FieldId = FieldId(19);
    /// Retained-object manifest digest.
    pub const RETAINED_OBJECT_MANIFEST: FieldId = FieldId(20);
    /// Live PermanenceCandidate manifest digest.
    pub const PERMANENCE_CANDIDATE_MANIFEST: FieldId = FieldId(21);
    /// ReplicaCustody / retained-certificate manifest digest.
    pub const REPLICA_CUSTODY_MANIFEST: FieldId = FieldId(22);
    /// NonceLease Commit-domain floor.
    pub const NONCE_FLOOR_COMMIT: FieldId = FieldId(23);
    /// NonceLease Compact-domain floor.
    pub const NONCE_FLOOR_COMPACT: FieldId = FieldId(24);
    /// NonceLease Rotate-domain floor.
    pub const NONCE_FLOOR_ROTATE: FieldId = FieldId(25);
    /// IncarnationId.open_ordinal at the history boundary.
    pub const INCARNATION_OPEN_ORDINAL: FieldId = FieldId(26);
    /// IncarnationId.entropy at the history boundary.
    pub const INCARNATION_ENTROPY: FieldId = FieldId(27);
    /// Prior seal digest (or genesis).
    pub const PRIOR_SEAL_DIGEST: FieldId = FieldId(28);
    /// Retention certificate digest covering as-of/snapshot obligations.
    pub const RETENTION_CERTIFICATE_DIGEST: FieldId = FieldId(29);

    // ── MergeProof / grant / WAL / STH / pack shared schema fields ──
    /// One input packet content hash (repeatable, non-decreasing).
    pub const INPUT_CONTENT_HASH: FieldId = FieldId(30);
    /// Lineage hash.
    pub const LINEAGE_HASH: FieldId = FieldId(31);
    /// GrantId.
    pub const GRANT_ID: FieldId = FieldId(34);
    /// Predecessor / named StoreId in grant payloads.
    pub const PREDECESSOR_STORE: FieldId = FieldId(35);
    /// Fork-point state root.
    pub const FORK_POINT_ROOT: FieldId = FieldId(36);
    /// Successor principal binding.
    pub const SUCCESSOR_PRINCIPAL: FieldId = FieldId(37);
    /// Identity seed (fork / recovery successor entropy).
    pub const IDENTITY_SEED: FieldId = FieldId(38);
    /// Key-material commitment.
    pub const KEY_MATERIAL_COMMITMENT: FieldId = FieldId(39);
    /// Predecessor FenceEpoch counter (recovery).
    pub const PREDECESSOR_EPOCH: FieldId = FieldId(40);
    /// Predecessor FenceEpoch.store_id (recovery).
    pub const PREDECESSOR_EPOCH_STORE_ID: FieldId = FieldId(41);
    /// RecoveryMatrix threshold (min_signers).
    pub const MATRIX_THRESHOLD: FieldId = FieldId(42);
    /// RecoveryMatrix max_signers.
    pub const MATRIX_MAX_SIGNERS: FieldId = FieldId(43);
    /// FROST group verifying key bytes.
    pub const GROUP_VERIFYING_KEY: FieldId = FieldId(44);
    /// ed25519 verifying key bytes (consent / entitlement key-id digests).
    pub const VERIFYING_KEY: FieldId = FieldId(45);
    /// Ancestor-read from-epoch counter.
    pub const FROM_EPOCH: FieldId = FieldId(46);
    /// Ancestor-read from-epoch store id.
    pub const FROM_EPOCH_STORE_ID: FieldId = FieldId(47);
    /// Ancestor-read to-epoch counter.
    pub const TO_EPOCH: FieldId = FieldId(48);
    /// Ancestor-read to-epoch store id.
    pub const TO_EPOCH_STORE_ID: FieldId = FieldId(49);
    /// WAL predecessor hash.
    pub const PREDECESSOR_HASH: FieldId = FieldId(50);
    /// WAL payload kind tag bytes (`commit` / `nonce_floor` / `incarnation`).
    pub const PAYLOAD_KIND: FieldId = FieldId(51);
    /// CommitOrdinal / commit-payload ordinal.
    pub const COMMIT_ORDINAL: FieldId = FieldId(52);
    /// Opaque payload body bytes.
    pub const PAYLOAD_BODY: FieldId = FieldId(53);
    /// MintDomain discriminant (1=Commit, 2=Compact, 3=Rotate).
    pub const MINT_DOMAIN: FieldId = FieldId(54);
    /// DomainCounter ceiling / counter value.
    pub const DOMAIN_COUNTER: FieldId = FieldId(55);
    /// Content root (chain_bind) — before predecessor / link / ordinal.
    pub const CONTENT_ROOT: FieldId = FieldId(56);
    /// Predecessor root (chain_bind).
    pub const PREDECESSOR_ROOT: FieldId = FieldId(57);
    /// ChainLinkKind discriminant (1=Ordinary, 2=Recovery, 3=Fork).
    pub const CHAIN_LINK_KIND: FieldId = FieldId(58);
    /// Commit ordinal inside a chained-state-root bind (after link kind).
    pub const CHAIN_COMMIT_ORDINAL: FieldId = FieldId(81);
    /// WAL incarnation open ordinal (after [`PAYLOAD_KIND`]).
    pub const WAL_INCARNATION_ORDINAL: FieldId = FieldId(82);
    /// WAL incarnation entropy (after [`WAL_INCARNATION_ORDINAL`]).
    pub const WAL_INCARNATION_ENTROPY: FieldId = FieldId(83);
    /// Leave-is-free pack kind tag bytes.
    pub const PACK_KIND: FieldId = FieldId(59);
    /// Count of wrapped shred salts (before repeated salt field group).
    pub const WRAPPED_SALT_COUNT: FieldId = FieldId(60);
    /// Per-salt StoreId (repeatable group; equal FieldId across salts).
    pub const SALT_STORE_ID: FieldId = FieldId(61);
    /// Per-salt fence-epoch counter (repeatable).
    pub const SALT_FENCE_EPOCH: FieldId = FieldId(62);
    /// Per-salt SegmentCounter (repeatable).
    pub const SEGMENT_COUNTER: FieldId = FieldId(63);
    /// Per-salt ciphertext bytes (repeatable; length-prefixed).
    pub const CIPHERTEXT: FieldId = FieldId(64);
    /// Count of incarnation-history entries.
    pub const INCARNATION_COUNT: FieldId = FieldId(65);
    /// Per-incarnation open ordinal (repeatable).
    pub const PACK_INCARNATION_ORDINAL: FieldId = FieldId(66);
    /// Per-incarnation entropy (repeatable).
    pub const PACK_INCARNATION_ENTROPY: FieldId = FieldId(67);
    /// Admission protocol version tag (8 bytes).
    pub const PROTOCOL_VERSION: FieldId = FieldId(68);
    /// Origin fence epoch counter (admission).
    pub const ORIGIN_EPOCH: FieldId = FieldId(69);
    /// Origin commit ordinal (admission).
    pub const ORIGIN_COMMIT: FieldId = FieldId(70);
    /// Schema cut digest.
    pub const SCHEMA_CUT: FieldId = FieldId(71);
    /// Record digest (admission).
    pub const RECORD_DIGEST: FieldId = FieldId(72);
    /// Predecessor history digest (admission).
    pub const PREDECESSOR_HISTORY_DIGEST: FieldId = FieldId(73);
    /// Post-state root (admission).
    pub const POST_STATE_ROOT: FieldId = FieldId(74);
    /// Authorizing key id (admission).
    pub const AUTHORIZING_KEY_ID: FieldId = FieldId(75);
    /// Scope manifest digest (admission).
    pub const SCOPE_MANIFEST_DIGEST: FieldId = FieldId(76);
    /// Origin fence epoch store id (admission genesis bind).
    pub const ORIGIN_EPOCH_STORE_ID: FieldId = FieldId(77);
    /// Optional OperationKey (admission); absent → [`FieldTag::OptionalAbsent`].
    pub const OPERATION_KEY: FieldId = FieldId(78);
    /// Admission signature bytes.
    pub const SIGNATURE: FieldId = FieldId(79);
    /// Leave-is-free pack opaque payload bytes (after salt/incarnation groups).
    pub const PACK_PAYLOAD: FieldId = FieldId(80);
    /// MergeProof state root (after input hashes + lineage; not [`STATE_ROOT`]).
    pub const MERGE_STATE_ROOT: FieldId = FieldId(84);
    /// MergeProof compact-domain counter (after [`MERGE_STATE_ROOT`]).
    pub const MERGE_COMPACT_COUNTER: FieldId = FieldId(85);
    /// MergeProof output content hash (after [`MERGE_COMPACT_COUNTER`]).
    pub const MERGE_OUTPUT_CONTENT_HASH: FieldId = FieldId(86);

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
    /// Corrupt / truncated encoding — also residual DEK / KEK / plaintext
    /// ShredSalt surviving in sealed bytes after shred (§64/§65 deep scrub).
    /// No separate refuse variant: dst exhaustive matches must stay green.
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
        if let Some(last) = self.last_field_id
            && id.get() < last.get()
        {
            return Err(TranscriptRefuse::FieldOrderViolated);
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

    /// Deep sealed-artifact scrub (§64/§65): refuse if any shredded secret
    /// needle (DEK / KEK / plaintext ShredSalt bytes) is still reachable inside
    /// these sealed transcript bytes.
    ///
    /// Empty needles are a no-op Ok. Needle length zero is ignored (not a
    /// secret). Residual hits refuse as [`TranscriptRefuse::Corrupt`] — the
    /// production door the crypto-shred reachability campaign exercises.
    pub fn refuse_residual_secrets(
        &self,
        shredded_secret_needles: &[&[u8]],
    ) -> Result<(), TranscriptRefuse> {
        refuse_residual_secret_bytes(self.as_bytes(), shredded_secret_needles)
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
            if let Some(prev) = last_id
                && id < prev
            {
                return Err(TranscriptRefuse::FieldOrderViolated);
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
                unknown_tag => {
                    drop(unknown_tag);
                    return Err(TranscriptRefuse::Corrupt);
                }
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

/// Closed list of sealed artifact kinds the deep reachability campaign must
/// search. Includes [`SealedArtifactKind::KeyCommit`] (CMT-1 intact). Order is
/// stable for DST replay.
pub const SEALED_ARTIFACT_KINDS: &[SealedArtifactKind] = &[
    SealedArtifactKind::CheckpointSeal,
    SealedArtifactKind::AdmissionCertificate,
    SealedArtifactKind::ForkGrant,
    SealedArtifactKind::RecoveryGrant,
    SealedArtifactKind::MergeProofHeader,
    SealedArtifactKind::AuditKeyLeaf,
    SealedArtifactKind::WalHeader,
    SealedArtifactKind::KeyCommit,
    SealedArtifactKind::StateRootHead,
    SealedArtifactKind::LeaveIsFreePack,
    SealedArtifactKind::ChainedStateRoot,
    SealedArtifactKind::AncestorReadGrant,
    SealedArtifactKind::WrappedShredSalt,
];

/// Production scrub over arbitrary sealed-artifact bytes (§64/§65).
///
/// Refuses when any non-empty needle from `shredded_secret_needles` appears as
/// a contiguous substring of `sealed_bytes`. Used by transcript parse sites,
/// CheckpointSeal encode, and the crypto-shred deep reachability DST.
/// Residual hit → [`TranscriptRefuse::Corrupt`] (no new refuse variant).
pub fn refuse_residual_secret_bytes(
    sealed_bytes: &[u8],
    shredded_secret_needles: &[&[u8]],
) -> Result<(), TranscriptRefuse> {
    for needle in shredded_secret_needles {
        if needle.is_empty() {
            continue;
        }
        if sealed_bytes.len() >= needle.len()
            && sealed_bytes
                .windows(needle.len())
                .any(|window| window == *needle)
        {
            return Err(TranscriptRefuse::Corrupt);
        }
    }
    Ok(())
}

/// Deep reachability scrub over caller-supplied sealed transcripts (one per
/// kind under test). Callers pass real [`CanonicalTranscript`] bytes from the
/// typed production encoders (golden vectors under `store/golden/`).
pub fn refuse_residual_secrets_in_all_sealed_kinds(
    sealed_transcripts: &[CanonicalTranscript],
    shredded_secret_needles: &[&[u8]],
) -> Result<(), TranscriptRefuse> {
    // Exhaustiveness: adding a SealedArtifactKind without updating this match
    // fails to compile — keep SEALED_ARTIFACT_KINDS in lockstep.
    fn kinds_complete(kind: SealedArtifactKind) {
        match kind {
            SealedArtifactKind::CheckpointSeal
            | SealedArtifactKind::AdmissionCertificate
            | SealedArtifactKind::ForkGrant
            | SealedArtifactKind::RecoveryGrant
            | SealedArtifactKind::MergeProofHeader
            | SealedArtifactKind::AuditKeyLeaf
            | SealedArtifactKind::WalHeader
            | SealedArtifactKind::KeyCommit
            | SealedArtifactKind::StateRootHead
            | SealedArtifactKind::LeaveIsFreePack
            | SealedArtifactKind::ChainedStateRoot
            | SealedArtifactKind::AncestorReadGrant
            | SealedArtifactKind::WrappedShredSalt => {}
        }
    }
    for kind in SEALED_ARTIFACT_KINDS {
        kinds_complete(*kind);
    }
    for transcript in sealed_transcripts {
        transcript.refuse_residual_secrets(shredded_secret_needles)?;
    }
    Ok(())
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
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
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
        let key = bytes
            .get(*i..end)
            .ok_or(TranscriptRefuse::Corrupt)?
            .to_vec();
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
            unknown_tag => {
                drop(unknown_tag);
                return Err(TranscriptRefuse::Corrupt);
            }
        }
        if *i > bytes.len() {
            return Err(TranscriptRefuse::Corrupt);
        }
    }
    Ok(())
}

/// Domain label for CMT-1 key-commitment transcripts (seat 59 / confidentiality).
pub const KEY_COMMIT_DOMAIN_LABEL: &[u8] = b"KEY_COMMIT";
/// Domain label for CheckpointSeal binding digests.
pub const CHECKPOINT_SEAL_DOMAIN_LABEL: &[u8] = b"kyzo.checkpoint_seal.v1";
/// Domain label for MergeProof sealed identity.
pub const MERGE_PROOF_DOMAIN_LABEL: &[u8] = b"kyzo.merge_proof.v1";
/// Domain label for ForkGrant payload digests.
pub const FORK_GRANT_PAYLOAD_DOMAIN_LABEL: &[u8] = b"kyzo.fork_grant.payload.v1";
/// Domain label for RecoveryGrant payload digests.
pub const RECOVERY_GRANT_PAYLOAD_DOMAIN_LABEL: &[u8] = b"kyzo.recovery_grant.payload.v1";
/// Domain label for RecoveryMatrix digests.
pub const RECOVERY_MATRIX_DOMAIN_LABEL: &[u8] = b"kyzo.recovery_matrix.v1";
/// Domain label for fork-consent verifying-key digests.
pub const FORK_CONSENT_KEY_ID_DOMAIN_LABEL: &[u8] = b"kyzo.fork_consent.key_id.v1";
/// Domain label for ancestor-entitlement verifying-key digests.
pub const ANCESTOR_ENTITLEMENT_KEY_ID_DOMAIN_LABEL: &[u8] = b"kyzo.ancestor_entitlement.key_id.v1";
/// Domain label for AncestorReadGrant payload digests.
pub const ANCESTOR_READ_GRANT_PAYLOAD_DOMAIN_LABEL: &[u8] = b"kyzo.ancestor_read_grant.payload.v1";
/// Domain label for fork successor StoreId derivation.
pub const FORK_STORE_ID_DOMAIN_LABEL: &[u8] = b"kyzo.store_id.fork.v1";
/// Domain label for fork WriteAuthority token derivation.
pub const FORK_WRITE_TOKEN_DOMAIN_LABEL: &[u8] = b"kyzo.write_authority.fork.v1";
/// Domain label for recovery WriteAuthority token derivation.
pub const RECOVERY_WRITE_TOKEN_DOMAIN_LABEL: &[u8] = b"kyzo.write_authority.recovery.v1";
/// Domain label for WAL record hashes.
pub const WAL_RECORD_DOMAIN_LABEL: &[u8] = b"kyzo.wal.record.v1";
/// Domain label for StateRootHead compact digests.
pub const STATE_ROOT_HEAD_DOMAIN_LABEL: &[u8] = b"kyzo.state_root_head.v1";
/// Domain label for chained state-root binds.
pub const CHAINED_STATE_ROOT_DOMAIN_LABEL: &[u8] = b"kyzo.chained_state_root.v1";
/// Domain label for leave-is-free pack content roots.
pub const LEAVE_IS_FREE_PACK_DOMAIN_LABEL: &[u8] = b"kyzo.leave_is_free.pack.root.v1";
/// Domain label for AuditKeyLeaf subject transcripts.
pub const AUDIT_KEY_LEAF_DOMAIN_LABEL: &[u8] = b"kyzo.audit_key_leaf.v1";
/// Domain label for AdmissionCertificate envelopes.
pub const ADMISSION_CERTIFICATE_DOMAIN_LABEL: &[u8] = b"kyzo.admission_certificate.v1";
/// Domain label for WrappedShredSalt KEK-wrap AAD (former raw `"WSS1"` prefix).
pub const WRAPPED_SHRED_SALT_AAD_DOMAIN_LABEL: &[u8] = b"WSS1";

/// Independently derived golden for [`SealedArtifactKind::KeyCommit`] (seat 59).
///
/// Hex is hand-derived from the CanonicalTranscript wire format
/// (MAGIC/version/field tags/be u64/digest32/bytes) for the normative fixture —
/// never captured by calling [`encode_key_commitment`] and pasting its output.
/// Production [`encode_key_commitment`] must match this derivation.
///
/// Normative fixture: key-id starts `08 01 8b…`, store-id starts `02 8c 8d…`,
/// fence_epoch = 0, domain label `KEY_COMMIT`.
pub const KEY_COMMIT_GOLDEN_VEC: &str = r#"
# FormatVersion: 6
# Kind: KeyCommit
# Decision: seat-59 — independently derived wire bytes (not captured from production encoder)
4b5458310136000000060001010000000000000008000202000000013600030308018b8c8d8e8f909192939495969798999a9b9c9d9e9fa0a1a2a3a4a5a6a7a800040308028c8d8e8f909192939495969798999a9b9c9d9e9fa0a1a2a3a4a5a6a7a8a90005020000000a4b45595f434f4d4d49540008010000000000000000
"#;

/// Independently derived golden for [`SealedArtifactKind::WrappedShredSalt`] (seat 59).
///
/// Hex is hand-derived from the CanonicalTranscript wire format for the normative
/// fixture — never captured by calling [`encode_wrapped_shred_salt_aad`].
/// Production [`encode_wrapped_shred_salt_aad`] must match this derivation.
///
/// Normative fixture: store-id `[0x11; 32]`, fence_epoch = 0, segment = 0,
/// domain label `WSS1`.
pub const WRAPPED_SHRED_SALT_AAD_GOLDEN_VEC: &str = r#"
# FormatVersion: 6
# Kind: WrappedShredSalt
# Decision: seat-59 — independently derived wire bytes (not captured from production encoder)
4b545831013600000006000101000000000000000d00020200000001360005020000000457535331003d031111111111111111111111111111111111111111111111111111111111111111003e010000000000000000003f010000000000000000
"#;

/// Open a builder with ARTIFACT_KIND + FORMAT_VERSION + DOMAIN_LABEL — the shared
/// header every production sealed-artifact encoder uses.
fn begin_sealed_artifact(
    kind: SealedArtifactKind,
    format_version: FormatVersion,
    domain_label: &[u8],
) -> Result<CanonicalTranscriptBuilder, TranscriptRefuse> {
    let mut b = CanonicalTranscriptBuilder::new(format_version)?;
    b.append_u64(FieldId::ARTIFACT_KIND, kind.tag())?;
    b.append_bytes(FieldId::FORMAT_VERSION, &format_version.as_bytes())?;
    b.append_bytes(FieldId::DOMAIN_LABEL, domain_label)?;
    Ok(b)
}

/// Mint the CMT-1 key-commitment transcript: domain-label + key-id + CryptoDomain.
///
/// This is the ONE sealed-byte constructor for AEAD key-commitment (seat 59).
/// A hand-rolled `KEY_COMMIT_DOMAIN_v1 ‖ key ‖ …` layout is Unconstructible.
pub fn encode_key_commitment(
    key_id: &[u8; 32],
    crypto_domain: CryptoDomain,
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = CanonicalTranscriptBuilder::new(FormatVersion::CURRENT)?;
    b.append_u64(FieldId::ARTIFACT_KIND, SealedArtifactKind::KeyCommit.tag())?;
    b.append_bytes(FieldId::FORMAT_VERSION, &FormatVersion::CURRENT.as_bytes())?;
    // key-id (DEK / KEK opening key) — PRIMARY_DIGEST field encoding.
    b.append_digest32(FieldId::PRIMARY_DIGEST, key_id)?;
    // CryptoDomain.store_id
    b.append_digest32(
        FieldId::SECONDARY_DIGEST,
        crypto_domain.store_id().as_bytes(),
    )?;
    b.append_bytes(FieldId::DOMAIN_LABEL, KEY_COMMIT_DOMAIN_LABEL)?;
    b.append_u64(FieldId::FENCE_EPOCH, crypto_domain.fence_epoch().get())?;
    Ok(b.seal())
}

/// Mint the WrappedShredSalt KEK-wrap AAD transcript: domain-label + store + epoch + segment.
///
/// This is the ONE sealed-byte constructor for shred-salt wrap AAD (seat 59).
/// A hand-rolled `"WSS1" ‖ store_id ‖ fence_epoch ‖ segment` layout is Unconstructible.
pub fn encode_wrapped_shred_salt_aad(
    crypto_domain: CryptoDomain,
    segment: u64,
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::WrappedShredSalt,
        FormatVersion::CURRENT,
        WRAPPED_SHRED_SALT_AAD_DOMAIN_LABEL,
    )?;
    b.append_digest32(FieldId::SALT_STORE_ID, crypto_domain.store_id().as_bytes())?;
    b.append_u64(FieldId::SALT_FENCE_EPOCH, crypto_domain.fence_epoch().get())?;
    b.append_u64(FieldId::SEGMENT_COUNTER, segment)?;
    Ok(b.seal())
}

/// Real CheckpointSeal field schema (former `digest_parts` / kyzo.checkpoint_seal.v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointSealTranscriptParts {
    /// FormatVersion stamped into the seal.
    pub format_version: FormatVersion,
    /// Store identity.
    pub store_id: [u8; 32],
    /// Crypto domain at the cut.
    pub crypto_domain: CryptoDomain,
    /// Fence epoch at the cut (must match crypto_domain under seal law).
    pub fence_epoch: u64,
    /// Fence epoch StoreId bind.
    pub fence_epoch_store_id: [u8; 32],
    /// Dense commit ordinal at the cut.
    pub cut: u64,
    /// Plaintext-canonical state root.
    pub state_root: [u8; 32],
    /// Final WAL hash of the covered prefix.
    pub final_wal_hash: [u8; 32],
    /// Checkpoint manifest digest.
    pub checkpoint_manifest: [u8; 32],
    /// Catalog generation at cut.
    pub catalog_generation: u64,
    /// Retained-object manifest digest.
    pub retained_object_manifest: [u8; 32],
    /// PermanenceCandidate manifest digest.
    pub permanence_candidate_manifest: [u8; 32],
    /// ReplicaCustody manifest digest.
    pub replica_custody_manifest: [u8; 32],
    /// NonceLease Commit floor.
    pub nonce_floor_commit: u64,
    /// NonceLease Compact floor.
    pub nonce_floor_compact: u64,
    /// NonceLease Rotate floor.
    pub nonce_floor_rotate: u64,
    /// Incarnation open ordinal at the history boundary.
    pub incarnation_open_ordinal: u64,
    /// Incarnation entropy at the history boundary.
    pub incarnation_entropy: [u8; 32],
    /// Prior seal digest (or genesis).
    pub prior_seal_digest: [u8; 32],
    /// Retention certificate digest.
    pub retention_certificate_digest: [u8; 32],
}

/// Encode a CheckpointSeal binding under the one CanonicalTranscript constructor.
pub fn encode_checkpoint_seal(
    parts: &CheckpointSealTranscriptParts,
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::CheckpointSeal,
        parts.format_version,
        CHECKPOINT_SEAL_DOMAIN_LABEL,
    )?;
    b.append_digest32(FieldId::STORE_ID, &parts.store_id)?;
    b.append_digest32(
        FieldId::CRYPTO_DOMAIN_STORE_ID,
        parts.crypto_domain.store_id().as_bytes(),
    )?;
    b.append_u64(
        FieldId::CRYPTO_DOMAIN_FENCE_EPOCH,
        parts.crypto_domain.fence_epoch().get(),
    )?;
    b.append_digest32(
        FieldId::CRYPTO_DOMAIN_FENCE_STORE_ID,
        parts.crypto_domain.fence_epoch().store_id().as_bytes(),
    )?;
    b.append_u64(FieldId::CUT_FENCE_EPOCH, parts.fence_epoch)?;
    b.append_digest32(FieldId::CUT_FENCE_STORE_ID, &parts.fence_epoch_store_id)?;
    b.append_u64(FieldId::CUT_ORDINAL, parts.cut)?;
    b.append_digest32(FieldId::STATE_ROOT, &parts.state_root)?;
    b.append_digest32(FieldId::FINAL_WAL_HASH, &parts.final_wal_hash)?;
    b.append_digest32(FieldId::CHECKPOINT_MANIFEST, &parts.checkpoint_manifest)?;
    b.append_u64(FieldId::CATALOG_GENERATION, parts.catalog_generation)?;
    b.append_digest32(
        FieldId::RETAINED_OBJECT_MANIFEST,
        &parts.retained_object_manifest,
    )?;
    b.append_digest32(
        FieldId::PERMANENCE_CANDIDATE_MANIFEST,
        &parts.permanence_candidate_manifest,
    )?;
    b.append_digest32(
        FieldId::REPLICA_CUSTODY_MANIFEST,
        &parts.replica_custody_manifest,
    )?;
    b.append_u64(FieldId::NONCE_FLOOR_COMMIT, parts.nonce_floor_commit)?;
    b.append_u64(FieldId::NONCE_FLOOR_COMPACT, parts.nonce_floor_compact)?;
    b.append_u64(FieldId::NONCE_FLOOR_ROTATE, parts.nonce_floor_rotate)?;
    b.append_u64(
        FieldId::INCARNATION_OPEN_ORDINAL,
        parts.incarnation_open_ordinal,
    )?;
    b.append_digest32(FieldId::INCARNATION_ENTROPY, &parts.incarnation_entropy)?;
    b.append_digest32(FieldId::PRIOR_SEAL_DIGEST, &parts.prior_seal_digest)?;
    b.append_digest32(
        FieldId::RETENTION_CERTIFICATE_DIGEST,
        &parts.retention_certificate_digest,
    )?;
    Ok(b.seal())
}

/// Encode a MergeProof sealed-identity transcript (former `sealed_identity_digest`).
pub fn encode_merge_proof_header(
    input_content_hashes: &[[u8; 32]],
    lineage_hash: &[u8; 32],
    state_root: &[u8; 32],
    compact_counter: u64,
    output_content_hash: &[u8; 32],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::MergeProofHeader,
        FormatVersion::CURRENT,
        MERGE_PROOF_DOMAIN_LABEL,
    )?;
    for content in input_content_hashes {
        b.append_digest32(FieldId::INPUT_CONTENT_HASH, content)?;
    }
    b.append_digest32(FieldId::LINEAGE_HASH, lineage_hash)?;
    b.append_digest32(FieldId::MERGE_STATE_ROOT, state_root)?;
    b.append_u64(FieldId::MERGE_COMPACT_COUNTER, compact_counter)?;
    b.append_digest32(FieldId::MERGE_OUTPUT_CONTENT_HASH, output_content_hash)?;
    Ok(b.seal())
}

/// Encode a ForkGrant payload transcript (former `fork_grant_payload_digest`).
pub fn encode_fork_grant_payload(
    grant_id: &[u8; 32],
    predecessor_store: &[u8; 32],
    fork_point_root: &[u8; 32],
    successor_principal: &[u8; 32],
    identity_seed: &[u8; 32],
    key_material_commitment: &[u8; 32],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::ForkGrant,
        FormatVersion::CURRENT,
        FORK_GRANT_PAYLOAD_DOMAIN_LABEL,
    )?;
    b.append_digest32(FieldId::GRANT_ID, grant_id)?;
    b.append_digest32(FieldId::PREDECESSOR_STORE, predecessor_store)?;
    b.append_digest32(FieldId::FORK_POINT_ROOT, fork_point_root)?;
    b.append_digest32(FieldId::SUCCESSOR_PRINCIPAL, successor_principal)?;
    b.append_digest32(FieldId::IDENTITY_SEED, identity_seed)?;
    b.append_digest32(FieldId::KEY_MATERIAL_COMMITMENT, key_material_commitment)?;
    Ok(b.seal())
}

/// Encode a RecoveryGrant payload transcript (former `recovery_grant_payload_digest`).
pub fn encode_recovery_grant_payload(
    grant_id: &[u8; 32],
    store_id: &[u8; 32],
    predecessor_epoch: u64,
    predecessor_epoch_store_id: &[u8; 32],
    successor_identity_seed: &[u8; 32],
    key_material_commitment: &[u8; 32],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::RecoveryGrant,
        FormatVersion::CURRENT,
        RECOVERY_GRANT_PAYLOAD_DOMAIN_LABEL,
    )?;
    // FieldIds must be non-decreasing: named store + identity/key before epoch pair.
    b.append_digest32(FieldId::GRANT_ID, grant_id)?;
    b.append_digest32(FieldId::PREDECESSOR_STORE, store_id)?;
    b.append_digest32(FieldId::IDENTITY_SEED, successor_identity_seed)?;
    b.append_digest32(FieldId::KEY_MATERIAL_COMMITMENT, key_material_commitment)?;
    b.append_u64(FieldId::PREDECESSOR_EPOCH, predecessor_epoch)?;
    b.append_digest32(
        FieldId::PREDECESSOR_EPOCH_STORE_ID,
        predecessor_epoch_store_id,
    )?;
    Ok(b.seal())
}

/// Encode a RecoveryMatrix digest transcript (former `recovery_matrix_digest`).
pub fn encode_recovery_matrix(
    threshold: u32,
    max_signers: u32,
    group_verifying_key: &[u8; 32],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::RecoveryGrant,
        FormatVersion::CURRENT,
        RECOVERY_MATRIX_DOMAIN_LABEL,
    )?;
    b.append_u64(FieldId::MATRIX_THRESHOLD, u64::from(threshold))?;
    b.append_u64(FieldId::MATRIX_MAX_SIGNERS, u64::from(max_signers))?;
    b.append_digest32(FieldId::GROUP_VERIFYING_KEY, group_verifying_key)?;
    Ok(b.seal())
}

/// Encode a fork-consent verifying-key id transcript (former `consent_key_id_digest`).
pub fn encode_fork_consent_key_id(
    verifying_key: &[u8; 32],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::ForkGrant,
        FormatVersion::CURRENT,
        FORK_CONSENT_KEY_ID_DOMAIN_LABEL,
    )?;
    b.append_digest32(FieldId::VERIFYING_KEY, verifying_key)?;
    Ok(b.seal())
}

/// Encode an ancestor-entitlement verifying-key id transcript.
pub fn encode_ancestor_entitlement_key_id(
    verifying_key: &[u8; 32],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::AncestorReadGrant,
        FormatVersion::CURRENT,
        ANCESTOR_ENTITLEMENT_KEY_ID_DOMAIN_LABEL,
    )?;
    b.append_digest32(FieldId::VERIFYING_KEY, verifying_key)?;
    Ok(b.seal())
}

/// Encode an AncestorReadGrant payload transcript.
pub fn encode_ancestor_read_grant_payload(
    store_id: &[u8; 32],
    from_epoch: u64,
    from_epoch_store_id: &[u8; 32],
    to_epoch: u64,
    to_epoch_store_id: &[u8; 32],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::AncestorReadGrant,
        FormatVersion::CURRENT,
        ANCESTOR_READ_GRANT_PAYLOAD_DOMAIN_LABEL,
    )?;
    b.append_digest32(FieldId::STORE_ID, store_id)?;
    b.append_u64(FieldId::FROM_EPOCH, from_epoch)?;
    b.append_digest32(FieldId::FROM_EPOCH_STORE_ID, from_epoch_store_id)?;
    b.append_u64(FieldId::TO_EPOCH, to_epoch)?;
    b.append_digest32(FieldId::TO_EPOCH_STORE_ID, to_epoch_store_id)?;
    Ok(b.seal())
}

/// Encode fork successor StoreId derivation inputs (former `derive_fork_store_id`).
pub fn encode_fork_store_id(
    grant_id: &[u8; 32],
    predecessor_store: &[u8; 32],
    fork_point_root: &[u8; 32],
    successor_principal: &[u8; 32],
    identity_seed: &[u8; 32],
    key_material_commitment: &[u8; 32],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::ForkGrant,
        FormatVersion::CURRENT,
        FORK_STORE_ID_DOMAIN_LABEL,
    )?;
    b.append_digest32(FieldId::GRANT_ID, grant_id)?;
    b.append_digest32(FieldId::PREDECESSOR_STORE, predecessor_store)?;
    b.append_digest32(FieldId::FORK_POINT_ROOT, fork_point_root)?;
    b.append_digest32(FieldId::SUCCESSOR_PRINCIPAL, successor_principal)?;
    b.append_digest32(FieldId::IDENTITY_SEED, identity_seed)?;
    b.append_digest32(FieldId::KEY_MATERIAL_COMMITMENT, key_material_commitment)?;
    Ok(b.seal())
}

/// Encode fork WriteAuthority token derivation inputs (former `derive_fork_write_token`).
pub fn encode_fork_write_token(
    store_id: &[u8; 32],
    grant_id: &[u8; 32],
    identity_seed: &[u8; 32],
    key_material_commitment: &[u8; 32],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::ForkGrant,
        FormatVersion::CURRENT,
        FORK_WRITE_TOKEN_DOMAIN_LABEL,
    )?;
    b.append_digest32(FieldId::STORE_ID, store_id)?;
    b.append_digest32(FieldId::GRANT_ID, grant_id)?;
    b.append_digest32(FieldId::IDENTITY_SEED, identity_seed)?;
    b.append_digest32(FieldId::KEY_MATERIAL_COMMITMENT, key_material_commitment)?;
    Ok(b.seal())
}

/// Encode recovery WriteAuthority token derivation inputs.
pub fn encode_recovery_write_token(
    store_id: &[u8; 32],
    grant_id: &[u8; 32],
    predecessor_epoch: u64,
    predecessor_epoch_store_id: &[u8; 32],
    successor_identity_seed: &[u8; 32],
    key_material_commitment: &[u8; 32],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::RecoveryGrant,
        FormatVersion::CURRENT,
        RECOVERY_WRITE_TOKEN_DOMAIN_LABEL,
    )?;
    // FieldIds must be non-decreasing: identity/key before epoch pair.
    b.append_digest32(FieldId::STORE_ID, store_id)?;
    b.append_digest32(FieldId::GRANT_ID, grant_id)?;
    b.append_digest32(FieldId::IDENTITY_SEED, successor_identity_seed)?;
    b.append_digest32(FieldId::KEY_MATERIAL_COMMITMENT, key_material_commitment)?;
    b.append_u64(FieldId::PREDECESSOR_EPOCH, predecessor_epoch)?;
    b.append_digest32(
        FieldId::PREDECESSOR_EPOCH_STORE_ID,
        predecessor_epoch_store_id,
    )?;
    Ok(b.seal())
}

/// Closed WAL record payload shapes for [`encode_wal_record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalRecordPayloadParts<'a> {
    /// Durable commit event.
    Commit {
        /// Dense commit ordinal.
        commit_ordinal: u64,
        /// Opaque commit body.
        body: &'a [u8],
    },
    /// NonceLease floor advance.
    NonceFloor {
        /// MintDomain discriminant: 1=Commit, 2=Compact, 3=Rotate.
        domain: u8,
        /// Exclusive ceiling.
        ceiling: u64,
    },
    /// Incarnation history seal.
    IncarnationSealed {
        /// Open ordinal.
        open_ordinal: u64,
        /// Entropy half of IncarnationId.
        entropy: &'a [u8; 32],
    },
}

/// Encode a WAL record hash transcript (former `hash_record` / kyzo.wal.record.v1).
pub fn encode_wal_record(
    predecessor_hash: &[u8; 32],
    payload: WalRecordPayloadParts<'_>,
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::WalHeader,
        FormatVersion::CURRENT,
        WAL_RECORD_DOMAIN_LABEL,
    )?;
    b.append_digest32(FieldId::PREDECESSOR_HASH, predecessor_hash)?;
    match payload {
        WalRecordPayloadParts::Commit {
            commit_ordinal,
            body,
        } => {
            b.append_bytes(FieldId::PAYLOAD_KIND, b"commit")?;
            b.append_u64(FieldId::COMMIT_ORDINAL, commit_ordinal)?;
            b.append_bytes(FieldId::PAYLOAD_BODY, body)?;
        }
        WalRecordPayloadParts::NonceFloor { domain, ceiling } => {
            b.append_bytes(FieldId::PAYLOAD_KIND, b"nonce_floor")?;
            b.append_u64(FieldId::MINT_DOMAIN, u64::from(domain))?;
            b.append_u64(FieldId::DOMAIN_COUNTER, ceiling)?;
        }
        WalRecordPayloadParts::IncarnationSealed {
            open_ordinal,
            entropy,
        } => {
            b.append_bytes(FieldId::PAYLOAD_KIND, b"incarnation")?;
            b.append_u64(FieldId::WAL_INCARNATION_ORDINAL, open_ordinal)?;
            b.append_digest32(FieldId::WAL_INCARNATION_ENTROPY, entropy)?;
        }
    }
    Ok(b.seal())
}

/// Encode a StateRootHead compact transcript (former `StateRootHead::compact_digest`).
pub fn encode_state_root_head(
    store_id: &[u8; 32],
    fence_epoch: u64,
    commit_ordinal: u64,
    root: &[u8; 32],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::StateRootHead,
        FormatVersion::CURRENT,
        STATE_ROOT_HEAD_DOMAIN_LABEL,
    )?;
    b.append_digest32(FieldId::STORE_ID, store_id)?;
    b.append_u64(FieldId::CUT_FENCE_EPOCH, fence_epoch)?;
    b.append_u64(FieldId::CUT_ORDINAL, commit_ordinal)?;
    b.append_digest32(FieldId::STATE_ROOT, root)?;
    Ok(b.seal())
}

/// Encode a chained state-root bind transcript (former `chain_bind`).
///
/// `link_kind`: 1=Ordinary, 2=Recovery, 3=Fork.
pub fn encode_chained_state_root(
    content_root: &[u8; 32],
    predecessor_root: &[u8; 32],
    link_kind: u8,
    commit_ordinal: u64,
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::ChainedStateRoot,
        FormatVersion::CURRENT,
        CHAINED_STATE_ROOT_DOMAIN_LABEL,
    )?;
    b.append_digest32(FieldId::CONTENT_ROOT, content_root)?;
    b.append_digest32(FieldId::PREDECESSOR_ROOT, predecessor_root)?;
    b.append_u64(FieldId::CHAIN_LINK_KIND, u64::from(link_kind))?;
    b.append_u64(FieldId::CHAIN_COMMIT_ORDINAL, commit_ordinal)?;
    Ok(b.seal())
}

/// One wrapped shred salt's sealed fields for [`encode_leave_is_free_pack`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveIsFreeSaltTranscriptPart {
    /// CryptoDomain.store_id.
    pub store_id: [u8; 32],
    /// CryptoDomain.fence_epoch counter.
    pub fence_epoch: u64,
    /// SegmentCounter.
    pub segment: u64,
    /// Wrapped salt ciphertext bytes.
    pub ciphertext: Vec<u8>,
}

/// One incarnation-history entry for [`encode_leave_is_free_pack`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaveIsFreeIncarnationTranscriptPart {
    /// Open ordinal.
    pub open_ordinal: u64,
    /// Entropy half.
    pub entropy: [u8; 32],
}

/// Encode a leave-is-free pack content-root transcript (former `pack_content_root`).
///
/// `pack_kind` is the former tag bytes: `seal_and_suffix` or `full_wal`.
pub fn encode_leave_is_free_pack(
    pack_kind: &[u8],
    format_version: FormatVersion,
    salts: &[LeaveIsFreeSaltTranscriptPart],
    incarnations: &[LeaveIsFreeIncarnationTranscriptPart],
    payload: &[u8],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::LeaveIsFreePack,
        format_version,
        LEAVE_IS_FREE_PACK_DOMAIN_LABEL,
    )?;
    b.append_bytes(FieldId::PACK_KIND, pack_kind)?;
    b.append_u64(FieldId::WRAPPED_SALT_COUNT, salts.len() as u64)?;
    for salt in salts {
        b.append_digest32(FieldId::SALT_STORE_ID, &salt.store_id)?;
        b.append_u64(FieldId::SALT_FENCE_EPOCH, salt.fence_epoch)?;
        b.append_u64(FieldId::SEGMENT_COUNTER, salt.segment)?;
        b.append_bytes(FieldId::CIPHERTEXT, &salt.ciphertext)?;
    }
    b.append_u64(FieldId::INCARNATION_COUNT, incarnations.len() as u64)?;
    for incarnation in incarnations {
        b.append_u64(FieldId::PACK_INCARNATION_ORDINAL, incarnation.open_ordinal)?;
        b.append_digest32(FieldId::PACK_INCARNATION_ENTROPY, &incarnation.entropy)?;
    }
    b.append_bytes(FieldId::PACK_PAYLOAD, payload)?;
    Ok(b.seal())
}

/// Encode an AuditKeyLeaf subject transcript.
pub fn encode_audit_key_leaf(
    subject_primary: &[u8; 32],
    subject_secondary: &[u8; 32],
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    // Field ids 3/4 precede DOMAIN_LABEL (5) — same header shape as KeyCommit.
    let mut b = CanonicalTranscriptBuilder::new(FormatVersion::CURRENT)?;
    b.append_u64(
        FieldId::ARTIFACT_KIND,
        SealedArtifactKind::AuditKeyLeaf.tag(),
    )?;
    b.append_bytes(FieldId::FORMAT_VERSION, &FormatVersion::CURRENT.as_bytes())?;
    b.append_digest32(FieldId::PRIMARY_DIGEST, subject_primary)?;
    b.append_digest32(FieldId::SECONDARY_DIGEST, subject_secondary)?;
    b.append_bytes(FieldId::DOMAIN_LABEL, AUDIT_KEY_LEAF_DOMAIN_LABEL)?;
    Ok(b.seal())
}

/// Real AdmissionCertificate field schema for [`encode_admission_certificate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionCertificateTranscriptParts {
    /// Protocol / format version tag (8 bytes).
    pub protocol_version: [u8; 8],
    /// Origin StoreId.
    pub origin_store: [u8; 32],
    /// Origin fence epoch counter.
    pub origin_epoch: u64,
    /// Origin fence epoch StoreId bind.
    pub origin_epoch_store_id: [u8; 32],
    /// Origin commit ordinal.
    pub origin_commit: u64,
    /// Schema cut digest.
    pub schema_cut: [u8; 32],
    /// Record digest.
    pub record_digest: [u8; 32],
    /// Predecessor history digest.
    pub predecessor_history_digest: [u8; 32],
    /// Post-state root.
    pub post_state_root: [u8; 32],
    /// Authorizing key id.
    pub authorizing_key_id: [u8; 32],
    /// Scope manifest digest.
    pub scope_manifest_digest: [u8; 32],
    /// Optional OperationKey.
    pub operation_key: Option<[u8; 32]>,
    /// Signature over the signing body.
    pub signature: super::crypto::Signature,
}

/// Encode an AdmissionCertificate envelope under the one constructor.
pub fn encode_admission_certificate(
    parts: &AdmissionCertificateTranscriptParts,
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let mut b = begin_sealed_artifact(
        SealedArtifactKind::AdmissionCertificate,
        FormatVersion::CURRENT,
        ADMISSION_CERTIFICATE_DOMAIN_LABEL,
    )?;
    b.append_digest32(FieldId::STORE_ID, &parts.origin_store)?;
    b.append_bytes(FieldId::PROTOCOL_VERSION, &parts.protocol_version)?;
    b.append_u64(FieldId::ORIGIN_EPOCH, parts.origin_epoch)?;
    b.append_u64(FieldId::ORIGIN_COMMIT, parts.origin_commit)?;
    b.append_digest32(FieldId::SCHEMA_CUT, &parts.schema_cut)?;
    b.append_digest32(FieldId::RECORD_DIGEST, &parts.record_digest)?;
    b.append_digest32(
        FieldId::PREDECESSOR_HISTORY_DIGEST,
        &parts.predecessor_history_digest,
    )?;
    b.append_digest32(FieldId::POST_STATE_ROOT, &parts.post_state_root)?;
    b.append_digest32(FieldId::AUTHORIZING_KEY_ID, &parts.authorizing_key_id)?;
    b.append_digest32(FieldId::SCOPE_MANIFEST_DIGEST, &parts.scope_manifest_digest)?;
    b.append_digest32(FieldId::ORIGIN_EPOCH_STORE_ID, &parts.origin_epoch_store_id)?;
    match parts.operation_key {
        Some(op) => b.append_digest32(FieldId::OPERATION_KEY, &op)?,
        None => b.append_optional_absent(FieldId::OPERATION_KEY)?,
    }
    b.append_bytes(FieldId::SIGNATURE, parts.signature.as_bytes())?;
    Ok(b.seal())
}

/// Normative StoreId for golden-vector / residual-scrub campaigns
/// (hand-derived wire law — same pin as `store/golden/*.vec`).
pub fn normative_golden_store() -> StoreId {
    StoreId::from_digest([0x11; 32])
}

/// Normative digest bytes shared by golden fixtures (hand-derived wire law).
pub fn normative_golden_digest() -> [u8; 32] {
    [0x22u8; 32]
}

/// Normative CheckpointSeal parts for golden / scrub campaigns.
pub fn normative_checkpoint_seal_parts() -> CheckpointSealTranscriptParts {
    let store = normative_golden_store();
    let domain = CryptoDomain::new(store, FenceEpoch::genesis(store));
    let dig = normative_golden_digest();
    CheckpointSealTranscriptParts {
        format_version: FormatVersion::CURRENT,
        store_id: *store.as_bytes(),
        crypto_domain: domain,
        fence_epoch: domain.fence_epoch().get(),
        fence_epoch_store_id: *domain.fence_epoch().store_id().as_bytes(),
        cut: 1,
        state_root: dig,
        final_wal_hash: dig,
        checkpoint_manifest: dig,
        catalog_generation: 1,
        retained_object_manifest: dig,
        permanence_candidate_manifest: dig,
        replica_custody_manifest: dig,
        nonce_floor_commit: 0,
        nonce_floor_compact: 0,
        nonce_floor_rotate: 0,
        incarnation_open_ordinal: 0,
        incarnation_entropy: dig,
        prior_seal_digest: dig,
        retention_certificate_digest: dig,
    }
}

/// Normative AdmissionCertificate parts for golden / scrub campaigns.
pub fn normative_admission_parts() -> AdmissionCertificateTranscriptParts {
    let store = normative_golden_store();
    let dig = normative_golden_digest();
    AdmissionCertificateTranscriptParts {
        protocol_version: [0; 8],
        origin_store: *store.as_bytes(),
        origin_epoch: 0,
        origin_epoch_store_id: *store.as_bytes(),
        origin_commit: 1,
        schema_cut: dig,
        record_digest: dig,
        predecessor_history_digest: dig,
        post_state_root: dig,
        authorizing_key_id: dig,
        scope_manifest_digest: dig,
        operation_key: None,
        signature: super::crypto::Signature::from_bytes([0u8; 64]),
    }
}

/// CMT-1 KeyCommit normative key-id (matches [`KEY_COMMIT_GOLDEN_VEC`]).
pub fn normative_key_commit_key_id() -> [u8; 32] {
    [
        0x08u8, 0x01, 0x8b, 0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
        0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6,
        0xa7, 0xa8,
    ]
}

/// CMT-1 KeyCommit normative CryptoDomain (matches [`KEY_COMMIT_GOLDEN_VEC`]).
pub fn normative_key_commit_domain() -> CryptoDomain {
    let key_store = StoreId::from_digest([
        0x08, 0x02, 0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98,
        0x99, 0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
        0xa8, 0xa9,
    ]);
    CryptoDomain::new(key_store, FenceEpoch::genesis(key_store))
}

/// Encode one sealed kind via production `encode_*` with normative pins.
///
/// Residual-secret campaigns and the production half of the independent-golden
/// pin test share this path. Each arm calls the typed production encoder with
/// hand-derived parts; expected bytes come from the independent wire encoder
/// in `pins`, never from capturing this function's output into `.vec`.
pub fn encode_normative_production_transcript(
    kind: SealedArtifactKind,
) -> Result<CanonicalTranscript, TranscriptRefuse> {
    let store = normative_golden_store();
    let dig = normative_golden_digest();
    match kind {
        SealedArtifactKind::CheckpointSeal => {
            encode_checkpoint_seal(&normative_checkpoint_seal_parts())
        }
        SealedArtifactKind::AdmissionCertificate => {
            encode_admission_certificate(&normative_admission_parts())
        }
        SealedArtifactKind::ForkGrant => {
            encode_fork_grant_payload(&dig, &dig, &dig, &dig, &dig, &dig)
        }
        SealedArtifactKind::RecoveryGrant => {
            encode_recovery_grant_payload(&dig, &dig, 0, &dig, &dig, &dig)
        }
        SealedArtifactKind::MergeProofHeader => {
            encode_merge_proof_header(&[dig], &dig, &dig, 1, &dig)
        }
        SealedArtifactKind::AuditKeyLeaf => encode_audit_key_leaf(&dig, &dig),
        SealedArtifactKind::WalHeader => encode_wal_record(
            &dig,
            WalRecordPayloadParts::Commit {
                commit_ordinal: 1,
                body: b"body",
            },
        ),
        SealedArtifactKind::KeyCommit => encode_key_commitment(
            &normative_key_commit_key_id(),
            normative_key_commit_domain(),
        ),
        SealedArtifactKind::StateRootHead => encode_state_root_head(store.as_bytes(), 0, 1, &dig),
        SealedArtifactKind::LeaveIsFreePack => encode_leave_is_free_pack(
            b"seal_and_suffix",
            FormatVersion::CURRENT,
            &[LeaveIsFreeSaltTranscriptPart {
                store_id: *store.as_bytes(),
                fence_epoch: 0,
                segment: 0,
                ciphertext: vec![1, 2, 3],
            }],
            &[LeaveIsFreeIncarnationTranscriptPart {
                open_ordinal: 0,
                entropy: dig,
            }],
            b"payload",
        ),
        SealedArtifactKind::ChainedStateRoot => encode_chained_state_root(&dig, &dig, 1, 1),
        SealedArtifactKind::AncestorReadGrant => {
            encode_ancestor_read_grant_payload(&dig, 0, &dig, 1, &dig)
        }
        SealedArtifactKind::WrappedShredSalt => {
            let domain = CryptoDomain::new(store, FenceEpoch::genesis(store));
            encode_wrapped_shred_salt_aad(domain, 0)
        }
    }
}

/// One production transcript per [`SEALED_ARTIFACT_KINDS`] entry (scrub campaigns).
pub fn encode_all_normative_production_transcripts()
-> Result<Vec<CanonicalTranscript>, TranscriptRefuse> {
    SEALED_ARTIFACT_KINDS
        .iter()
        .copied()
        .map(encode_normative_production_transcript)
        .collect()
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
    if !hex.len().is_multiple_of(2) {
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
        unknown => {
            drop(unknown);
            Err(TranscriptRefuse::Corrupt)
        }
    }
}

#[cfg(test)]
mod pins {
    use miette::{IntoDiagnostic, Result, miette};
    use super::*;

    /// Wire field tags from the CanonicalTranscript format spec (not production API).
    const TAG_U64: u8 = 1;
    const TAG_BYTES: u8 = 2;
    const TAG_DIGEST32: u8 = 3;
    const TAG_OPTIONAL_ABSENT: u8 = 5;

    /// Deliberately dumb independent encoder written from the wire format only:
    /// MAGIC ‖ ver_len ‖ ver ‖ field_count_be_u32 ‖ (field_id_be_u16 ‖ tag ‖ payload)*.
    /// Does **not** call [`CanonicalTranscriptBuilder`] or any production `encode_*`.
    struct IndepWire {
        buf: Vec<u8>,
        field_count_at: usize,
        field_count: u32,
    }

    impl IndepWire {
        /// FormatVersion 6 → ASCII `"6"` (canonical spelling).
        fn new_v6() -> Self {
            let mut buf = Vec::with_capacity(64);
            buf.extend_from_slice(b"KTX1"); // MAGIC
            buf.push(1); // ver_len
            buf.extend_from_slice(b"6"); // ver
            let field_count_at = buf.len();
            buf.extend_from_slice(&0u32.to_be_bytes());
            Self {
                buf,
                field_count_at,
                field_count: 0,
            }
        }

        fn begin(&mut self, id: u16) {
            self.buf.extend_from_slice(&id.to_be_bytes());
            self.field_count += 1;
        }

        fn u64(&mut self, id: u16, value: u64) {
            self.begin(id);
            self.buf.push(TAG_U64);
            self.buf.extend_from_slice(&value.to_be_bytes());
        }

        fn bytes(&mut self, id: u16, bytes: &[u8]) {
            self.begin(id);
            self.buf.push(TAG_BYTES);
            self.buf
                .extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            self.buf.extend_from_slice(bytes);
        }

        fn digest32(&mut self, id: u16, digest: &[u8; 32]) {
            self.begin(id);
            self.buf.push(TAG_DIGEST32);
            self.buf.extend_from_slice(digest);
        }

        fn optional_absent(&mut self, id: u16) {
            self.begin(id);
            self.buf.push(TAG_OPTIONAL_ABSENT);
        }

        fn finish(mut self) -> Vec<u8> {
            let count = self.field_count.to_be_bytes();
            self.buf[self.field_count_at..self.field_count_at + 4].copy_from_slice(&count);
            self.buf
        }
    }

    /// Shared sealed-artifact header fields from the wire schema
    /// (ARTIFACT_KIND / FORMAT_VERSION / DOMAIN_LABEL).
    fn indep_begin_sealed(kind_tag: u64, domain_label: &[u8]) -> IndepWire {
        let mut w = IndepWire::new_v6();
        w.u64(1, kind_tag); // FieldId::ARTIFACT_KIND
        w.bytes(2, b"6"); // FieldId::FORMAT_VERSION
        w.bytes(5, domain_label); // FieldId::DOMAIN_LABEL
        w
    }

    /// Independently derive expected sealed bytes for the normative fixture of `kind`.
    /// Field order and tags come from the CanonicalTranscript wire schema / FieldId table —
    /// never from calling production `encode_*` and capturing output.
    fn independent_encode_normative(kind: SealedArtifactKind) -> Vec<u8> {
        let store = [0x11u8; 32];
        let dig = [0x22u8; 32];
        match kind {
            SealedArtifactKind::CheckpointSeal => {
                let mut w = indep_begin_sealed(1, b"kyzo.checkpoint_seal.v1");
                w.digest32(9, &store); // STORE_ID
                w.digest32(10, &store); // CRYPTO_DOMAIN_STORE_ID
                w.u64(11, 0); // CRYPTO_DOMAIN_FENCE_EPOCH
                w.digest32(12, &store); // CRYPTO_DOMAIN_FENCE_STORE_ID
                w.u64(13, 0); // CUT_FENCE_EPOCH
                w.digest32(14, &store); // CUT_FENCE_STORE_ID
                w.u64(15, 1); // CUT_ORDINAL
                w.digest32(16, &dig); // STATE_ROOT
                w.digest32(17, &dig); // FINAL_WAL_HASH
                w.digest32(18, &dig); // CHECKPOINT_MANIFEST
                w.u64(19, 1); // CATALOG_GENERATION
                w.digest32(20, &dig); // RETAINED_OBJECT_MANIFEST
                w.digest32(21, &dig); // PERMANENCE_CANDIDATE_MANIFEST
                w.digest32(22, &dig); // REPLICA_CUSTODY_MANIFEST
                w.u64(23, 0); // NONCE_FLOOR_COMMIT
                w.u64(24, 0); // NONCE_FLOOR_COMPACT
                w.u64(25, 0); // NONCE_FLOOR_ROTATE
                w.u64(26, 0); // INCARNATION_OPEN_ORDINAL
                w.digest32(27, &dig); // INCARNATION_ENTROPY
                w.digest32(28, &dig); // PRIOR_SEAL_DIGEST
                w.digest32(29, &dig); // RETENTION_CERTIFICATE_DIGEST
                w.finish()
            }
            SealedArtifactKind::AdmissionCertificate => {
                let mut w = indep_begin_sealed(2, b"kyzo.admission_certificate.v1");
                w.digest32(9, &store); // STORE_ID
                w.bytes(68, &[0u8; 8]); // PROTOCOL_VERSION
                w.u64(69, 0); // ORIGIN_EPOCH
                w.u64(70, 1); // ORIGIN_COMMIT
                w.digest32(71, &dig); // SCHEMA_CUT
                w.digest32(72, &dig); // RECORD_DIGEST
                w.digest32(73, &dig); // PREDECESSOR_HISTORY_DIGEST
                w.digest32(74, &dig); // POST_STATE_ROOT
                w.digest32(75, &dig); // AUTHORIZING_KEY_ID
                w.digest32(76, &dig); // SCOPE_MANIFEST_DIGEST
                w.digest32(77, &store); // ORIGIN_EPOCH_STORE_ID
                w.optional_absent(78); // OPERATION_KEY absent
                w.bytes(79, &[0u8; 64]); // SIGNATURE (wire width; typed as Signature above)
                w.finish()
            }
            SealedArtifactKind::ForkGrant => {
                let mut w = indep_begin_sealed(3, b"kyzo.fork_grant.payload.v1");
                w.digest32(34, &dig); // GRANT_ID
                w.digest32(35, &dig); // PREDECESSOR_STORE
                w.digest32(36, &dig); // FORK_POINT_ROOT
                w.digest32(37, &dig); // SUCCESSOR_PRINCIPAL
                w.digest32(38, &dig); // IDENTITY_SEED
                w.digest32(39, &dig); // KEY_MATERIAL_COMMITMENT
                w.finish()
            }
            SealedArtifactKind::RecoveryGrant => {
                let mut w = indep_begin_sealed(4, b"kyzo.recovery_grant.payload.v1");
                w.digest32(34, &dig); // GRANT_ID
                w.digest32(35, &dig); // PREDECESSOR_STORE
                w.digest32(38, &dig); // IDENTITY_SEED
                w.digest32(39, &dig); // KEY_MATERIAL_COMMITMENT
                w.u64(40, 0); // PREDECESSOR_EPOCH
                w.digest32(41, &dig); // PREDECESSOR_EPOCH_STORE_ID
                w.finish()
            }
            SealedArtifactKind::MergeProofHeader => {
                let mut w = indep_begin_sealed(5, b"kyzo.merge_proof.v1");
                w.digest32(30, &dig); // INPUT_CONTENT_HASH
                w.digest32(31, &dig); // LINEAGE_HASH
                w.digest32(84, &dig); // MERGE_STATE_ROOT
                w.u64(85, 1); // MERGE_COMPACT_COUNTER
                w.digest32(86, &dig); // MERGE_OUTPUT_CONTENT_HASH
                w.finish()
            }
            SealedArtifactKind::AuditKeyLeaf => {
                // Digests precede DOMAIN_LABEL (field ids 3/4 before 5).
                let mut w = IndepWire::new_v6();
                w.u64(1, 6); // ARTIFACT_KIND = AuditKeyLeaf
                w.bytes(2, b"6"); // FORMAT_VERSION
                w.digest32(3, &dig); // PRIMARY_DIGEST
                w.digest32(4, &dig); // SECONDARY_DIGEST
                w.bytes(5, b"kyzo.audit_key_leaf.v1"); // DOMAIN_LABEL
                w.finish()
            }
            SealedArtifactKind::WalHeader => {
                let mut w = indep_begin_sealed(7, b"kyzo.wal.record.v1");
                w.digest32(50, &dig); // PREDECESSOR_HASH
                w.bytes(51, b"commit"); // PAYLOAD_KIND
                w.u64(52, 1); // COMMIT_ORDINAL
                w.bytes(53, b"body"); // PAYLOAD_BODY
                w.finish()
            }
            SealedArtifactKind::KeyCommit => {
                let key_id = [
                    0x08u8, 0x01, 0x8b, 0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95,
                    0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f, 0xa0, 0xa1, 0xa2,
                    0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8,
                ];
                let key_store = [
                    0x08u8, 0x02, 0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96,
                    0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f, 0xa0, 0xa1, 0xa2, 0xa3,
                    0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9,
                ];
                let mut w = IndepWire::new_v6();
                w.u64(1, 8); // ARTIFACT_KIND = KeyCommit
                w.bytes(2, b"6"); // FORMAT_VERSION
                w.digest32(3, &key_id); // PRIMARY_DIGEST
                w.digest32(4, &key_store); // SECONDARY_DIGEST
                w.bytes(5, b"KEY_COMMIT"); // DOMAIN_LABEL
                w.u64(8, 0); // FENCE_EPOCH
                w.finish()
            }
            SealedArtifactKind::StateRootHead => {
                let mut w = indep_begin_sealed(9, b"kyzo.state_root_head.v1");
                w.digest32(9, &store); // STORE_ID
                w.u64(13, 0); // CUT_FENCE_EPOCH
                w.u64(15, 1); // CUT_ORDINAL
                w.digest32(16, &dig); // STATE_ROOT
                w.finish()
            }
            SealedArtifactKind::LeaveIsFreePack => {
                let mut w = indep_begin_sealed(10, b"kyzo.leave_is_free.pack.root.v1");
                w.bytes(59, b"seal_and_suffix"); // PACK_KIND
                w.u64(60, 1); // WRAPPED_SALT_COUNT
                w.digest32(61, &store); // SALT_STORE_ID
                w.u64(62, 0); // SALT_FENCE_EPOCH
                w.u64(63, 0); // SEGMENT_COUNTER
                w.bytes(64, &[1, 2, 3]); // CIPHERTEXT
                w.u64(65, 1); // INCARNATION_COUNT
                w.u64(66, 0); // PACK_INCARNATION_ORDINAL
                w.digest32(67, &dig); // PACK_INCARNATION_ENTROPY
                w.bytes(80, b"payload"); // PACK_PAYLOAD
                w.finish()
            }
            SealedArtifactKind::ChainedStateRoot => {
                let mut w = indep_begin_sealed(11, b"kyzo.chained_state_root.v1");
                w.digest32(56, &dig); // CONTENT_ROOT
                w.digest32(57, &dig); // PREDECESSOR_ROOT
                w.u64(58, 1); // CHAIN_LINK_KIND = Ordinary
                w.u64(81, 1); // CHAIN_COMMIT_ORDINAL
                w.finish()
            }
            SealedArtifactKind::AncestorReadGrant => {
                let mut w = indep_begin_sealed(12, b"kyzo.ancestor_read_grant.payload.v1");
                w.digest32(9, &dig); // STORE_ID
                w.u64(46, 0); // FROM_EPOCH
                w.digest32(47, &dig); // FROM_EPOCH_STORE_ID
                w.u64(48, 1); // TO_EPOCH
                w.digest32(49, &dig); // TO_EPOCH_STORE_ID
                w.finish()
            }
            SealedArtifactKind::WrappedShredSalt => {
                let mut w = indep_begin_sealed(13, b"WSS1");
                w.digest32(61, &store); // SALT_STORE_ID
                w.u64(62, 0); // SALT_FENCE_EPOCH
                w.u64(63, 0); // SEGMENT_COUNTER
                w.finish()
            }
        }
    }

    fn golden_file_for(kind: SealedArtifactKind) -> Option<&'static str> {
        Some(match kind {
            SealedArtifactKind::CheckpointSeal => include_str!("golden/checkpoint_seal.vec"),
            SealedArtifactKind::AdmissionCertificate => {
                include_str!("golden/admission_certificate.vec")
            }
            SealedArtifactKind::ForkGrant => include_str!("golden/fork_grant.vec"),
            SealedArtifactKind::RecoveryGrant => include_str!("golden/recovery_grant.vec"),
            SealedArtifactKind::MergeProofHeader => include_str!("golden/merge_proof_header.vec"),
            SealedArtifactKind::AuditKeyLeaf => include_str!("golden/audit_key_leaf.vec"),
            SealedArtifactKind::WalHeader => include_str!("golden/wal_header.vec"),
            SealedArtifactKind::StateRootHead => include_str!("golden/state_root_head.vec"),
            SealedArtifactKind::LeaveIsFreePack => include_str!("golden/leave_is_free_pack.vec"),
            SealedArtifactKind::ChainedStateRoot => include_str!("golden/chained_state_root.vec"),
            SealedArtifactKind::AncestorReadGrant => include_str!("golden/ancestor_read_grant.vec"),
            SealedArtifactKind::KeyCommit => KEY_COMMIT_GOLDEN_VEC,
            SealedArtifactKind::WrappedShredSalt => WRAPPED_SHRED_SALT_AAD_GOLDEN_VEC,
        })
    }

    /// Expected bytes = independent wire derivation; production `encode_*` must match.
    /// `.vec` files hold that same independently derived hex (durability pin) — never
    /// encoder-self-capture.
    #[test]
    fn production_matches_independent_wire_goldens() -> Result<()> {
        for &kind in SEALED_ARTIFACT_KINDS {
            let expected = independent_encode_normative(kind);
            let production = encode_normative_production_transcript(kind)?;
            assert_eq!(
                production.as_bytes(),
                expected.as_slice(),
                "production encode_{kind:?} must match independent wire derivation"
            );
            let golden = golden_file_for(kind)?;
            let from_vec = parse_golden_hex(golden)?;
            assert_eq!(
                from_vec.as_slice(),
                expected.as_slice(),
                "{kind:?}: .vec must hold independently derived bytes"
            );
            let parsed = CanonicalTranscript::parse(&expected)?;
            assert_eq!(parsed.as_bytes(), expected.as_slice());
        }
    
        Ok(())
    }

    /// Seat 59 grep gate: sealed-surface modules may SHA-256 only over
    /// `CanonicalTranscript.as_bytes()` — never `h.update(b"kyzo.…")` domain labels
    /// for sealed-artifact kinds (second serializer).
    #[test]
    fn seat59_no_second_serializer_on_sealed_surface() -> Result<()> {
        // Full module text (not cfg(test)-stripped): backup/grants place `#[cfg(test)]`
        // helpers mid-file before production digest sites — a first-split would miss them.
        let seal = include_str!("seal.rs");
        let compact = include_str!("compact.rs");
        let grants = include_str!("grants.rs");
        let wal = include_str!("wal.rs");
        let backup = include_str!("backup.rs");
        let merkle = include_str!("merkle.rs");

        // Split so this gate body never contains a contiguous forbidden update needle.
        let sealed_domains: [String; 17] = [
            ["kyzo.checkpoint", "_seal.v1"].concat(),
            ["kyzo.merge", "_proof.v1"].concat(),
            ["kyzo.fork_grant", ".payload.v1"].concat(),
            ["kyzo.recovery_grant", ".payload.v1"].concat(),
            ["kyzo.recovery", "_matrix.v1"].concat(),
            ["kyzo.fork_consent", ".key_id.v1"].concat(),
            ["kyzo.ancestor_entitlement", ".key_id.v1"].concat(),
            ["kyzo.ancestor_read_grant", ".payload.v1"].concat(),
            ["kyzo.store_id", ".fork.v1"].concat(),
            ["kyzo.write_authority", ".fork.v1"].concat(),
            ["kyzo.write_authority", ".recovery.v1"].concat(),
            ["kyzo.wal", ".record.v1"].concat(),
            ["kyzo.state_root", "_head.v1"].concat(),
            ["kyzo.chained_state", "_root.v1"].concat(),
            ["kyzo.leave_is_free", ".pack.root.v1"].concat(),
            ["kyzo.audit_key", "_leaf.v1"].concat(),
            ["kyzo.admission", "_certificate.v1"].concat(),
        ];

        for (name, src) in [
            ("seal.rs", seal),
            ("compact.rs", compact),
            ("grants.rs", grants),
            ("wal.rs", wal),
            ("backup.rs", backup),
            ("merkle.rs", merkle),
        ] {
            for domain in &sealed_domains {
                let needle = format!("update(b\"{domain}\")");
                assert!(
                    !src.contains(needle.as_str()),
                    "{name}: forbidden second-serializer domain hash `{needle}`"
                );
            }
        }

        // On pure sealed digest modules, every Sha256::new/default must hash transcript bytes.
        for (name, src) in [
            ("seal.rs", seal),
            ("compact.rs", compact),
            ("grants.rs", grants),
            ("wal.rs", wal),
            ("backup.rs", backup),
        ] {
            for ctor in ["Sha256::new()", "Sha256::default()"] {
                let mut rest = src;
                while let Some(idx) = rest.find(ctor) {
                    let after = &rest[idx + ctor.len()..];
                    let window = &after[..after.len().min(240)];
                    assert!(
                        window.contains("transcript.as_bytes()"),
                        "{name}: {ctor} must hash CanonicalTranscript.as_bytes(); nearby:\n{window}"
                    );
                    rest = &after[1..];
                }
            }
        }

        // merkle sealed paths (compact_digest / chain_bind): same transcript-hash rule at
        // encode_* *call sites* only — the trailing `(` excludes `use` import lines.
        for encode_name in ["encode_state_root_head(", "encode_chained_state_root("] {
            assert!(
                merkle.contains(encode_name),
                "merkle.rs must route sealed roots through {encode_name}"
            );
        }
        // After each of those encode_* call sites, Sha256 must digest transcript.as_bytes().
        for marker in ["encode_state_root_head(", "encode_chained_state_root("] {
            let mut rest = merkle;
            while let Some(idx) = rest.find(marker) {
                let after = &rest[idx..];
                let window = &after[..after.len().min(400)];
                assert!(
                    window.contains("transcript.as_bytes()"),
                    "merkle.rs: {marker} site must hash transcript.as_bytes();\n{window}"
                );
                rest = &after[marker.len()..];
            }
        }
    
        Ok(())
    }

    #[test]
    fn production_encoders_seal_without_field_order_refuse() -> Result<()> {
        let store = normative_golden_store();
        let domain = CryptoDomain::new(store, FenceEpoch::genesis(store));
        let dig = normative_golden_digest();

        assert!(encode_checkpoint_seal(&normative_checkpoint_seal_parts()).is_ok());
        assert!(encode_merge_proof_header(&[dig], &dig, &dig, 1, &dig).is_ok());
        assert!(encode_fork_grant_payload(&dig, &dig, &dig, &dig, &dig, &dig).is_ok());
        assert!(encode_recovery_grant_payload(&dig, &dig, 0, &dig, &dig, &dig).is_ok());
        assert!(encode_recovery_matrix(2, 3, &dig).is_ok());
        assert!(encode_fork_consent_key_id(&dig).is_ok());
        assert!(encode_ancestor_entitlement_key_id(&dig).is_ok());
        assert!(encode_ancestor_read_grant_payload(&dig, 0, &dig, 1, &dig).is_ok());
        assert!(encode_fork_store_id(&dig, &dig, &dig, &dig, &dig, &dig).is_ok());
        assert!(encode_fork_write_token(&dig, &dig, &dig, &dig).is_ok());
        assert!(encode_recovery_write_token(&dig, &dig, 0, &dig, &dig, &dig).is_ok());
        assert!(
            encode_wal_record(
                &dig,
                WalRecordPayloadParts::Commit {
                    commit_ordinal: 1,
                    body: b"body",
                },
            )
            .is_ok()
        );
        assert!(
            encode_wal_record(
                &dig,
                WalRecordPayloadParts::NonceFloor {
                    domain: 1,
                    ceiling: 2,
                },
            )
            .is_ok()
        );
        assert!(
            encode_wal_record(
                &dig,
                WalRecordPayloadParts::IncarnationSealed {
                    open_ordinal: 0,
                    entropy: &dig,
                },
            )
            .is_ok()
        );
        assert!(encode_state_root_head(store.as_bytes(), 0, 1, &dig).is_ok());
        assert!(encode_chained_state_root(&dig, &dig, 1, 1).is_ok());
        assert!(
            encode_leave_is_free_pack(
                b"seal_and_suffix",
                FormatVersion::CURRENT,
                &[LeaveIsFreeSaltTranscriptPart {
                    store_id: *store.as_bytes(),
                    fence_epoch: 0,
                    segment: 0,
                    ciphertext: vec![1, 2, 3],
                }],
                &[LeaveIsFreeIncarnationTranscriptPart {
                    open_ordinal: 0,
                    entropy: dig,
                }],
                b"payload",
            )
            .is_ok()
        );
        assert!(encode_audit_key_leaf(&dig, &dig).is_ok());
        assert!(encode_admission_certificate(&normative_admission_parts()).is_ok());
        assert!(encode_key_commitment(&dig, domain).is_ok());
        assert!(encode_wrapped_shred_salt_aad(domain, 0).is_ok());
        for kind in SEALED_ARTIFACT_KINDS {
            assert!(
                encode_normative_production_transcript(*kind).is_ok(),
                "normative production encode must seal for {kind:?}"
            );
        }
    
        Ok(())
    }

    #[test]
    fn unknown_version_refuses() -> Result<()> {
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
    
        Ok(())
    }

    #[test]
    fn duplicate_map_key_refuses() -> Result<()> {
        let mut b = CanonicalTranscriptBuilder::new(FormatVersion::CURRENT)?;
        let entries = vec![
            (b"a".to_vec(), MapValue::U64(1)),
            (b"a".to_vec(), MapValue::U64(2)),
        ];
        assert_eq!(
            b.append_map(FieldId::BINDINGS_MAP, &entries),
            Err(TranscriptRefuse::DuplicateMapKey)
        );
    
        Ok(())
    }

    #[test]
    fn length_bound_checked_before_alloc() -> Result<()> {
        let mut b = CanonicalTranscriptBuilder::new(FormatVersion::CURRENT)?;
        let huge = vec![0u8; (MAX_BYTES_FIELD as usize) + 1];
        assert_eq!(
            b.append_bytes(FieldId::DOMAIN_LABEL, &huge),
            Err(TranscriptRefuse::LengthBoundExceeded)
        );
    
        Ok(())
    }
}
