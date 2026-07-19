/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Root tamper evidence (story #289): independently recompute a plaintext-
//! canonical [`StateRoot`] from store contents and compare it to the stored
//! [`RootChain`] tip via [`as_of_root`] / [`roots_equal_at_cut`]. The
//! expected digest is always looked up from the chain; the observed digest
//! is always a cold rescan — a caller-supplied root is never an input to
//! this door.
//!
//! ## Query-answer `::verify` — disclosed [OPEN]
//!
//! The query-answer capability (`::verify { <query> }`) that once ran the
//! production evaluator against `kyzo_oracle::eval` was **removed** from this
//! crate (storage-era crate wall: kyzo-core must never depend on kyzo-oracle).
//! The oracle-differential corpus is re-homed into `kyzo-trials` (zone-trials),
//! preserved pending the provenance rebuild.
//!
//! Until then, `SysOp::Verify` returns [`EngineRefuse::IndexOpNotLanded`] —
//! the same **disclosed** "parses, not landed" door as `::explain`
//! (`session/db.rs::run_sys_op`). That is not a silent stub: it is an honest
//! [OPEN] to the witness/checker work (`#257` Witness Law / `#258` The
//! Checker): provenance-backed `::verify` via
//! [`crate::exec::provenance`] (`provenance_graph` / `verify_proof`).
//! RA premise attribution is landed (`CompiledRuleBody::premise_sources`
//! + `want_premises` grounding rows); session wiring of `SysOp::Verify` is
//! the remaining door. Do not fake that door here.

use miette::Result;

use crate::store::merkle::{ChainedStateRoot, RootChain, StateRoot, as_of_root, link_at_cut, roots_equal_at_cut, state_root};
use crate::store::{CommitOrdinal, ReadTx};

/// Outcome of root tamper-evidence [`verify`]: intact chain match, or a
/// reproducible mismatch between the stored tip and an independent rescan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootVerifyOutcome {
    /// Recomputed content seals to the stored [`RootChain`] tip at the cut.
    Intact { root: StateRoot },
    /// Store contents no longer seal to the chain tip — tamper or rollback.
    Tampered {
        expected: StateRoot,
        recomputed: StateRoot,
    },
}

