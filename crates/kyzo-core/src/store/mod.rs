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

pub(crate) mod contract;
pub(crate) mod tx;
pub(crate) mod fjall;
pub(crate) mod backup;
pub(crate) mod verify_walk;
pub(crate) mod skip_walk;
#[allow(dead_code)]
pub(crate) mod merkle;
pub(crate) mod retry;
#[allow(dead_code)]
pub(crate) mod scratch;
pub(crate) mod keys;
pub(crate) mod time;
/// Identity + open capability + genesis (07 seat).
pub(crate) mod open;
/// WriteAuthority + incarnation + RecoveryMatrix + address fence (07 seat).
pub(crate) mod authority;
/// FenceEpoch + CryptoDomain + EpochGrant advance (07 seat).
pub(crate) mod epoch;
/// ForkGrant / RecoveryGrant + pure materialize (07 seat).
pub(crate) mod grants;
/// SweepDoor + IntentOrdinal/CommitOrdinal + Committed mint (07 seat).
pub(crate) mod sweep;
/// StableCommitCap closed sum + SnapshotFork + ForkGenerationWitness (07 seat).
pub(crate) mod commit_cap;
/// WAL segment format + cross-segment hash chain + replay (07 seat).
pub(crate) mod wal;
/// NonceLease + MintDomain + DomainCounter + pure nonce fn (07 seat).
pub(crate) mod nonce;

// Sim instrument lives at kyzo-crashfs (seat); path-included here so the
// sealed Storage trait can admit it as the contract's own test double.
// Path is relative to this file (`store/mod.rs`).
#[cfg(any(test, feature = "bench-internals"))]
#[cfg_attr(all(feature = "bench-internals", not(test)), allow(dead_code))]
#[path = "../../../kyzo-crashfs/src/sim.rs"]
pub(crate) mod sim;

pub use contract::{FormatVersion, Storage};
pub(crate) use contract::{SystemClock, SystemClockRefuse};
pub use tx::{
    Aborted, BackendIoError, CommitCorruption, CommitFailure, CommitIo, Committed, ConflictError,
    ReadTx, Slice, WriteTx,
};
pub use open::{
    EntropyArm, GenesisParams, GenesisSealed, GenesisSealedView, SizeClass, StagingTtl,
    StableCommitCapArm, StoreId, StoreOpen, StoreOpenVerb, genesis, open_with_capability,
};
pub use authority::{
    AddressFence, AddressFenceRefuse, AddressFenceTable, Entropy, IncarnationId, IncarnationMintCap,
    OpenOrdinal, RecoveryMatrix, RecoveryPublicKey, WriteAuthority,
};
pub use epoch::{
    CryptoDomain, EpochAdvanceCommitted, EpochGrant, FenceEpoch, IntentClear, advance,
    advance_recovery,
};
pub use grants::{
    AncestorReadGrant, ForkGrant, Grant, GrantId, MaterializedGrant, PriorMaterialization,
    RecoveryGrant, materialize,
};
pub use sweep::{
    AdmittedIntent, CommitOrdinal, IntentOrdinal, IntentionQueue, SweepDoor, SweepRefuse,
    SweepSealFailure, SweepSession,
};
pub use commit_cap::{ForkGenerationWitness, SnapshotFork, StableCommitCap};
pub use nonce::{DomainCounter, MintDomain, NonceLease, nonce};
pub use wal::{WalFloors, WalPayload, WalRecord, WalReplayState, WalSegment, replay};
