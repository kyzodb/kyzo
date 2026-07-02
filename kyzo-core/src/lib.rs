/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The KyzoDB storage kernel.
//!
//! # World model
//!
//! The type graph is the system's ontology — every public type is a claim
//! about what exists in this domain, and its constructors are the only ways
//! that thing can come to be:
//!
//! - [`DataValue`] — the atom of meaning: totally ordered, serializable.
//!   Thirteen kinds whose declaration order *is* the cross-type order.
//! - [`Tuple`] — a fact's body: an ordered sequence of values.
//! - [`Validity`] / [`ValidityTs`] — a time-stamped existence claim,
//!   ordered newest-first; retraction is a first-class assertion of absence.
//! - [`EncodedKey`] — a fact's written form: relation prefix, memcomparable
//!   tuple bytes, fixed-width validity tail. Constructed only by encoders;
//!   possession proves provenance. Bytes read back from disk are *claimed*
//!   keys until fallible decoding proves them.
//! - [`Storage`] — a universe of facts. Hands out transactions; can bulk
//!   import, fsync, verify, dump, and restore itself.
//! - [`ReadTx`] / [`WriteTx`] — the two species of transaction. A reader is
//!   one consistent snapshot and cannot write *by construction*; a writer
//!   adds a conflict-tracked write set, and committing consumes it — the
//!   committed-but-alive transaction does not exist as a state.
//! - [`ConflictError`] — the typed, retryable refusal of a commit; see
//!   [`retry_on_conflict`] for the liveness half.
//! - [`FormatVersion`] — the identity of the on-disk encoding, stamped in
//!   every store and dump; a mismatch refuses to open.
//! - [`VerifyReport`] — the store's integrity, made inspectable.
//!
//! The load-bearing law beneath all of it: encoded byte order equals
//! semantic value order, so one ordered keyspace serves every access path.
//! The query engine layers build on this kernel.

// Zero `unsafe` is a compiler guarantee in this crate, not a convention;
// CI checks that this attribute stays.
#![forbid(unsafe_code)]
// The transaction traits return boxed iterator types by design; naming them
// would not simplify the contract.
#![allow(clippy::type_complexity)]
// `DataValue` is used as a set/map key throughout (e.g. `DataValue::Set`);
// clippy flags it as a "mutable key type" through false-positive interior-
// mutability detection in its field types. Keys are never mutated via shared
// references.
#![allow(clippy::mutable_key_type)]

pub(crate) mod data;
pub(crate) mod query;
pub(crate) mod storage;

pub use data::tuple::{EncodedKey, Tuple, encode_tuple_key};
pub use data::value::{
    DataValue, JsonData, Num, RegexWrapper, UuidWrapper, Validity, ValidityTs, VecElementType,
    Vector, current_validity,
};
pub use storage::backup::{dump_storage, restore_storage};
pub use storage::fjall::{
    FjallReadTx, FjallStorage, FjallWriteTx, StorageOptions, StorageStats, new_fjall_storage,
    new_fjall_storage_with,
};
pub use storage::retry::retry_on_conflict;
pub use storage::verify::{CorruptEntry, VerifyReport, verify_storage};
pub use storage::{ConflictError, FormatVersion, ReadTx, Storage, WriteTx};
