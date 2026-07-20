/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Ordered substrate: contract, transactions, fjall adapter, backup, walks,
//! and the 07 storage-Spec seats (identity, authority, commit door, WAL, …).
//!
//! Seat 8 ([`forge_wall`]): Store never mints [`crate::session::admit::KyzoRecord`]
//! from bytes — SST/WAL/object/dump doors are currency only.
//!
//! No module-level `allow(dead_code)`: seats with live host/session callers
//! stay used; unwired items warn honestly. Do not silence with a LOUD lie.

/// WriteAuthority + incarnation + RecoveryMatrix + address fence (07 seat).
pub(crate) mod authority;
/// Leave-is-free pack / import ceremony (07 seat).
pub(crate) mod backup;
/// StableCommitCap closed sum + SnapshotFork + ForkGenerationWitness (07 seat).
pub(crate) mod commit_cap;
/// Deterministic compaction: pace, range class, MergeProof (07 seat).
pub(crate) mod compact;
pub(crate) mod contract;
/// DEK/KEK/ShredSalt/WrappedShredSalt/AuditKey/AEAD pipeline (07 seat).
pub(crate) mod crypto;
/// FenceEpoch + CryptoDomain + EpochGrant advance (07 seat).
pub(crate) mod epoch;
/// Failure lattice + closed StoreRefuse ledger + debt economy (07 seat).
pub(crate) mod failure;
pub(crate) mod fjall;
/// Seat 8 — Store cannot mint KyzoRecord from bytes (forge wall).
pub(crate) mod forge_wall;
/// ForkGrant / RecoveryGrant + pure materialize (07 seat).
pub(crate) mod grants;
/// OperationKey + OperationOutcome + request_digest memo (07 seat).
pub(crate) mod idempotency;
pub(crate) mod keys;
pub(crate) mod merkle;
/// NonceLease + MintDomain + DomainCounter + pure nonce fn (07 seat).
pub(crate) mod nonce;
/// Object seam: staging, permanence, durability classes, GC (07 seat).
pub(crate) mod objects;
/// Identity + open capability + genesis (07 seat).
pub(crate) mod open;
/// AdmissionCertificate + verify_replica + crossing-contract validation (07 seat).
pub(crate) mod replica;
pub(crate) mod retry;
pub(crate) mod scratch;
/// CheckpointSeal + truncate consume + seal verification (07 seat).
pub(crate) mod seal;
pub(crate) mod skip_walk;
/// SweepDoor + IntentOrdinal/CommitOrdinal + Applied/Committed mint (07 seat).
pub(crate) mod sweep;
pub(crate) mod time;
/// CanonicalTranscript + golden vectors + unknown-version refuse (07 seat).
pub(crate) mod transcript;
pub(crate) mod tx;
pub(crate) mod verify_walk;
/// WAL segment format + cross-segment hash chain + replay (07 seat).
pub(crate) mod wal;

// Sim instrument lives at kyzo-crashfs (seat); path-included here so the
// sealed Storage trait can admit it as the contract's own test double.
// Path is relative to this file (`store/mod.rs`).
#[cfg(any(test, feature = "bench-internals"))]
#[cfg_attr(all(feature = "bench-internals", not(test)), allow(dead_code))]
#[path = "../../../kyzo-crashfs/src/sim.rs"]
pub(crate) mod sim;

