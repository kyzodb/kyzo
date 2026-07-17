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
//! [`ConflictError`] / [`CommitFailure::Conflict`] via [`From`]) triggers a
//! rerun and every other error propagates untouched.

use std::num::NonZeroUsize;

use miette::Result;

use crate::storage::{
    CommitCorruption, CommitFailure, CommitIo, ConflictError, ReadTx, Storage, WriteTx,
};

/// Session/query refusal carried mid-attempt.
///
/// Wraps [`miette::Report`] without `thiserror`'s `#[error(transparent)]` on
/// Report (that path demands `as_dyn_error`, which Report does not satisfy).
/// Construct only via [`RetryError::session`] / [`RetryError::session_report`]
/// — never `From<Report>` on [`RetryError`].
#[derive(Debug)]
pub(crate) struct SessionRefuse(miette::Report);

impl std::fmt::Display for SessionRefuse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for SessionRefuse {}

/// Typed non-retryable refusal inside a conflict-retry attempt.
///
/// Conflict vs fatal is decided only by [`RetryError`] variant — never by
/// diagnostic code or string identity. There is no [`miette::Report`] erase
/// channel here: commit fatals are [`CommitIo`] / [`CommitCorruption`];
/// storage ops used mid-attempt are [`StorageOpFailure`]; session/query
/// refusals enter only through [`Self::Session`] via an explicit constructor
/// (never `From<Report>` on [`RetryError`], so `?` on `miette::Result` cannot
/// silently erase into this channel).
///
/// Not a miette Diagnostic: a manual [`From`] into [`miette::Report`] owns
/// the lift so Diagnostic-derive cannot fight it.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RetryFatal {
    /// Commit-time IO (including durability shortfall after apply).
    #[error(transparent)]
    Io(#[from] CommitIo),

    /// Commit-time corruption.
    #[error(transparent)]
    Corruption(#[from] CommitCorruption),

    /// Mid-attempt storage op (write_tx / get / put / …) refused.
    #[error(transparent)]
    StorageOp(#[from] StorageOpFailure),

    /// Session/query refusal observed mid-attempt.
    /// Built only via [`RetryError::session`] / [`RetryError::session_report`].
    #[error(transparent)]
    Session(#[from] SessionRefuse),
}

/// Named mid-attempt storage-op refusal. Variant identity is the op — not a
/// formatted string and not a residual Report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
#[diagnostic(code(storage::op))]
pub(crate) enum StorageOpFailure {
    #[error("storage write_tx failed")]
    WriteTx,
    #[error("storage get failed")]
    Get,
    #[error("storage put failed")]
    Put,
    #[error("sim: injected transient read fault")]
    SimInjectedReadFault,
}

/// Typed channel into the conflict-retry loop: conflict (rerun) or fatal.
///
/// Construct via [`From<ConflictError>`] / [`From<CommitFailure>`] /
/// [`From<StorageOpFailure>`], or [`RetryError::session`] for session-tier
/// Diagnostics. There is no `From<miette::Report>` — attempt bodies cannot
/// `?` a `miette::Result` into this channel.
///
/// Not a miette Diagnostic: [`From<RetryError> for Report`] is the sole lift.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RetryError {
    /// SSI conflict — discard the attempt; retry on a fresh snapshot.
    #[error(transparent)]
    Conflict(ConflictError),
    /// Non-retryable refusal; propagate as the attempt's outcome.
    #[error(transparent)]
    Fatal(RetryFatal),
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
            CommitFailure::Io(io) => Self::Fatal(RetryFatal::Io(io)),
            CommitFailure::Corruption(c) => Self::Fatal(RetryFatal::Corruption(c)),
        }
    }
}

impl From<StorageOpFailure> for RetryError {
    fn from(op: StorageOpFailure) -> Self {
        Self::Fatal(RetryFatal::StorageOp(op))
    }
}

impl From<RetryFatal> for RetryError {
    fn from(f: RetryFatal) -> Self {
        Self::Fatal(f)
    }
}

