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

/// WriteAuthority + incarnation + RecoveryMatrix + address fence (07 seat).
pub(crate) mod authority;
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
/// ForkGrant / RecoveryGrant + pure materialize (07 seat).
pub(crate) mod grants;
/// OperationKey + OperationOutcome + request_digest memo (07 seat).
pub(crate) mod idempotency;
pub(crate) mod keys;
#[allow(dead_code)]
pub(crate) mod merkle;
/// NonceLease + MintDomain + DomainCounter + pure nonce fn (07 seat).
pub(crate) mod nonce;
/// Object seam: staging, permanence, durability classes, GC (07 seat).
pub(crate) mod objects;
/// Identity + open capability + genesis (07 seat).
pub(crate) mod open;
/// AdmissionCertificate + verify_replica + ReplicaCustody (07 seat).
pub(crate) mod replica;
pub(crate) mod retry;
#[allow(dead_code)]
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

pub use authority::{
    AddressFence, AddressFenceRefuse, AddressFenceTable, Entropy, IncarnationId,
    IncarnationMintCap, OpenOrdinal, RecoveryMatrix, RecoveryPublicKey, WriteAuthority,
};
pub use backup::{
    ImportCapability, LeaveIsFreeKind, LeaveIsFreePack, LeaveIsFreeParts, PackRefuse, dump_storage,
    import_verify, restore_storage,
};
pub use commit_cap::{ForkGenerationWitness, SnapshotFork, StableCommitCap};
pub use compact::{
    CompactRefuse, CompactStateRoot, CompactionDebt, CompactionPace, KvSeparationThreshold,
    LineageHash, MergeProof, MergeProofParts, MergedPacket, PacketContentHash, RangeClass,
    classify_range_at_commit, pace,
};
pub use contract::{FormatVersion, Storage};
pub(crate) use contract::{SystemClock, SystemClockRefuse};
pub use crypto::{
    AeadArm, AuditKey, Ciphertext, CompressedBytes, CryptoRefuse, Dek, Kek, KekUnwrapCap,
    SegmentCounter, ShredLedger, ShredReceipt, ShredSalt, ShredTombstone, WrappedShredSalt,
    compress, compress_then_encrypt, decompress, decrypt, derive_dek, encrypt, shred,
    unwrap_shred_salt, wrap_shred_salt,
};
pub use epoch::{
    CryptoDomain, EpochAdvanceCommitted, EpochGrant, FenceEpoch, IntentClear, advance,
    advance_recovery,
};
pub use failure::{
    DebtLedger, FailureLattice, KeyspaceId, OperatorHealthSurface, QuarantineRange, StoreRefuse,
};
pub use grants::{
    AncestorReadGrant, ForkGrant, Grant, GrantId, MaterializedGrant, PriorMaterialization,
    RecoveryGrant, materialize,
};
pub use idempotency::{IdempotencyEntry, IdempotencyMemo, OperationKey, OperationOutcome};
pub use keys::{Secret, SecretKeyRefuse, refuse_if_secret};
pub use merkle::{
    ChainLinkKind, ChainedStateRoot, ForkPoint, GENESIS_ROOT, MerkleChainRefuse, RootChain,
    StateRoot, as_of_root, fork_equivalence, roots_equal_at_cut,
};
pub use nonce::{DomainCounter, MintDomain, NonceLease, nonce};
pub use objects::{
    BackendContract, ConfirmedCopies, ConsistencyClass, ContentHash, Downgrade, FailureDomains,
    IntegrityVerification, ObjectDurabilityClass, ObjectId, ObjectRef, ObjectRefuse, ObjectSlot,
    ObjectStore, PermanenceCandidate, PermanenceWitness, ReclaimCertificate, Regions, Repair,
    ResolvedObject, RetentionCertificate, StagingToken, VolatilePending, gc_durable,
    reclaim_candidate, resolve,
};
pub use open::{
    EntropyArm, GenesisParams, GenesisSealed, GenesisSealedView, SizeClass, StableCommitCapArm,
    StagingTtl, StoreId, StoreOpen, StoreOpenVerb, genesis, open_with_capability,
};
pub use replica::{
    AdmissionCertificate, AdmissionCertificateParts, AuthorizingKey, AuthorizingKeyId,
    AuthorizingKeyTable, LocalProjection, OriginContinuity, ReplicaCustody, ReplicaKey,
    ReplicaRefuse, ScopeManifestDigest, ScopeManifestStatus, ScopeManifestTable, anchor_pending,
    verify_replica,
};
pub use seal::{
    CheckpointSeal, CheckpointSealParts, GENESIS_PRIOR_SEAL, NonceLeaseFloors, SealDigest,
    SealRefuse, TruncationReceipt, truncate,
};
pub use sweep::{
    AdmittedIntent, CommitOrdinal, IntentOrdinal, IntentionQueue, SweepDoor, SweepRefuse,
    SweepSealFailure, SweepSession,
};
pub use transcript::{
    CanonicalTranscript, CanonicalTranscriptBuilder, FieldId, MapValue, SealedArtifactKind,
    TranscriptRefuse, encode_golden_fixture, parse_golden_hex,
};
pub use tx::{
    Aborted, Applied, BackendIoError, CommitCorruption, CommitFailure, CommitIo, Committed,
    ConflictError, ReadTx, Slice, WriteTx,
};
pub use wal::{WalFloors, WalPayload, WalRecord, WalReplayState, WalSegment, replay};