// Seat reexports: crate-visible names for hosts and mid-wiring.
// `unused_imports` here is reexport noise only — not a dead_code silence.
#[allow(unused_imports)]
pub use authority::{
    AddressFence, AddressFenceRefuse, AddressFenceTable, Entropy, IncarnationId,
    IncarnationMintCap, OpenOrdinal, RecoveryMatrix, RecoveryPublicKey, WriteAuthority,
    WriteTokenId,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use backup::{
    ImportCapability, LeaveIsFreeKind, LeaveIsFreePack, LeaveIsFreeParts, ObjectsCompleteness,
    PackRefuse, dump_storage, import_verify, restore_storage,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use commit_cap::{ForkGenerationWitness, SnapshotFork, StableCommitCap};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use compact::{
    CompactRefuse, CompactStateRoot, CompactionDebt, CompactionPace, KvSeparationThreshold,
    LineageHash, MergeProof, MergeProofParts, MergedPacket, PacketContentHash, RangeClass,
    classify_range_at_commit, pace,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use contract::{FormatVersion, Storage};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub(crate) use contract::{SystemClock, SystemClockRefuse};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use crypto::{
    AeadArm, AuditKey, Ciphertext, CompressedBytes, CryptoRefuse, Dek, Kek, KekUnwrapCap,
    SegmentCounter, ShredLedger, ShredReceipt, ShredSalt, ShredTombstone, WrappedShredSalt,
    compress, compress_then_encrypt, decompress, decrypt, derive_dek, encrypt, shred,
    unwrap_shred_salt, wrap_shred_salt,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use epoch::{
    CryptoDomain, EpochAdvanceCommitted, EpochGrant, FenceEpoch, IntentClear, advance,
    advance_recovery,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use failure::{
    DebtLedger, FailureLattice, KeyspaceId, OperatorHealthSurface, QuarantineRange, StoreRefuse,
};
#[allow(unused_imports)] // seat 8 forge wall witness
pub use forge_wall::StoreCurrencyOnly;
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use grants::{
    AncestorReadGrant, ForkGrant, ForkPointRoot, Grant, GrantId, IdentitySeed,
    KeyMaterialCommitment, MaterializedGrant, PriorMaterialization, RecoveryGrant,
    SuccessorPrincipal, materialize,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use idempotency::{
    IdempotencyEntry, IdempotencyMemo, OperationKey, OperationOutcome, RequestDigest,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use keys::{Secret, SecretKeyRefuse, refuse_if_secret};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use merkle::{
    ChainLinkKind, ChainedStateRoot, ForkPoint, GENESIS_ROOT, MerkleChainRefuse, RootChain,
    StateRoot, as_of_root, fork_equivalence, roots_equal_at_cut,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use nonce::{DomainCounter, MintDomain, NonceLease, nonce};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use objects::{
    BackendContract, ConfirmedCopies, ConsistencyClass, ContentHash, Downgrade, FailureDomains,
    IntegrityVerification, ObjectDurabilityClass, ObjectId, ObjectRef, ObjectRefuse, ObjectSlot,
    ObjectStore, PermanenceCandidate, PermanenceWitness, ReclaimCertificate, Regions, Repair,
    ResolvedObject, RetentionCertificate, StagingToken, VolatilePending, gc_durable,
    reclaim_candidate, resolve,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use open::{
    EntropyArm, GenesisParams, GenesisRefuse, GenesisSealed, GenesisSealedView, SizeClass,
    StableCommitCapArm, StagingTtl, StoreId, StoreOpen, StoreOpenVerb, genesis, open_path_only,
    open_with_capability,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use replica::{
    AdmissionCertificate, AdmissionCertificateParts, AuthorizingKey, AuthorizingKeyId,
    AuthorizingKeyTable, CrossingCapabilitySet, CrossingContext, CrossingEnvelope,
    CrossingEvidence, CrossingEvidenceDemand, CrossingKind, CrossingRefuse, CrossingStatus,
    CrossingValidated, LocalProjection, OriginContinuity, PostStateRoot, ReplicaCustody,
    ReplicaKey, ReplicaRefuse, ScopeManifestDigest, ScopeManifestStatus, ScopeManifestTable,
    anchor_pending, refuse_in_place_local_reinterpretation, validate_crossing_before_lower,
    verify_replica,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use seal::{
    CheckpointSeal, CheckpointSealParts, GENESIS_PRIOR_SEAL, NonceLeaseFloors, SealDigest,
    SealRefuse, TruncateLedger, TruncationReceipt, truncate,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use sweep::{
    AdmittedIntent, CommitOrdinal, IntentOrdinal, IntentionQueue, SweepDoor, SweepRefuse,
    SweepSealFailure, SweepSession,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use transcript::{
    CanonicalTranscript, CanonicalTranscriptBuilder, FieldId, MapValue, SealedArtifactKind,
    TranscriptRefuse, encode_golden_fixture, parse_golden_hex,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use tx::{
    Aborted, Applied, BackendIoError, CommitCorruption, CommitFailure, CommitIo, Committed,
    ConflictError, ReadTx, Slice, WriteTx,
};
#[allow(unused_imports)] // seat reexport; production may not bind yet
pub use wal::{WalFloors, WalHash, WalPayload, WalRecord, WalReplayState, WalSegment, replay};