/// Tamper-evidence door: independently recompute the store's plaintext-
/// canonical content root and compare it to the stored [`RootChain`] at
/// `cut` via [`as_of_root`].
///
/// Authority split (load-bearing):
/// - **expected** — only from [`as_of_root`] / [`RootChain`] (stored prior);
/// - **observed** — only from a cold [`state_root`] rescan of `tx`.
///
/// A caller-supplied [`StateRoot`] is not a parameter of this door and
/// cannot become the expected digest. Forged / swapped store bytes surface
/// as [`RootVerifyOutcome::Tampered`].
pub(crate) fn verify(
    tx: &impl ReadTx,
    chain: &RootChain,
    cut: CommitOrdinal,
    budget: std::num::NonZeroU64,
) -> Result<RootVerifyOutcome> {
    let expected = as_of_root(chain, cut)?;
    let link = link_at_cut(chain, cut)?;
    debug_assert_eq!(
        expected,
        link.root(),
        "as_of_root and link_at_cut must name the same tip"
    );

    let content = StateRoot::from_merkle(state_root(tx, budget)?);
    let recomputed = ChainedStateRoot::mint(
        link.store_id(),
        link.fence_epoch(),
        link.commit_ordinal(),
        content,
        link.predecessor_root(),
        link.link(),
    )
    .root();

    if roots_equal_at_cut(expected, recomputed) {
        Ok(RootVerifyOutcome::Intact { root: expected })
    } else {
        Ok(RootVerifyOutcome::Tampered {
            expected,
            recomputed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::fjall::new_fjall_storage;

    fn merkle_budget() -> std::num::NonZeroU64 {
        std::num::NonZeroU64::new(1_000_000).unwrap()
    }

    /// Intact store + lawful chain tip → [`RootVerifyOutcome::Intact`].
    /// A forged [`StateRoot`] sitting in scope is never consulted: `verify`
    /// takes only `(tx, chain, cut, budget)`.
    #[test]
    fn verify_intact_store_matches_stored_root_chain_tip() {
        use crate::store::epoch::FenceEpoch;
        use crate::store::merkle::{ChainLinkKind, GENESIS_ROOT};
        use crate::store::open::StoreId;
        use crate::store::{Storage, WriteTx};

        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let content: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"k00".to_vec(), b"v0".to_vec()),
            (b"k01".to_vec(), b"v1".to_vec()),
            (b"k02".to_vec(), b"v2".to_vec()),
        ];
        {
            let mut tx = db.write_tx().unwrap();
            for (k, v) in &content {
                tx.put(k, v).unwrap();
            }
            tx.commit().unwrap();
        }

        let store_id = StoreId::from_digest([0x29; 32]);
        let fence = FenceEpoch::genesis(store_id);
        let cut = CommitOrdinal::ZERO.successor().unwrap();

        let tx = db.read_tx().unwrap();
        let content_root = StateRoot::from_merkle(state_root(&tx, merkle_budget()).unwrap());

        let mut chain = RootChain::empty();
        assert_eq!(chain.prior_root(), GENESIS_ROOT);
        let link = ChainedStateRoot::mint(
            store_id,
            fence,
            cut,
            content_root,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        );
        chain.append(link).unwrap();

        // A forged digest in scope — verify never takes it as input.
        let _forged_caller_root = StateRoot::from_digest([0xDE; 32]);
        assert_ne!(_forged_caller_root, as_of_root(&chain, cut).unwrap());

        match verify(&tx, &chain, cut, merkle_budget()).expect("verify runs") {
            RootVerifyOutcome::Intact { root } => {
                assert_eq!(root, as_of_root(&chain, cut).unwrap());
                assert!(roots_equal_at_cut(root, chain.prior_root()));
            }
            RootVerifyOutcome::Tampered { expected, recomputed } => {
                panic!("expected Intact, got Tampered {{ expected: {expected:?}, recomputed: {recomputed:?} }}")
            }
        }
    }

    /// Real security proof: mutate one stored value after the chain tip is
    /// sealed. Independent rescan + rebind ≠ [`as_of_root`] tip → Tampered.
    /// Cold merkle of the tampered bytes alone is well-formed; detection is
    /// comparison against the stored [`RootChain`], not AEAD or a delivered
    /// digest.
    #[test]
    fn verify_detects_store_tamper_against_stored_root_chain() {
        use crate::store::epoch::FenceEpoch;
        use crate::store::merkle::ChainLinkKind;
        use crate::store::open::StoreId;
        use crate::store::{Storage, WriteTx};

        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        {
            let mut tx = db.write_tx().unwrap();
            tx.put(b"k00", b"honest-v0").unwrap();
            tx.put(b"k01", b"honest-v1").unwrap();
            tx.commit().unwrap();
        }

        let store_id = StoreId::from_digest([0xA4; 32]);
        let fence = FenceEpoch::genesis(store_id);
        let cut = CommitOrdinal::ZERO.successor().unwrap();

        let content_root = {
            let tx = db.read_tx().unwrap();
            StateRoot::from_merkle(state_root(&tx, merkle_budget()).unwrap())
        };

        let mut chain = RootChain::empty();
        let link = ChainedStateRoot::mint(
            store_id,
            fence,
            cut,
            content_root,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        );
        chain.append(link).unwrap();
        let expected_tip = as_of_root(&chain, cut).unwrap();

        // Attacker swaps one value under the sealed tip.
        {
            let mut tx = db.write_tx().unwrap();
            tx.put(b"k00", b"TAMPERED!!").unwrap();
            tx.commit().unwrap();
        }

        let tx = db.read_tx().unwrap();
        let tampered_content =
            StateRoot::from_merkle(state_root(&tx, merkle_budget()).unwrap());
        assert_ne!(
            tampered_content, content_root,
            "tamper must change the cold content root"
        );

        match verify(&tx, &chain, cut, merkle_budget()).expect("verify runs") {
            RootVerifyOutcome::Tampered {
                expected,
                recomputed,
            } => {
                assert_eq!(expected, expected_tip);
                assert_ne!(recomputed, expected);
                assert!(!roots_equal_at_cut(expected, recomputed));
            }
            RootVerifyOutcome::Intact { root } => {
                panic!("tampered store must not Intact; got root={root:?}")
            }
        }
    }

    /// Valid-but-stale rollback: an older internally-consistent snapshot
    /// under a tip advanced past that cut. `verify` at the tip cut reports
    /// Tampered — same detection class as merkle's stored-prior test, wired
    /// through the session verify door.
    #[test]
    fn verify_detects_valid_but_stale_rollback_at_tip_cut() {
        use crate::store::epoch::FenceEpoch;
        use crate::store::merkle::ChainLinkKind;
        use crate::store::open::StoreId;
        use crate::store::{Storage, WriteTx};

        let store_id = StoreId::from_digest([0x58; 32]);
        let fence = FenceEpoch::genesis(store_id);

        let state_v1: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"k00".to_vec(), b"v0".to_vec()),
            (b"k01".to_vec(), b"v1".to_vec()),
        ];
        let state_v2: Vec<(Vec<u8>, Vec<u8>)> = {
            let mut s = state_v1.clone();
            s.push((b"k02".to_vec(), b"v2".to_vec()));
            s
        };
        let state_v3: Vec<(Vec<u8>, Vec<u8>)> = {
            let mut s = state_v2.clone();
            s.push((b"k03".to_vec(), b"v3".to_vec()));
            s
        };

        fn write_state(pairs: &[(Vec<u8>, Vec<u8>)]) -> crate::store::fjall::FjallStorage {
            let dir = tempfile::tempdir().unwrap();
            let db = new_fjall_storage(dir.path()).unwrap();
            std::mem::forget(dir);
            let mut tx = db.write_tx().unwrap();
            for (k, v) in pairs {
                tx.put(k, v).unwrap();
            }
            tx.commit().unwrap();
            db
        }

        let db_v1 = write_state(&state_v1);
        let content_v1 = StateRoot::from_merkle(
            state_root(&db_v1.read_tx().unwrap(), merkle_budget()).unwrap(),
        );
        let db_v2 = write_state(&state_v2);
        let content_v2 = StateRoot::from_merkle(
            state_root(&db_v2.read_tx().unwrap(), merkle_budget()).unwrap(),
        );
        let db_v3 = write_state(&state_v3);
        let content_v3 = StateRoot::from_merkle(
            state_root(&db_v3.read_tx().unwrap(), merkle_budget()).unwrap(),
        );
        assert_ne!(content_v1, content_v2);
        assert_ne!(content_v2, content_v3);

        let o1 = CommitOrdinal::ZERO.successor().unwrap();
        let o2 = o1.successor().unwrap();
        let o3 = o2.successor().unwrap();

        let mut chain = RootChain::empty();
        chain
            .append(ChainedStateRoot::mint(
                store_id,
                fence,
                o1,
                content_v1,
                chain.prior_root(),
                ChainLinkKind::Ordinary,
            ))
            .unwrap();
        chain
            .append(ChainedStateRoot::mint(
                store_id,
                fence,
                o2,
                content_v2,
                chain.prior_root(),
                ChainLinkKind::Ordinary,
            ))
            .unwrap();
        chain
            .append(ChainedStateRoot::mint(
                store_id,
                fence,
                o3,
                content_v3,
                chain.prior_root(),
                ChainLinkKind::Ordinary,
            ))
            .unwrap();

        // Live tip store matches tip cut.
        assert!(matches!(
            verify(&db_v3.read_tx().unwrap(), &chain, o3, merkle_budget()).unwrap(),
            RootVerifyOutcome::Intact { .. }
        ));

        // Attacker restores an older internally-consistent backup (v1 bytes).
        let rolled = write_state(&state_v1);
        let rolled_content = StateRoot::from_merkle(
            state_root(&rolled.read_tx().unwrap(), merkle_budget()).unwrap(),
        );
        assert_eq!(
            rolled_content, content_v1,
            "rolled-back store must be internally consistent with v1"
        );

        match verify(&rolled.read_tx().unwrap(), &chain, o3, merkle_budget()).unwrap() {
            RootVerifyOutcome::Tampered {
                expected,
                recomputed,
            } => {
                assert_eq!(expected, as_of_root(&chain, o3).unwrap());
                assert!(!roots_equal_at_cut(expected, recomputed));
            }
            RootVerifyOutcome::Intact { .. } => {
                panic!("valid-but-stale rollback at tip must Tamper")
            }
        }

        // Same rolled-back bytes still Intact at the older cut they match.
        match verify(&rolled.read_tx().unwrap(), &chain, o1, merkle_budget()).unwrap() {
            RootVerifyOutcome::Intact { root } => {
                assert_eq!(root, as_of_root(&chain, o1).unwrap());
            }
            RootVerifyOutcome::Tampered { .. } => {
                panic!("v1 bytes must Intact against as-of cut o1")
            }
        }
    }
}
