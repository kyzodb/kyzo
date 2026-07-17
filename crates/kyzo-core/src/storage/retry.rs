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
//! commits a fresh transaction; [`RetryError::Conflict`] (from
//! [`ConflictError`] / [`CommitFailure::Conflict`] via [`From`], or
//! [`RetryError::classify`] on a residual Report) triggers a rerun and
//! every other error propagates untouched.

use std::num::NonZeroUsize;

use miette::{Result, miette};

use crate::storage::{CommitFailure, ConflictError};

/// Typed channel into the conflict-retry loop: conflict (rerun) or fatal.
///
/// Construct via [`From<ConflictError>`] / [`From<CommitFailure>`], or
/// [`Self::classify`] when an attempt still surfaces an erased Report.
/// Classification never uses `downcast_ref`.
#[derive(Debug)]
pub(crate) enum RetryError {
    /// SSI conflict — discard the attempt; retry on a fresh snapshot.
    Conflict(ConflictError),
    /// Non-retryable refusal; propagate as the attempt's outcome.
    Other(miette::Report),
}

impl RetryError {
    /// Admit a residual [`miette::Report`] into this channel without
    /// `downcast_ref`: only diagnostic code `storage::conflict` (the code
    /// on [`ConflictError`], forwarded by transparent
    /// [`CommitFailure::Conflict`]) is a conflict; everything else is fatal.
    pub(crate) fn classify(r: miette::Report) -> Self {
        if r.code()
            .is_some_and(|c| c.to_string() == "storage::conflict")
        {
            Self::Conflict(ConflictError)
        } else {
            Self::Other(r)
        }
    }
}

impl From<ConflictError> for RetryError {
    fn from(c: ConflictError) -> Self {
        Self::Conflict(c)
    }
}

impl From<CommitFailure> for RetryError {
    fn from(f: CommitFailure) -> Self {
        match f {
            CommitFailure::Conflict(c) => Self::Conflict(c),
            other => Self::Other(miette::Report::from(other)),
        }
    }
}

impl From<RetryError> for miette::Report {
    fn from(e: RetryError) -> Self {
        match e {
            RetryError::Conflict(c) => c.into(),
            RetryError::Other(r) => r,
        }
    }
}

/// Run `attempt` until it commits without conflict, retrying HOT (no
/// pause between attempts).
///
/// `attempt` must be a complete transaction cycle — create, read/write,
/// commit — so each retry sees a fresh snapshot. Non-conflict errors
/// propagate immediately. `max_attempts` bounds pathological contention;
/// exhausting it returns the final [`crate::storage::ConflictError`].
/// Zero attempts are unrepresentable: the budget is [`NonZeroUsize`] at the
/// API, not a runtime check after mint.
///
/// Attempt errors are [`miette::Report`]; retry classifies them into
/// [`RetryError`] via [`RetryError::classify`] (diagnostic code
/// `storage::conflict`, never `downcast_ref`). Typed
/// [`CommitFailure`] / [`ConflictError`] enter the same channel through
/// [`From`] when constructed directly.
///
/// Hot retry is for harnesses whose conflicts are injected or simulated
/// (the DST campaigns): pausing there wastes wall-clock on races that
/// virtual time already resolves. Real concurrent sessions retry through
/// [`retry_on_conflict_with_backoff`] instead.
pub fn retry_on_conflict<T>(
    max_attempts: NonZeroUsize,
    mut attempt: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut last_err = None;
    for _ in 0..max_attempts.get() {
        match attempt() {
            Ok(v) => return Ok(v),
            Err(e) => match RetryError::classify(e) {
                RetryError::Conflict(c) => {
                    last_err = Some(c);
                }
                RetryError::Other(r) => return Err(r),
            },
        }
    }
    // INVARIANT(retry): NonZero attempts + conflict-only continuation ⇒ last_err is Some.
    Err(last_err
        .map(Into::into)
        .unwrap_or_else(|| miette!("conflict retry exhausted without a stored error")))
}

/// As [`retry_on_conflict`], with losses backing off: the first retries
/// yield the scheduler slot, later ones sleep with exponential growth
/// (capped). Hot-spinning loses fairness under real contention — a
/// writer racing N rivals on one fact can lose every round while the
/// winners immediately re-enter; the backoff de-synchronizes the herd so
/// every writer eventually lands. The session tier's mutation path
/// retries through this. Timing is the only thing affected; answers
/// never depend on it.
pub fn retry_on_conflict_with_backoff<T>(
    max_attempts: NonZeroUsize,
    mut attempt: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut last_err = None;
    for n in 0..max_attempts.get() {
        match attempt() {
            Ok(v) => return Ok(v),
            Err(e) => match RetryError::classify(e) {
                RetryError::Conflict(c) => {
                    last_err = Some(c);
                    backoff(n);
                }
                RetryError::Other(r) => return Err(r),
            },
        }
    }
    // INVARIANT(retry): NonZero attempts + conflict-only continuation ⇒ last_err is Some.
    Err(last_err
        .map(Into::into)
        .unwrap_or_else(|| miette!("conflict retry exhausted without a stored error")))
}

/// The `n`-th loss's pause: yield for the first few, then sleep,
/// doubling to a cap.
#[cfg(not(target_arch = "wasm32"))]
fn backoff(n: usize) {
    if n < 3 {
        std::thread::yield_now();
    } else {
        let ms = 1u64 << (n - 3).min(6); // 1ms .. 64ms
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// Single-threaded wasm has no rival to wait out.
#[cfg(target_arch = "wasm32")]
fn backoff(_n: usize) {}
