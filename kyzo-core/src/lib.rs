/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! KyzoDB storage engine core.
//!
//! The KyzoDB storage kernel: the memcomparable key encoding, the value
//! model it encodes, the `Storage`/`StoreTx` contract, the `fjall` backend,
//! integrity verification, and the pure-Rust backup/restore. The query
//! engine layers build on this kernel.

// Zero `unsafe` is a compiler guarantee in this crate, not a convention:
// the CI ratchet (ci/unsafe-baseline.txt = 0) is defense in depth on top.
#![forbid(unsafe_code)]
// The `Storage`/`StoreTx` trait signatures return boxed iterator types by
// design; naming them would not simplify the contract.
#![allow(clippy::type_complexity)]
// `DataValue` is used as a set/map key throughout (e.g. `DataValue::Set`);
// clippy flags it as a "mutable key type" through false-positive interior-
// mutability detection in its field types. Keys are never mutated via shared
// references.
#![allow(clippy::mutable_key_type)]

pub(crate) mod data;
pub(crate) mod storage;

pub use data::tuple::{Tuple, encode_tuple_key};
pub use data::value::{
    DataValue, JsonData, Num, RegexWrapper, UuidWrapper, Validity, ValidityTs, VecElementType,
    Vector, current_validity,
};
pub use storage::backup::{dump_storage, restore_storage};
pub use storage::fjall::{
    FjallStorage, StorageOptions, StorageStats, new_fjall_storage, new_fjall_storage_with,
};
pub use storage::verify::{CorruptEntry, VerifyReport, verify_storage};
pub use storage::{ConflictError, Storage, StoreTx};