impl RetryError {
    /// Lift a typed session/query Diagnostic into the fatal channel.
    pub(crate) fn session(d: impl miette::Diagnostic + Send + Sync + 'static) -> Self {
        Self::Fatal(RetryFatal::Session(SessionRefuse(miette::Report::new(d))))
    }

    /// Lift an already-built session/query Report into the fatal channel.
    /// Explicit only — not `From`, so `?` on `miette::Result` stays closed.
    pub(crate) fn session_report(r: miette::Report) -> Self {
        Self::Fatal(RetryFatal::Session(SessionRefuse(r)))
    }
}

impl From<RetryError> for miette::Report {
    fn from(e: RetryError) -> Self {
        match e {
            RetryError::Conflict(c) => c.into(),
            RetryError::Fatal(RetryFatal::Io(io)) => io.into(),
            RetryError::Fatal(RetryFatal::Corruption(c)) => c.into(),
            RetryError::Fatal(RetryFatal::StorageOp(op)) => op.into(),
            RetryError::Fatal(RetryFatal::Session(SessionRefuse(r))) => r,
        }
    }
}

/// Open a write transaction for a retry attempt — typed op failure, never Report.
pub(crate) fn write_tx_attempt<S: Storage>(db: &S) -> std::result::Result<S::WriteTx, RetryError> {
    db.write_tx()
        .map_err(|_| RetryError::from(StorageOpFailure::WriteTx))
}

/// Point-get for a retry attempt — typed op failure, never Report.
pub(crate) fn get_attempt(
    tx: &impl ReadTx,
    key: &[u8],
) -> std::result::Result<Option<::fjall::Slice>, RetryError> {
    tx.get(key)
        .map_err(|_| RetryError::from(StorageOpFailure::Get))
}

/// Put for a retry attempt — typed op failure, never Report.
pub(crate) fn put_attempt(
    tx: &mut impl WriteTx,
    key: &[u8],
    val: &[u8],
) -> std::result::Result<(), RetryError> {
    tx.put(key, val)
        .map_err(|_| RetryError::from(StorageOpFailure::Put))
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
/// Attempt errors are [`RetryError`]: conflict vs fatal is decided only by
/// matching [`RetryError::Conflict`] / [`RetryError::Fatal`]. Typed
/// [`CommitFailure`] / [`ConflictError`] / [`StorageOpFailure`] enter through
/// [`From`]; session Reports only through [`RetryError::session_report`].
///
/// Hot retry is for harnesses whose conflicts are injected or simulated
/// (the DST campaigns): pausing there wastes wall-clock on races that
/// virtual time already resolves. Real concurrent sessions retry through
/// [`retry_on_conflict_with_backoff`] instead.
pub fn retry_on_conflict<T>(
    max_attempts: NonZeroUsize,
    mut attempt: impl FnMut() -> std::result::Result<T, RetryError>,
) -> Result<T> {
    let max = max_attempts.get();
    // NonZero ⇒ at least one attempt. Only Conflict continues the loop, so
    // `last_conflict` is always a stored ConflictError when we exhaust.
    let mut last_conflict = match attempt() {
        Ok(v) => return Ok(v),
        Err(RetryError::Conflict(c)) => c,
        Err(e) => return Err(e.into()),
    };
    for _ in 1..max {
        match attempt() {
            Ok(v) => return Ok(v),
            Err(RetryError::Conflict(c)) => last_conflict = c,
            Err(e) => return Err(e.into()),
        }
    }
    Err(last_conflict.into())
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
    mut attempt: impl FnMut() -> std::result::Result<T, RetryError>,
) -> Result<T> {
    let max = max_attempts.get();
    let mut last_conflict = match attempt() {
        Ok(v) => return Ok(v),
        Err(RetryError::Conflict(c)) => {
            backoff(0);
            c
        }
        Err(e) => return Err(e.into()),
    };
    for n in 1..max {
        match attempt() {
            Ok(v) => return Ok(v),
            Err(RetryError::Conflict(c)) => {
                last_conflict = c;
                backoff(n);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(last_conflict.into())
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
