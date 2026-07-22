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
/// AdmissionCertificate + verify_replica + NamespacedRecordIdentity + crossing validation + promotion (#270).
pub(crate) mod replica;
pub(crate) mod retry;
pub(crate) mod scratch;
/// CheckpointSeal + truncate consume + seal verification (07 seat).
pub(crate) mod seal;
pub(crate) mod skip_walk;
/// SweepDoor + IntentOrdinal/CommitOrdinal + Applied/Committed mint (07 seat).
pub(crate) mod sweep;
pub(crate) mod time;
/// CanonicalTranscript + production golden vectors + unknown-version refuse (§59).
pub(crate) mod transcript;
pub(crate) mod tx;
pub(crate) mod verify_walk;
/// WAL segment format + cross-segment hash chain + replay (07 seat).
pub(crate) mod wal;

// Sim instrument lives at kyzo-crashfs (seat); path-included here so the
// sealed Storage trait can admit it as the contract's own test double.
// Path is relative to this file (`store/mod.rs`).
#[cfg(any(test, feature = "bench-internals"))]
#[path = "../../../kyzo-crashfs/src/sim.rs"]
pub(crate) mod sim;

// Seat reexports: crate-visible names for hosts and mid-wiring.
// Production-used names stay live; test-corpus flat imports are cfg(test);
// unused flat reexports are cut (callers use module paths).
pub use authority::{
    WriteAuthority,
};
#[cfg(test)]
pub use authority::{
    Entropy, IncarnationId, OpenOrdinal, RecoveryMatrix,
};
// Leave-is-free external import stays fail-closed (#359 / #375 T5): no flat
// reexport of ImportCapability / OriginRootRegistry — module-path only.

pub use commit_cap::{
    SnapshotFork,
};
#[cfg(test)]
pub use commit_cap::{
    StableCommitCap,
};

pub use contract::{
    FormatVersion, Storage,
};
pub(crate) use contract::{
    SystemClock,
};
#[cfg(test)]
pub use crypto::{
    AeadArm, Kek, KekUnwrapCap, Nonce, SegmentCounter, ShredSalt,
    compress_then_encrypt, decompress, decrypt, derive_dek,
};
pub use epoch::{
    CryptoDomain, FenceEpoch,
};
#[cfg(test)]
pub use failure::{
    FailureLattice, StoreRefuse,
};
#[cfg(test)]
pub use grants::{
    ForkGrant, Grant, GrantId, PriorMaterialization, RecoveryGrant, materialize,
};
#[cfg(test)]
pub use idempotency::{
    IdempotencyMemo, OperationKey, OperationOutcome, RequestDigest,
};

#[cfg(test)]
pub use merkle::{
    StateRoot,
};
#[cfg(test)]
pub use nonce::{
    DomainCounter, MintDomain, nonce,
};
#[cfg(test)]
pub use objects::{
    BackendContract, ConfirmedCopies, ConsistencyClass, ContentHash, Downgrade,
    FailureDomains, IntegrityVerification, ObjectDurabilityClass, ObjectId, ObjectRef,
    ObjectRefuse, PermanenceCandidate, PermanenceWitness, ReclaimCertificate, Regions,
    StagingToken, VolatilePending, reclaim_candidate,
};
pub use open::{
    EntropyArm, GenesisParams, GenesisSealed, GenesisSealedView, SizeClass,
    StableCommitCapArm, StagingTtl, StoreId, StoreOpen, genesis,
};
#[cfg(test)]
pub use replica::{
    ReplicaCustody, ReplicaKey, ScopeManifestDigest, ScopeManifestStatus,
    ScopeManifestTable,
};
#[cfg(test)]
pub use seal::{
    CheckpointSealParts, GENESIS_PRIOR_SEAL, NonceLeaseFloors, SealDigest, SealRefuse,
};
pub use sweep::{
    CommitOrdinal, SweepRefuse, SweepSealFailure,
};
#[cfg(test)]
pub use sweep::{
    SweepDoor, SweepSession,
};
#[cfg(test)]
pub use transcript::{
    CanonicalTranscript, SealedArtifactKind, TranscriptRefuse,
    encode_normative_production_transcript, parse_golden_hex,
};
pub use tx::{
    Aborted, Applied, BackendIoError, CommitCorruption, CommitFailure, CommitIo,
    Committed, ConflictError, ReadTx, WriteTx,
};
#[cfg(test)]
pub use tx::{
    Slice,
};
#[cfg(test)]
pub use wal::{
    WalHash,
};
