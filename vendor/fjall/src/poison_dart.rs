// Copyright (c) 2024-present, fjall-rs
// This source code is licensed under both the Apache 2.0 and MIT License
// (found in the LICENSE-* files in the repository)

//! Fault carriage for background workers.
//!
//! **Unknown-invariant** (fsync/hardware failure, worker panic) flips the
//! store-global poison flag → [`crate::Error::Poisoned`].
//!
//! **Scoped block checksum mismatch** does *not* flip that flag. It reports
//! through [`PoisonDart::report_scoped`] so a host (Kyzo `FailureLattice`)
//! can quarantine one range while the rest of the store keeps serving.
//! Escalating a scoped mismatch into whole-store poison is the availability
//! inversion this module exists to prevent.

use std::sync::{atomic::AtomicBool, Arc};

type PoisonSignal = Arc<AtomicBool>;

/// Reason for a scoped integrity fault (never whole-store poison).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopedFaultReason {
    /// LSM block checksum mismatch (content and/or logical identity).
    BlockChecksumMismatch,
}

/// Carriage payload for a scoped fault — host maps into its failure lattice.
#[derive(Debug, Clone)]
pub struct ScopedFault {
    /// Keyspace name when known at the fault site.
    pub keyspace: Option<Arc<str>>,
    /// Why the fault is scoped rather than unknown-invariant.
    pub reason: ScopedFaultReason,
}

/// Host callback for scoped faults (Kyzo wires this into `FailureLattice`).
pub type ScopedFaultHandler = Arc<dyn Fn(ScopedFault) + Send + Sync>;

/// RAII / worker fault carriage.
///
/// - [`Self::poison`] — unknown-invariant only (panic in worker, or caller
///   after hardware/fsync failure).
/// - [`Self::report_scoped`] — scoped checksum mismatch; does **not** set the
///   poison signal.
#[derive(Clone)]
pub struct PoisonDart {
    /// Whole-store unknown-invariant fail-stop signal.
    signal: PoisonSignal,
    /// Optional host carriage for scoped faults.
    scoped_handler: Option<ScopedFaultHandler>,
}

impl PoisonDart {
    pub fn new(signal: PoisonSignal) -> Self {
        Self {
            signal,
            scoped_handler: None,
        }
    }

    /// Attach a host handler that receives scoped fault carriage.
    #[must_use]
    pub fn with_scoped_handler(mut self, handler: ScopedFaultHandler) -> Self {
        self.scoped_handler = Some(handler);
        self
    }

    /// Unknown-invariant fail-stop: flips the store-global poison signal.
    ///
    /// Never call this for an ordinary scoped block checksum mismatch.
    pub fn poison(&self) {
        self.signal
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Report a scoped integrity fault without poisoning the store.
    ///
    /// The optional handler is the carriage into the host failure lattice.
    /// Never flips the unknown-invariant poison signal.
    pub fn report_scoped(&self, fault: ScopedFault) {
        if let Some(handler) = &self.scoped_handler {
            handler(fault);
        }
    }

    /// Whether the unknown-invariant poison signal is raised.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.signal.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Drop for PoisonDart {
    fn drop(&mut self) {
        if std::thread::panicking() {
            log::error!("Poisoning database because of panic in background worker");
            self.poison();
        }
    }
}

/// Classify whether an error is a scoped block checksum mismatch.
///
/// Scoped mismatches must be carried into the host lattice, never escalated
/// to [`crate::Error::Poisoned`].
#[must_use]
pub fn is_scoped_checksum_mismatch(err: &crate::Error) -> bool {
    matches!(
        err,
        crate::Error::Storage(lsm_tree::Error::ChecksumMismatch { .. })
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn scoped_fault_does_not_flip_poison_signal() {
        let signal = Arc::new(AtomicBool::new(false));
        let reported = Arc::new(AtomicBool::new(false));
        let reported_flag = reported.clone();

        let dart = PoisonDart::new(signal.clone()).with_scoped_handler(Arc::new(move |_| {
            reported_flag.store(true, Ordering::Release);
        }));

        dart.report_scoped(ScopedFault {
            keyspace: Some(Arc::from("items")),
            reason: ScopedFaultReason::BlockChecksumMismatch,
        });

        assert!(
            !signal.load(Ordering::Acquire),
            "scoped checksum mismatch must not raise store-global poison"
        );
        assert!(
            reported.load(Ordering::Acquire),
            "scoped fault must reach the carriage handler"
        );

        dart.poison();
        assert!(
            signal.load(Ordering::Acquire),
            "unknown-invariant poison must raise the signal"
        );
    }

    #[test]
    fn checksum_mismatch_error_is_scoped() {
        let err = crate::Error::Storage(lsm_tree::Error::ChecksumMismatch {
            got: lsm_tree::Checksum::from_raw(1),
            expected: lsm_tree::Checksum::from_raw(2),
        });
        assert!(is_scoped_checksum_mismatch(&err));

        let io = crate::Error::Io(std::io::Error::other("disk"));
        assert!(!is_scoped_checksum_mismatch(&io));
    }
}
