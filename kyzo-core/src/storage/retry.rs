/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Conflict-retry: the liveness half of optimistic concurrency.
//!
//! Under SSI, a conflicted transaction is not a failure — it is an
//! instruction to rerun. This is the single place that instruction is
//! obeyed: retryable work is expressed as a closure that builds, runs, and
//! commits a fresh transaction; [`ConflictError`] triggers a rerun and every
//! other error propagates untouched.

use miette::Result;

use crate::storage::ConflictError;

/// Run `attempt` until it commits without conflict.
///
/// `attempt` must be a complete transaction cycle — create, read/write,
/// commit — so each retry sees a fresh snapshot. Non-conflict errors
/// propagate immediately. `max_attempts` bounds pathological contention;
/// exhausting it returns the final [`ConflictError`].
pub fn retry_on_conflict<T>(
    max_attempts: usize,
    mut attempt: impl FnMut() -> Result<T>,
) -> Result<T> {
    debug_assert!(max_attempts > 0);
    let mut last_err = None;
    for _ in 0..max_attempts {
        match attempt() {
            Ok(v) => return Ok(v),
            Err(e) if e.downcast_ref::<ConflictError>().is_some() => {
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.expect("max_attempts > 0"))
}
