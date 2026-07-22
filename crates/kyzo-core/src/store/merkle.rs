/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Chained state roots + cold Merkle accountability (decisions.md §56–§58).
//!
//! Owns (07 Spec spine): per-commit chained state root, [`as_of_root`],
//! typed recovery/fork chain links, fork-equivalence via [`fork_point`],
//! recompute-and-compare replica equivalence via [`replica_equivalence_at_cut`],
//! signed-state-root-head (STH) bodies, RFC-6962-style
//! [`ConsistencyProof`] checking, and split-view detection on gossip
//! ([`check_sth_gossip`]) so an equivocating Store is
//! **Detected-on-gossip** before chains meet (Certificate Transparency model;
//! seats 2/56/58/69). Fabric carriage of compact STHs seats in `replica`
//! (NATS JetStream subject — never peer-dial, seat 92).
//!
//! Bans: current-only roots as the sole digest; roots over ciphertext;
//! path/URL equivalence claims; trusting a peer-delivered root as equivalence;
//! leaving split-view Unexposed-until-chains-meet when two gossiped STHs
//! disagree without a consistency proof.
//!
//! Also: a **cold, deterministic Merkle state root** over the ordered
//! keyspace — plaintext-canonical, cipher-invariant leaf commitment the
//! Spec chain binds.
//!
//! ## Cold root — what it is, and what it deliberately is not
//!
//! One root hash that is a *pure function of the committed `(key, value)`
//! set* — a content address the whole store can be compared by. It is
//! computed **cold**: a full ordered rescan of the range, folded into a
//! Merkle tree, no incremental maintenance. The scan order *is* the
//! canonical order, because the memcmp key encoding makes bytewise key
//! order equal semantic value order (`data/memcmp.rs`); so two stores with
//! identical logical content produce byte-identical roots regardless of the
//! write history that built them, and any single-byte difference in any key
//! or value produces a different root.
//!
//! The **incremental** cold-path alternative (in-keyspace tree nodes) remains
//! out of scope for the scan helper: fjall's commit order is internal and
//! its serialize point is not a hook we own. The Spec per-commit chain
//! ([`ChainedStateRoot`]) is minted at the SweepDoor durable event from
//! plaintext-canonical content — never over ciphertext.
//!
//! ## The tree
//!
//! Domain-separated, RFC-6962-style Merkle Tree Hash (MTH) over the leaf
//! sequence in key order:
//!
//! - **leaf** = `SHA-256(0x00 ‖ u64_be(key.len) ‖ key ‖ value)`. The key
//!   length prefix removes the key/value boundary ambiguity — without it,
//!   `(key=ab, value=c)` and `(key=a, value=bc)` would collide.
//! - **node** = `SHA-256(0x01 ‖ left ‖ right)`.
//! - **empty** = `SHA-256(0x02)`, a dedicated tag so an empty range can
//!   never collide with a leaf or node.
//!
//! The `0x00`/`0x01`/`0x02` domain tags are what stop a leaf hash from
//! being reinterpreted as an interior node (the classic second-preimage on
//! an undomained Merkle tree). They are pinned by golden vectors in the
//! tests; flipping a tag is a mutation the tests catch.
//!
//! ## The tree shape is canonical (RFC 6962), computed streaming
//!
//! MTH splits a run of `n` leaves at the largest power of two below `n`
//! (`k`): `MTH(D) = node(MTH(D[..k]), MTH(D[k..]))`, `MTH([d]) = leaf(d)`.
//! [`MerkleAccumulator`] computes exactly this MTH in a single streaming
//! pass with `O(log n)` memory — a stack of complete power-of-two subtrees,
//! merged whenever the top two are equal-sized, then bagged right-to-left
//! at the end. The tests cross-check it against an independent recursive MTH
//! over a materialised leaf vector, so the streaming form is not trusted on
//! its own word.

use std::num::NonZeroU64;

use fjall::Slice;
use miette::{Diagnostic, Result};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::ReadTx;
use super::epoch::FenceEpoch;
use super::open::StoreId;
use super::sweep::CommitOrdinal;
use super::transcript::{encode_chained_state_root, encode_state_root_head};
use super::wal::WalHash;
use kyzo_model::value::RelationId;

/// The number of `(k,v)` pairs a root scan may touch before it refuses. The
/// root is `O(n)` in the range's size; an unbounded scan over a hostile
/// keyspace is a denial-of-service, so the ceiling is mandatory and the
/// refusal is typed — never a silent truncation (a truncated scan would
/// forge a root for content that is not there).
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
#[error("merkle root scan exceeded its ceiling of {ceiling} entries")]
#[diagnostic(
    code(merkle::scan_exceeded),
    help("raise the scan ceiling, or root a smaller range")
)]
pub(crate) struct MerkleScanExceeded {
    pub(crate) ceiling: u64,
}

const LEAF_TAG: u8 = 0x00;
const NODE_TAG: u8 = 0x01;
const EMPTY_TAG: u8 = 0x02;

/// A 32-byte Merkle hash. Comparison is byte-exact; rendering is lowercase
/// hex (for golden vectors and the eventual sys-op result column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub(crate) struct MerkleHash(pub(crate) [u8; 32]);

const _: () = assert!(std::mem::size_of::<MerkleHash>() == std::mem::size_of::<[u8; 32]>());
const _: () = assert!(std::mem::align_of::<MerkleHash>() == std::mem::align_of::<[u8; 32]>());

impl MerkleHash {
    /// Lowercase hex, 64 characters.
    pub(crate) fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(char::from(HEX[usize::from(b >> 4)]));
            s.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
        s
    }
}

/// `leaf = SHA-256(0x00 ‖ u64_be(key.len) ‖ key ‖ value)`.
fn leaf_hash(key: &[u8], value: &[u8]) -> MerkleHash {
    let mut h = Sha256::new();
    h.update([LEAF_TAG]);
    h.update(match u64::try_from(key.len()) {
        Ok(n) => n,
        Err(_) => u64::MAX,
    }.to_be_bytes());
    h.update(key);
    h.update(value);
    MerkleHash(h.finalize().into())
}

/// `node = SHA-256(0x01 ‖ left ‖ right)`.
fn node_hash(left: &MerkleHash, right: &MerkleHash) -> MerkleHash {
    let mut h = Sha256::new();
    h.update([NODE_TAG]);
    h.update(left.0);
    h.update(right.0);
    MerkleHash(h.finalize().into())
}

/// `empty = SHA-256(0x02)`.
fn empty_hash() -> MerkleHash {
    let mut h = Sha256::new();
    h.update([EMPTY_TAG]);
    MerkleHash(h.finalize().into())
}

/// A streaming RFC-6962 Merkle-Tree-Hash builder. Holds a stack of complete
/// subtrees whose leaf-counts are strictly decreasing powers of two (bottom
/// = leftmost/largest). Pushing a leaf merges equal-sized top subtrees;
/// finalising bags the remaining peaks right-to-left. `O(log n)` memory.
struct MerkleAccumulator {
    /// `(leaf_count, subtree_root)`, bottom-to-top = left-to-right.
    stack: Vec<(u64, MerkleHash)>,
}

impl MerkleAccumulator {
    fn new() -> Self {
        Self { stack: Vec::new() }
    }

    /// Append the next leaf in key order.
    fn push_leaf(&mut self, hash: MerkleHash) {
        let mut node = (1u64, hash);
        // Merge while the stack top is a subtree of the same size: it was
        // pushed earlier, so it is the LEFT sibling of the node we hold.
        loop {
            let Some(&(top_size, _)) = self.stack.last() else {
                break;
            };
            if top_size != node.0 {
                break;
            }
            let Some((_, left)) = self.stack.pop() else {
                break;
            };
            node = (node.0 * 2, node_hash(&left, &node.1));
        }
        self.stack.push(node);
    }

    /// The MTH of everything pushed. Empty ⇒ the dedicated empty hash.
    fn finalize(self) -> MerkleHash {
        let mut peaks = self.stack.into_iter().rev();
        match peaks.next() {
            None => empty_hash(),
            Some((_, mut acc)) => {
                // `acc` starts at the rightmost (smallest) peak; fold each
                // peak to its left in as the left sibling.
                for (_, left) in peaks {
                    acc = node_hash(&left, &acc);
                }
                acc
            }
        }
    }
}

/// Fold an ordered `(k,v)` stream into a root, capped at `budget` entries.
fn root_over<'a>(
    entries: Box<dyn Iterator<Item = Result<(Slice, Slice)>> + 'a>,
    budget: NonZeroU64,
) -> Result<MerkleHash> {
    let ceiling = budget.get();
    let mut spent: u64 = 0;
    let mut acc = MerkleAccumulator::new();
    for entry in entries {
        let (k, v) = entry?;
        spent += 1;
        if spent > ceiling {
            return Err(MerkleScanExceeded { ceiling }.into());
        }
        acc.push_leaf(leaf_hash(&k, &v));
    }
    Ok(acc.finalize())
}

/// The Merkle root over the **whole keyspace**, in canonical order. A pure
/// function of the committed `(k,v)` set (the determinism dividend): two
/// stores that hold the same content return the same root no matter how
/// they were written. For a validity-keyed relation this commits to *every*
/// stored version (retractions are stored keys, never physical deletes), so
/// the whole-store root is a bitemporal commitment.
pub(crate) fn state_root(tx: &impl ReadTx, budget: NonZeroU64) -> Result<MerkleHash> {
    root_over(tx.total_scan(), budget)
}

/// The Merkle root over the half-open byte range `[lower, upper)`, in
/// canonical order. A degenerate range (`lower >= upper`) is empty and
/// roots to [`empty_hash`], never an error — the same contract as
/// [`ReadTx::range_scan`].
pub(crate) fn range_root(
    tx: &impl ReadTx,
    lower: &[u8],
    upper: &[u8],
    budget: NonZeroU64,
) -> Result<MerkleHash> {
    root_over(tx.range_scan(lower, upper), budget)
}

/// The Merkle root over one relation's contiguous key range
/// `[relid_be, (relid+1)_be)`. Refuses (typed) an id outside the 48-bit
/// space rather than overflowing the prefix arithmetic.
pub(crate) fn relation_root(
    tx: &impl ReadTx,
    rel: RelationId,
    budget: NonZeroU64,
) -> Result<MerkleHash> {
    let lower = rel.raw().to_be_bytes();
    // `RelationId::new`/`raw_decode` refuse any id at or beyond
    // `RelationId::CAP` (1 << 48), so `raw()` is always below it and the
    // successor `+ 1` cannot overflow the encoded prefix.
    let upper = (rel.raw() + 1).to_be_bytes();
    range_root(tx, &lower, &upper, budget)
}

// ── Spec accountability spine (§56 / §57 / §58) ───────────────────────────

/// Plaintext-canonical state root digest (32 bytes). Cipher-invariant —
/// never computed over ciphertext (§59 / crypto ban).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StateRoot([u8; 32]);

impl StateRoot {
    /// Wrap an already-proven root digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lift a cold [`MerkleHash`] into Spec [`StateRoot`].
    pub(crate) fn from_merkle(hash: MerkleHash) -> Self {
        Self(hash.0)
    }
}

/// Typed link distinguishing ordinary commit, recovery advance, and fork.
///
/// Recovery advances seal a typed recovery link auditors can distinguish
/// from ordinary advance. Fork links bind the shared fork-point root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChainLinkKind {
    /// Ordinary Committed root mint.
    Ordinary,
    /// Same-principal recovery advance (auditor-distinguishable).
    Recovery,
    /// Different-principal fork discovery link.
    Fork,
}

impl ChainLinkKind {
    /// Wire discriminant for [`encode_chained_state_root`]: 1=Ordinary, 2=Recovery, 3=Fork.
    pub(crate) fn transcript_tag(self) -> u8 {
        match self {
            ChainLinkKind::Ordinary => 1,
            ChainLinkKind::Recovery => 2,
            ChainLinkKind::Fork => 3,
        }
    }
}

/// One per-commit chained state root — predecessor hash seals the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChainedStateRoot {
    store_id: StoreId,
    fence_epoch: FenceEpoch,
    commit_ordinal: CommitOrdinal,
    root: StateRoot,
    predecessor_root: StateRoot,
    link: ChainLinkKind,
}

impl ChainedStateRoot {
    /// Mint a chained root at the durable event (SweepDoor / epoch advance).
    ///
    /// `predecessor_root` is the prior commit’s root (or genesis digest for
    /// the first mint in a CryptoDomain). Roots are plaintext-canonical.
    pub(crate) fn mint(
        store_id: StoreId,
        fence_epoch: FenceEpoch,
        commit_ordinal: CommitOrdinal,
        content_root: StateRoot,
        predecessor_root: StateRoot,
        link: ChainLinkKind,
    ) -> Result<Self, MerkleChainRefuse> {
        let root = chain_bind(content_root, predecessor_root, link, commit_ordinal)?;
        Ok(Self {
            store_id,
            fence_epoch,
            commit_ordinal,
            root,
            predecessor_root,
            link,
        })
    }

    /// Store identity.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Fence epoch of this mint.
    pub fn fence_epoch(self) -> FenceEpoch {
        self.fence_epoch
    }

    /// Dense commit ordinal of this mint.
    pub fn commit_ordinal(self) -> CommitOrdinal {
        self.commit_ordinal
    }

    /// Chained root digest at this commit.
    pub fn root(self) -> StateRoot {
        self.root
    }

    /// Predecessor root this mint covers.
    pub fn predecessor_root(self) -> StateRoot {
        self.predecessor_root
    }

    /// Typed chain link kind.
    pub fn link(self) -> ChainLinkKind {
        self.link
    }
}

/// Genesis predecessor root for the first mint in a CryptoDomain.
pub const GENESIS_ROOT: StateRoot = StateRoot([0u8; 32]);

/// Fork-point root sealed in a ForkGrant — shared ancestry without revealing
/// post-fork content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForkPoint {
    /// Shared root at the fork cut.
    fork_point: StateRoot,
    /// Predecessor Store identity.
    predecessor_store: StoreId,
    /// Fence epoch / cut at the fork.
    fence_epoch: FenceEpoch,
    commit_ordinal: CommitOrdinal,
}

impl ForkPoint {
    /// Seal a fork-point from grant materialization inputs.
    pub(crate) fn seal(
        fork_point: StateRoot,
        predecessor_store: StoreId,
        fence_epoch: FenceEpoch,
        commit_ordinal: CommitOrdinal,
    ) -> Self {
        Self {
            fork_point,
            predecessor_store,
            fence_epoch,
            commit_ordinal,
        }
    }

    /// Shared fork-point root.
    pub fn fork_point(self) -> StateRoot {
        self.fork_point
    }

    /// Predecessor Store identity.
    pub fn predecessor_store(self) -> StoreId {
        self.predecessor_store
    }

    /// Fence epoch at the fork.
    pub fn fence_epoch(self) -> FenceEpoch {
        self.fence_epoch
    }

    /// Commit ordinal at the fork cut.
    pub fn commit_ordinal(self) -> CommitOrdinal {
        self.commit_ordinal
    }
}

/// Domain-separated composition digest of meaning tip × WAL tip.
///
/// Owned identity for [`DurableCommitCut::composed`] — never a bare `[u8; 32]`.
/// Bytes are produced only by [`compose_durable_cut`] under the
/// `kyzo.durable_commit_cut.v1` domain tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct DurableCutDigest([u8; 32]);

const _: () = assert!(std::mem::size_of::<DurableCutDigest>() == std::mem::size_of::<[u8; 32]>());
const _: () = assert!(std::mem::align_of::<DurableCutDigest>() == std::mem::align_of::<[u8; 32]>());

impl DurableCutDigest {
    /// Borrow the digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// One durable commit boundary: meaning-layer tip × WAL byte-chain tip.
///
/// Seat 24 (WAL hash chain) and seat 56 ([`RootChain`]) meet here — not as two
/// independent chains that never compose. [`compose`](DurableCommitCut::compose)
/// domain-separates both tips; breaking either bind breaks [`cuts_equal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DurableCommitCut {
    meaning_root: StateRoot,
    commit_ordinal: CommitOrdinal,
    /// WAL [`WalHash`] / `final_hash` tip after the Commit record at this cut.
    wal_final_hash: WalHash,
    /// Domain-separated composition of both tips (load-bearing bind).
    composed: DurableCutDigest,
}

impl DurableCommitCut {
    /// Compose meaning tip with WAL byte-chain tip at the SweepDoor durable seal.
    pub(crate) fn compose(meaning: &ChainedStateRoot, wal_final_hash: WalHash) -> Self {
        let meaning_root = meaning.root();
        let commit_ordinal = meaning.commit_ordinal();
        let composed = compose_durable_cut(meaning_root, wal_final_hash, commit_ordinal);
        Self {
            meaning_root,
            commit_ordinal,
            wal_final_hash,
            composed,
        }
    }

    /// Meaning-layer chained root at this cut.
    pub fn meaning_root(self) -> StateRoot {
        self.meaning_root
    }

    /// Dense commit ordinal at this cut.
    pub fn commit_ordinal(self) -> CommitOrdinal {
        self.commit_ordinal
    }

    /// WAL byte-chain tip (`final_hash`) at this cut.
    pub fn wal_final_hash(self) -> WalHash {
        self.wal_final_hash
    }

    /// Domain-separated composition digest of both tips.
    pub fn composed(self) -> DurableCutDigest {
        self.composed
    }
}

/// Composed cuts equal iff both tips and the composition digest agree.
pub fn cuts_equal(left: DurableCommitCut, right: DurableCommitCut) -> bool {
    left.composed() == right.composed()
        && left.meaning_root() == right.meaning_root()
        && left.wal_final_hash() == right.wal_final_hash()
        && left.commit_ordinal() == right.commit_ordinal()
}

/// Domain-separated bind of meaning tip + WAL tip at one commit ordinal.
fn compose_durable_cut(
    meaning_root: StateRoot,
    wal_final_hash: WalHash,
    commit_ordinal: CommitOrdinal,
) -> DurableCutDigest {
    let mut h = Sha256::new();
    h.update(b"kyzo.durable_commit_cut.v1");
    h.update(meaning_root.as_bytes());
    h.update(wal_final_hash.as_bytes());
    h.update(u64::to_be_bytes(commit_ordinal.get()));
    DurableCutDigest(h.finalize().into())
}

/// An ordered chain of chained roots — supports as-of lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootChain {
    links: Vec<ChainedStateRoot>,
}

impl RootChain {
    /// Empty chain (pre-first-commit).
    pub fn empty() -> Self {
        Self { links: Vec::new() }
    }

    /// Append a mint; predecessor must match the prior terminal root
    /// (or [`GENESIS_ROOT`] when empty).
    pub fn append(&mut self, link: ChainedStateRoot) -> Result<(), MerkleChainRefuse> {
        let expected = match self.links.last() {
            Some(l) => l.root(),
            None => GENESIS_ROOT,
        };
        if link.predecessor_root() != expected {
            return Err(MerkleChainRefuse::PredecessorMismatch);
        }
        self.links.push(link);
        Ok(())
    }

    /// All links in commit order.
    pub fn links(&self) -> &[ChainedStateRoot] {
        &self.links
    }

    /// Prior root the next mint must cover — tip of the chain, or
    /// [`GENESIS_ROOT`] when empty. Stored on the SweepDoor so the prior is
    /// not cold on-demand only.
    pub fn prior_root(&self) -> StateRoot {
        match self.links.last() {
            Some(l) => l.root(),
            None => GENESIS_ROOT,
        }
    }

    /// Contiguous segment for STH consistency proofs — first link may sit
    /// mid-lineage (predecessor need not be [`GENESIS_ROOT`]); every later
    /// link must cover the prior tip.
    pub(crate) fn from_contiguous_segment(
        links: &[ChainedStateRoot],
    ) -> Result<Self, MerkleChainRefuse> {
        let mut chain = Self::empty();
        for (i, link) in links.iter().enumerate() {
            if i == 0 {
                chain.links.push(*link);
            } else {
                chain.append(*link)?;
            }
        }
        Ok(chain)
    }
}

/// Chain link covering `cut` — the mint whose root [`as_of_root`] returns.
///
/// Independent recomputation (session `verify`) needs the link's predecessor
/// and mint metadata, not only the tip digest.
pub fn link_at_cut(
    chain: &RootChain,
    cut: CommitOrdinal,
) -> Result<&ChainedStateRoot, MerkleChainRefuse> {
    let mut best: Option<&ChainedStateRoot> = None;
    for link in chain.links() {
        if link.commit_ordinal().get() <= cut.get() {
            best = Some(link);
        } else {
            break;
        }
    }
    best.ok_or(MerkleChainRefuse::CutBeforeGenesis)
}

/// State root at any committed transaction time (as-of root).
///
/// Current-only root as the sole accountability digest is deleted —
/// as-of is a first-class surface (§57).
pub fn as_of_root(chain: &RootChain, cut: CommitOrdinal) -> Result<StateRoot, MerkleChainRefuse> {
    link_at_cut(chain, cut).map(|l| l.root())
}

/// Fork-equivalence: shared fork-point root without revealing post-fork content.
///
/// Equality at a cut = recomputed roots ([`replica_equivalence_at_cut`]).
/// Path/URL equivalence claims are refused ([`refuse_path_url_sameness`]) —
/// only independent root comparison / shared [`ForkPoint`] prove sameness.
pub fn fork_equivalence(a: &ForkPoint, b: &ForkPoint) -> bool {
    a.fork_point() == b.fork_point()
        && a.predecessor_store() == b.predecessor_store()
        && a.fence_epoch() == b.fence_epoch()
        && a.commit_ordinal() == b.commit_ordinal()
}

/// Store equality at a cut: identical recomputed roots.
pub fn roots_equal_at_cut(left: StateRoot, right: StateRoot) -> bool {
    left == right
}

/// One replica's local mint material for independent recomputation at a cut.
///
/// Carries only what *this* instance observed (content from its ordered facts,
/// predecessor from its local chain). A peer-delivered root is never a field —
/// federation fabric carriage of facts stays `[OPEN]`; this is the
/// single-transport engine protocol (seat 58).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReplicaCutRecompute {
    store_id: StoreId,
    fence_epoch: FenceEpoch,
    commit_ordinal: CommitOrdinal,
    /// Plaintext-canonical content root from this instance's ordered facts.
    content_root: StateRoot,
    /// Predecessor this instance's local chain covers.
    predecessor_root: StateRoot,
    link: ChainLinkKind,
}

impl ReplicaCutRecompute {
    /// Build from local observation only — never from a received root.
    pub(crate) fn from_local(
        store_id: StoreId,
        fence_epoch: FenceEpoch,
        commit_ordinal: CommitOrdinal,
        content_root: StateRoot,
        predecessor_root: StateRoot,
        link: ChainLinkKind,
    ) -> Self {
        Self {
            store_id,
            fence_epoch,
            commit_ordinal,
            content_root,
            predecessor_root,
            link,
        }
    }

    /// Independently recompute the chained root this side contributes.
    pub fn recompute(self) -> Result<StateRoot, MerkleChainRefuse> {
        Ok(ChainedStateRoot::mint(
            self.store_id,
            self.fence_epoch,
            self.commit_ordinal,
            self.content_root,
            self.predecessor_root,
            self.link,
        )?
        .root())
    }
}

/// Recompute-and-compare replica equivalence at a cut (seat 58).
///
/// Each side independently recomputes via [`ReplicaCutRecompute::recompute`];
/// comparison is [`roots_equal_at_cut`] over those digests. Neither argument
/// is a received/delivered root — a trust-the-peer costume cannot enter.
pub fn replica_equivalence_at_cut(
    left: ReplicaCutRecompute,
    right: ReplicaCutRecompute,
) -> Result<bool, MerkleChainRefuse> {
    Ok(roots_equal_at_cut(left.recompute()?, right.recompute()?))
}

/// Path/URL "same store" claim — location is not identity (seat 4) and not
/// equivalence (seat 58). Construction is allowed; using it as equivalence
/// is refused by [`refuse_path_url_sameness`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathUrlSamenessClaim {
    left_location: String,
    right_location: String,
}

impl PathUrlSamenessClaim {
    /// Record a path/URL pair that would costume location as sameness.
    pub fn claim(left_location: impl Into<String>, right_location: impl Into<String>) -> Self {
        Self {
            left_location: left_location.into(),
            right_location: right_location.into(),
        }
    }

    /// Left path/URL location.
    pub fn left_location(&self) -> &str {
        &self.left_location
    }

    /// Right path/URL location.
    pub fn right_location(&self) -> &str {
        &self.right_location
    }
}

/// Refuse a path/URL "same store" claim — never an equivalence proof.
pub fn refuse_path_url_sameness(_claim: PathUrlSamenessClaim) -> MerkleChainRefuse {
    MerkleChainRefuse::PathUrlSameness
}

/// Typed refusals on the chain / as-of / replica-equivalence / STH path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum MerkleChainRefuse {
    /// Append whose predecessor does not cover the chain terminal.
    #[error("merkle chain: predecessor root mismatch")]
    #[diagnostic(code(store::merkle::predecessor_mismatch))]
    PredecessorMismatch,
    /// as-of cut before any chained mint.
    #[error("merkle chain: as-of cut before genesis mint")]
    #[diagnostic(code(store::merkle::cut_before_genesis))]
    CutBeforeGenesis,
    /// Path/URL "same store" is not store equivalence (seat 58).
    #[error("merkle chain: path/URL sameness is not store equivalence")]
    #[diagnostic(code(store::merkle::path_url_sameness))]
    PathUrlSameness,
    /// Consistency proof does not bind the two state-root heads.
    #[error("merkle chain: consistency proof failed for state-root heads")]
    #[diagnostic(code(store::merkle::consistency_proof_failed))]
    ConsistencyProofFailed,
    /// Gossiped STHs for one Store disagree without a consistency extension
    /// — split-view / equivocation **Detected-on-gossip** (CT model).
    #[error("merkle chain: split-view detected on STH gossip before chains meet")]
    #[diagnostic(code(store::merkle::split_view_detected))]
    SplitViewDetected,
    /// Consistency proof required between unequal ordinals was absent.
    #[error("merkle chain: consistency proof required for STH gossip check")]
    #[diagnostic(code(store::merkle::consistency_proof_required))]
    ConsistencyProofRequired,
    /// Gossip pair names distinct Store identities — not a same-Store check.
    #[error("merkle chain: STH gossip pair spans distinct Store identities")]
    #[diagnostic(code(store::merkle::sth_store_mismatch))]
    SthStoreMismatch,
    /// Canonical transcript encode failed for a typed head/bind (invariant breach).
    #[error(transparent)]
    #[diagnostic(transparent)]
    Transcript(#[from] crate::store::transcript::TranscriptRefuse),
}

// ── STH gossip / consistency (CT non-equivocation; seats 2/56/58/69) ──────

/// Compact unsigned state-root head — the digest payload an STH carries.
///
/// Signed and carried on the NATS JetStream fabric by the replica gossip
/// obligation ([`crate::store::replica`]); this type is the Merkle meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StateRootHead {
    store_id: StoreId,
    fence_epoch: FenceEpoch,
    commit_ordinal: CommitOrdinal,
    root: StateRoot,
}

impl StateRootHead {
    /// Mint a head from a chain tip link.
    pub fn from_chain_tip(chain: &RootChain) -> Result<Self, MerkleChainRefuse> {
        let tip = chain
            .links()
            .last()
            .ok_or(MerkleChainRefuse::CutBeforeGenesis)?;
        Ok(Self {
            store_id: tip.store_id(),
            fence_epoch: tip.fence_epoch(),
            commit_ordinal: tip.commit_ordinal(),
            root: tip.root(),
        })
    }

    /// Mint a head at a committed cut (as-of).
    pub fn from_cut(chain: &RootChain, cut: CommitOrdinal) -> Result<Self, MerkleChainRefuse> {
        let link = link_at_cut(chain, cut)?;
        Ok(Self {
            store_id: link.store_id(),
            fence_epoch: link.fence_epoch(),
            commit_ordinal: link.commit_ordinal(),
            root: link.root(),
        })
    }

    /// Store identity this head commits.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Fence epoch of this head.
    pub fn fence_epoch(self) -> FenceEpoch {
        self.fence_epoch
    }

    /// Commit ordinal (tree size analogue) of this head.
    pub fn commit_ordinal(self) -> CommitOrdinal {
        self.commit_ordinal
    }

    /// Chained state root at this head.
    pub fn root(self) -> StateRoot {
        self.root
    }

    /// Compact signing / gossip body: SHA-256 of the ONE
    /// [`encode_state_root_head`] transcript bytes.
    ///
    /// Hand-rolled field hashing of the head is Unconstructible — the
    /// transcript is the sole serializer; this digests its sealed bytes only.
    pub fn compact_digest(self) -> Result<[u8; 32], MerkleChainRefuse> {
        let transcript = encode_state_root_head(
            self.store_id.as_bytes(),
            self.fence_epoch.get(),
            self.commit_ordinal.get(),
            self.root.as_bytes(),
        )?;
        let mut h = Sha256::new();
        h.update(transcript.as_bytes());
        Ok(h.finalize().into())
    }
}

/// Contiguous RootChain subsegment proving a newer head extends an older one.
///
/// Analogue of RFC 6962 consistency proofs over the Spec chained-root spine:
/// recomputing [`as_of_root`] at both cuts on the proof chain must reproduce
/// both heads. A split-view cannot forge a proof that binds divergent tips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsistencyProof {
    links: Vec<ChainedStateRoot>,
}

impl ConsistencyProof {
    /// Links in commit order covering the older-through-newer segment.
    pub fn links(&self) -> &[ChainedStateRoot] {
        &self.links
    }
}

/// Build a consistency proof from an honest [`RootChain`] between two cuts.
pub fn build_consistency_proof(
    chain: &RootChain,
    older: CommitOrdinal,
    newer: CommitOrdinal,
) -> Result<ConsistencyProof, MerkleChainRefuse> {
    if older.get() > newer.get() {
        return Err(MerkleChainRefuse::ConsistencyProofFailed);
    }
    // Cover every link with ordinal in [older, newer] so as-of at both cuts works.
    let links: Vec<ChainedStateRoot> = chain
        .links()
        .iter()
        .copied()
        .filter(|l| {
            l.commit_ordinal().get() >= older.get() && l.commit_ordinal().get() <= newer.get()
        })
        .collect();
    if links.is_empty() {
        return Err(MerkleChainRefuse::CutBeforeGenesis);
    }
    // First link must be exactly at `older` (or the segment cannot name it).
    if links[0].commit_ordinal() != older {
        return Err(MerkleChainRefuse::ConsistencyProofFailed);
    }
    if links.last().map(|l| l.commit_ordinal()) != Some(newer) {
        return Err(MerkleChainRefuse::ConsistencyProofFailed);
    }
    Ok(ConsistencyProof { links })
}

/// Verify that `newer` is a consistent extension of `older` under `proof`.
pub fn verify_consistency_proof(
    older: &StateRootHead,
    newer: &StateRootHead,
    proof: &ConsistencyProof,
) -> Result<(), MerkleChainRefuse> {
    if older.store_id() != newer.store_id() {
        return Err(MerkleChainRefuse::SthStoreMismatch);
    }
    if older.fence_epoch() != newer.fence_epoch() {
        return Err(MerkleChainRefuse::ConsistencyProofFailed);
    }
    if older.commit_ordinal().get() > newer.commit_ordinal().get() {
        return Err(MerkleChainRefuse::ConsistencyProofFailed);
    }
    if older.commit_ordinal() == newer.commit_ordinal() {
        return if older.root() == newer.root() {
            Ok(())
        } else {
            Err(MerkleChainRefuse::SplitViewDetected)
        };
    }

    // First link must match `older` exactly; segment may sit mid-lineage.
    let first = proof
        .links()
        .first()
        .ok_or(MerkleChainRefuse::ConsistencyProofFailed)?;
    if first.store_id() != older.store_id()
        || first.fence_epoch() != older.fence_epoch()
        || first.commit_ordinal() != older.commit_ordinal()
        || first.root() != older.root()
    {
        return Err(MerkleChainRefuse::ConsistencyProofFailed);
    }
    for link in proof.links() {
        if link.store_id() != older.store_id() || link.fence_epoch() != older.fence_epoch() {
            return Err(MerkleChainRefuse::ConsistencyProofFailed);
        }
    }

    let rebuilt = RootChain::from_contiguous_segment(proof.links())?;
    let as_older = as_of_root(&rebuilt, older.commit_ordinal())?;
    let as_newer = as_of_root(&rebuilt, newer.commit_ordinal())?;
    if as_older != older.root() || as_newer != newer.root() {
        return Err(MerkleChainRefuse::ConsistencyProofFailed);
    }
    let tip = rebuilt
        .links()
        .last()
        .ok_or(MerkleChainRefuse::ConsistencyProofFailed)?;
    if tip.commit_ordinal() != newer.commit_ordinal() || tip.root() != newer.root() {
        return Err(MerkleChainRefuse::ConsistencyProofFailed);
    }
    Ok(())
}

/// Outcome of a same-Store STH gossip consistency check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GossipConsistency {
    /// Both peers observed the identical head.
    Identical,
    /// Newer head extends older under a valid consistency proof.
    ConsistentExtension,
}

/// Cross-check two gossiped state-root heads (Certificate Transparency gossip).
///
/// Same ordinal + divergent roots → [`MerkleChainRefuse::SplitViewDetected`].
/// Unequal ordinals without a proof → [`MerkleChainRefuse::ConsistencyProofRequired`].
/// Unequal ordinals with a failing proof → [`MerkleChainRefuse::SplitViewDetected`].
/// Detection happens **before chains meet** — Detected-on-gossip, not
/// Unexposed-until-chains-meet.
pub fn check_sth_gossip(
    observed_a: &StateRootHead,
    observed_b: &StateRootHead,
    proof: Option<&ConsistencyProof>,
) -> Result<GossipConsistency, MerkleChainRefuse> {
    if observed_a.store_id() != observed_b.store_id() {
        return Err(MerkleChainRefuse::SthStoreMismatch);
    }
    let (older, newer) = if observed_a.commit_ordinal().get() <= observed_b.commit_ordinal().get() {
        (observed_a, observed_b)
    } else {
        (observed_b, observed_a)
    };

    if older.commit_ordinal() == newer.commit_ordinal() {
        if older.fence_epoch() == newer.fence_epoch() && older.root() == newer.root() {
            return Ok(GossipConsistency::Identical);
        }
        return Err(MerkleChainRefuse::SplitViewDetected);
    }

    let Some(proof) = proof else {
        return Err(MerkleChainRefuse::ConsistencyProofRequired);
    };
    match verify_consistency_proof(older, newer, proof) {
        Ok(()) => Ok(GossipConsistency::ConsistentExtension),
        Err(MerkleChainRefuse::SplitViewDetected) => Err(MerkleChainRefuse::SplitViewDetected),
        Err(MerkleChainRefuse::ConsistencyProofFailed)
        | Err(MerkleChainRefuse::PredecessorMismatch)
        | Err(MerkleChainRefuse::CutBeforeGenesis) => Err(MerkleChainRefuse::SplitViewDetected),
        Err(other) => Err(other),
    }
}

/// Chained root bind: SHA-256 of the ONE [`encode_chained_state_root`]
/// transcript bytes (content ‖ predecessor ‖ link ‖ ordinal).
///
/// Hand-rolled `kyzo.chained_state_root.v1` field hashing is Unconstructible —
/// the transcript is the sole serializer; this digests its sealed bytes only.
fn chain_bind(
    content_root: StateRoot,
    predecessor_root: StateRoot,
    link: ChainLinkKind,
    commit_ordinal: CommitOrdinal,
) -> Result<StateRoot, MerkleChainRefuse> {
    let transcript = encode_chained_state_root(
        content_root.as_bytes(),
        predecessor_root.as_bytes(),
        link.transcript_tag(),
        commit_ordinal.get(),
    )?;
    let mut h = Sha256::new();
    h.update(transcript.as_bytes());
    Ok(StateRoot(h.finalize().into()))
}

#[cfg(test)]
mod tests {
    //! The root's guarantees, proven — not asserted:
    //!
    //! - **golden vectors** pin the exact SHA-256 bytes of the empty root, a
    //!   one-leaf root, and a two-leaf root (each recomputed here by hand
    //!   from the domain-separated formula), so a changed domain tag or a
    //!   changed leaf encoding is caught byte-for-byte;
    //! - the streaming accumulator is cross-checked against an **independent
    //!   recursive MTH** over shuffled inputs of every small size;
    //! - the **load-bearing** storage test: two real fjall stores with
    //!   identical content but different write histories yield the SAME
    //!   root, and any single-byte edit yields a DIFFERENT one;
    //! - **cipher-invariance** (§59): identical root for the same logical
    //!   store observed encrypted vs plaintext — the leaf commits to
    //!   canonical plaintext outside AEAD; ciphertext bytes are not the
    //!   leaf input;
    //! - **valid-but-stale rollback** (§56–§58): an older internally-consistent
    //!   snapshot fails [`roots_equal_at_cut`] against the stored prior tip;
    //! - **historical_edit** of a past [`ChainedStateRoot`]: every forward
    //!   chained root breaks — [`chain_bind`] covers the predecessor (proven);
    //! - **point-in-time** [`as_of_root`] (§57): at a real past cut the returned
    //!   root equals that cut's [`StateRoot`] and differs from tip / later cuts;
    //! - **replica-equivalence** (§58): two instances replaying the same ordered
    //!   facts match by independent recompute-and-compare; a delivered root is
    //!   not the comparison basis; path/URL "same store" is refused;
    //! - **STH gossip non-equivocation** (§2/56/58/69): an equivocating Store
    //!   showing divergent histories to two peers is
    //!   [`MerkleChainRefuse::SplitViewDetected`] via [`check_sth_gossip`]
    //!   before chains meet; honest extension verifies under
    //!   [`ConsistencyProof`];
    //! - determinism across store reopen;
    //! - the typed refusals (scan ceiling, out-of-range relation id).

    use miette::{IntoDiagnostic, Result, miette};
    use std::num::NonZeroU64;

    use fjall::Slice;

    use super::{
        MerkleHash, StateRoot, empty_hash, leaf_hash, node_hash, relation_root, root_over,
        state_root,
    };
    use crate::store::fjall::new_fjall_storage;
    use crate::store::{Storage, WriteTx};
    use kyzo_model::value::RelationId;

    fn big_budget() -> Result<NonZeroU64> {
        NonZeroU64::new(1_000_000).ok_or_else(|| miette!("nonzero budget"))
    }

    // ── golden vectors: LITERAL SHA-256 digests, computed off-tree from the
    // domain-separated formula (see the module docs), pinned as hex so a
    // changed domain tag or leaf encoding is caught byte-for-byte. These do
    // NOT reference the source `*_TAG` constants — a mutation to a tag must
    // move the digest away from the literal, and a golden that reused the
    // const would move with it and hide the mutation.

    #[test]
    fn empty_root_is_pinned() -> Result<()> {
        // SHA-256(0x02)
        assert_eq!(
            empty_hash().to_hex(),
            "dbc1b4c900ffe48d575b5da5c638040125f65db0fe3e24494b76ea986457d986"
        );

        Ok(())
    }

    #[test]
    fn single_leaf_root_is_pinned() -> Result<()> {
        // leaf = SHA-256(0x00 ‖ u64_be(3) ‖ "key" ‖ "val")
        let (k, v) = (b"key".as_slice(), b"val".as_slice());
        let golden = "f26135a572169d94e1cd659dc6e6ba89caddd4d1b0acc6fa87b3b9fed4045bc0";
        assert_eq!(leaf_hash(k, v).to_hex(), golden);
        // A one-entry root is exactly that leaf (no interior node).
        let root = root_over(
            Box::new(std::iter::once(Ok((Slice::from(k), Slice::from(v))))),
            big_budget()?,
        )?;
        assert_eq!(root.to_hex(), golden);

        Ok(())
    }

    #[test]
    fn two_leaf_root_is_pinned() -> Result<()> {
        // l0 = leaf("a","1"), l1 = leaf("b","2"), root = SHA-256(0x01 ‖ l0 ‖ l1)
        let (k0, v0) = (b"a".as_slice(), b"1".as_slice());
        let (k1, v1) = (b"b".as_slice(), b"2".as_slice());
        assert_eq!(
            leaf_hash(k0, v0).to_hex(),
            "cac24e82a1f10b6010ebb27c201f0bfe9278faf45d7bd0c1a3e71a45ccfd6113"
        );
        assert_eq!(
            leaf_hash(k1, v1).to_hex(),
            "ce5ed247914ea4eba3153ae6170651c5ac6b931ff064544c42050757d29eebb7"
        );
        let golden = "e116928b471f8efb9cdf905d2ddf8ca2c835c1f6978a4b7f100c0a241347eb94";
        assert_eq!(
            node_hash(&leaf_hash(k0, v0), &leaf_hash(k1, v1)).to_hex(),
            golden
        );
        let root = root_over(
            Box::new(
                [
                    (Slice::from(k0), Slice::from(v0)),
                    (Slice::from(k1), Slice::from(v1)),
                ]
                .into_iter()
                .map(Ok),
            ),
            big_budget()?,
        )?;
        assert_eq!(root.to_hex(), golden);

        Ok(())
    }

    #[test]
    fn key_length_prefix_removes_boundary_ambiguity() -> Result<()> {
        // (key=ab, value=c) and (key=a, value=bc) share the byte stream
        // "abc" but must not collide, because the key length is prefixed.
        assert_ne!(leaf_hash(b"ab", b"c"), leaf_hash(b"a", b"bc"));

        Ok(())
    }

    // ── the streaming accumulator equals an independent recursive MTH ─────

    /// RFC-6962 MTH, recursively, over a materialised leaf-hash slice — a
    /// second implementation that shares no code with the streaming one.
    fn recursive_mth(leaves: &[MerkleHash]) -> MerkleHash {
        match leaves.len() {
            0 => empty_hash(),
            1 => leaves[0],
            n => {
                // largest power of two strictly less than n
                let mut k = 1usize;
                while k * 2 < n {
                    k *= 2;
                }
                node_hash(&recursive_mth(&leaves[..k]), &recursive_mth(&leaves[k..]))
            }
        }
    }

    fn root_of_pairs(pairs: &[(Vec<u8>, Vec<u8>)]) -> Result<MerkleHash> {
        root_over(
            Box::new(
                pairs
                    .iter()
                    .map(|(k, v)| Ok((Slice::from(k), Slice::from(v)))),
            ),
            big_budget()?,
        )
    }

    #[test]
    fn streaming_matches_recursive_mth_for_every_small_size() -> Result<()> {
        for n in 0u64..40 {
            let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..n)
                .map(|i| {
                    (
                        format!("k{i:04}").into_bytes(),
                        format!("v{i}").into_bytes(),
                    )
                })
                .collect();
            let leaves: Vec<MerkleHash> = pairs.iter().map(|(k, v)| leaf_hash(k, v)).collect();
            assert_eq!(
                root_of_pairs(&pairs)?,
                recursive_mth(&leaves),
                "streaming ≠ recursive MTH at n={n}"
            );
        }

        Ok(())
    }

    // ── cipher-invariance (§59): leaf = canonical plaintext, never AEAD ──

    /// Standing cipher-invariance: identical root for the same logical store
    /// observed encrypted vs plaintext. The built leaf commits to canonical
    /// plaintext outside AEAD — decrypt-before-merkle feeds the same bytes
    /// into [`leaf_hash`] whether carriage was ciphertext or plaintext.
    /// Rooting over ciphertext body would be a different commitment (banned).
    #[test]
    fn cipher_invariant_root_identical_encrypted_vs_plaintext() -> Result<()> {
        use crate::store::contract::FormatVersion;
        use crate::store::epoch::{CryptoDomain, FenceEpoch};
        use crate::store::open::StoreId;
        use crate::store::transcript::{CanonicalTranscriptBuilder, FieldId, SealedArtifactKind};
        use crate::store::{
            AeadArm, Kek, KekUnwrapCap, Nonce, SegmentCounter, ShredSalt, compress_then_encrypt,
            decompress, decrypt, derive_dek,
        };

        let store = StoreId::from_digest([0xC1; 32]);
        let domain = CryptoDomain::new(store, FenceEpoch::genesis(store));
        let kek = Kek::from_bytes([0x59; 32]);
        let cap = KekUnwrapCap::from_kek(kek);
        let salt = ShredSalt::from_bytes([0xA5; 32]);
        let dek = derive_dek(&cap, domain, SegmentCounter::ZERO, &salt);

        let mut aad_b = CanonicalTranscriptBuilder::new(FormatVersion::CURRENT)?;
        aad_b.append_u64(
            FieldId::ARTIFACT_KIND,
            SealedArtifactKind::AuditKeyLeaf.tag(),
        )?;
        let aad = aad_b.seal();

        let logical: Vec<(Vec<u8>, Vec<u8>)> = (0..8u32)
            .map(|i| {
                (
                    format!("k{i:03}").into_bytes(),
                    format!("canonical-plaintext-{i}").into_bytes(),
                )
            })
            .collect();

        // Plaintext observation: leaf over canonical plaintext.
        let plaintext_root = root_of_pairs(&logical)?;

        // Encrypted carriage: seal each value; open before the leaf (the
        // only lawful merkle input). Ciphertext body must not be the leaf.
        let mut opened_pairs = Vec::with_capacity(logical.len());
        for (i, (k, plaintext)) in logical.iter().enumerate() {
            let mut nonce_bytes = [0u8; 12];
            nonce_bytes[..4].copy_from_slice(&match u32::try_from(i) {
                Ok(n) => n,
                Err(_) => u32::MAX,
            }.to_be_bytes());
            let nonce = Nonce::from_bytes(nonce_bytes);
            let ct = compress_then_encrypt(plaintext, &dek, nonce, AeadArm::Siv, &aad)?;
            assert_ne!(
                ct.body(),
                plaintext.as_slice(),
                "AEAD ciphertext must not equal plaintext"
            );
            assert_ne!(
                leaf_hash(k, plaintext),
                leaf_hash(k, ct.body()),
                "leaf over ciphertext ≠ leaf over plaintext — cipher-invariance requires plaintext"
            );
            let opened = decrypt(&ct, &dek, &aad)?;
            let round = decompress(&opened)?;
            assert_eq!(&round, plaintext);
            opened_pairs.push((k.clone(), round));
        }
        let encrypted_obs_root = root_of_pairs(&opened_pairs)?;

        assert_eq!(
            plaintext_root, encrypted_obs_root,
            "encrypted vs plaintext observation: identical root — leaf is plaintext-canonical"
        );
        assert_eq!(
            StateRoot::from_merkle(plaintext_root),
            StateRoot::from_merkle(encrypted_obs_root),
            "StateRoot lift preserves cipher-invariant digest"
        );

        // Live store scan over the same plaintext content equals both.
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let mut tx = db.write_tx()?;
        for (k, v) in &logical {
            tx.put(k, v)?;
        }
        tx.commit()?;
        let tx = db.read_tx()?;
        let store_root = state_root(&tx, big_budget()?)?;
        assert_eq!(store_root, plaintext_root);
        assert_eq!(
            StateRoot::from_merkle(store_root),
            StateRoot::from_merkle(encrypted_obs_root),
            "encrypted vs plaintext identical StateRoot via state_root"
        );

        Ok(())
    }

    // ── valid-but-stale rollback (§56–§58): stored prior tip catches swap ──

    /// An older internally-consistent backup still has a well-formed content
    /// root (cold merkle of that snapshot verifies). Detection is root
    /// comparison against the stored prior tip on [`RootChain`] — without a
    /// stored prior, cold rescan of the restored bytes alone cannot notice
    /// the rollback. Swap state-at-cut-1 under a tip advanced to cut-3 →
    /// [`roots_equal_at_cut`] is false.
    #[test]
    fn valid_but_stale_rollback_detected_against_stored_prior() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::open::StoreId;
        use crate::store::sweep::CommitOrdinal;

        use super::{
            ChainLinkKind, ChainedStateRoot, GENESIS_ROOT, RootChain, as_of_root,
            roots_equal_at_cut,
        };

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

        let content_v1 = StateRoot::from_merkle(root_of_pairs(&state_v1)?);
        let content_v2 = StateRoot::from_merkle(root_of_pairs(&state_v2)?);
        let content_v3 = StateRoot::from_merkle(root_of_pairs(&state_v3)?);
        assert_ne!(content_v1, content_v2);
        assert_ne!(content_v2, content_v3);

        let o1 = CommitOrdinal::ZERO.successor()?;
        let o2 = o1.successor()?;
        let o3 = o2.successor()?;

        let mut chain = RootChain::empty();
        assert_eq!(chain.prior_root(), GENESIS_ROOT);

        let link1 = ChainedStateRoot::mint(
            store_id,
            fence,
            o1,
            content_v1,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        );
        chain.append(link1)?;
        let root_at_v1 = as_of_root(&chain, o1)?;

        let link2 = ChainedStateRoot::mint(
            store_id,
            fence,
            o2,
            content_v2,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        );
        chain.append(link2)?;

        let link3 = ChainedStateRoot::mint(
            store_id,
            fence,
            o3,
            content_v3,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        );
        chain.append(link3)?;

        // T1 stores the prior tip on the SweepDoor — live tip after commit 3.
        let stored_prior = chain.prior_root();
        let tip_as_of = as_of_root(&chain, o3)?;
        assert!(
            roots_equal_at_cut(stored_prior, tip_as_of),
            "live tip prior equals as-of root at tip cut"
        );

        // Attacker swaps an older internally-consistent state (v1 snapshot).
        // Recompute: content root of the restored bytes is well-formed.
        let restored_content = StateRoot::from_merkle(root_of_pairs(&state_v1)?);
        assert_eq!(
            restored_content, content_v1,
            "older state remains internally consistent"
        );
        let stale_as_of = as_of_root(&chain, o1)?;
        assert_eq!(stale_as_of, root_at_v1);

        // Detection: stored tip prior vs older cut — mismatch (§58).
        assert!(
            !roots_equal_at_cut(stored_prior, stale_as_of),
            "valid-but-stale rollback: stored tip prior ≠ as-of root of older cut"
        );

        // Swap older content under the tip ordinal with the lawful predecessor:
        // still ≠ stored tip (content changed; chain bind notices).
        let pred_at_o2 = as_of_root(&chain, o2)?;
        let forged_at_tip = ChainedStateRoot::mint(
            store_id,
            fence,
            o3,
            content_v1,
            pred_at_o2,
            ChainLinkKind::Ordinary,
        );
        assert!(
            !roots_equal_at_cut(stored_prior, forged_at_tip.root()),
            "valid-but-stale rollback: older content rebound at tip ≠ stored prior"
        );

        // Controls: same cut equals itself; tip equals tip.
        assert!(roots_equal_at_cut(stale_as_of, root_at_v1));
        assert!(roots_equal_at_cut(stored_prior, chain.prior_root()));

        Ok(())
    }

    /// Disk rollback: real fjall directory backed up at v1, advanced to v3,
    /// directory restored from the v1 backup, reopened. Recomputed content
    /// root equals the v1 root and differs from the live tip content; binding
    /// the restored content under the tip cut ≠ the stored prior tip.
    #[test]
    fn valid_but_stale_rollback_on_disk_dir_restore() -> Result<()> {
        use std::path::{Path, PathBuf};

        use crate::store::epoch::FenceEpoch;
        use crate::store::open::StoreId;
        use crate::store::sweep::CommitOrdinal;

        use super::{
            ChainLinkKind, ChainedStateRoot, GENESIS_ROOT, RootChain, as_of_root, link_at_cut,
            roots_equal_at_cut,
        };

        fn copy_dir_recursive(src: &Path, dst: &Path) {
            std::fs::create_dir_all(dst)?;
            for entry in std::fs::read_dir(src)? {
                let entry = entry?;
                let ty = entry.file_type()?;
                let from = entry.path();
                let to = dst.join(entry.file_name());
                if ty.is_dir() {
                    copy_dir_recursive(&from, &to);
                } else {
                    std::fs::copy(&from, &to)?;
                }
            }
        }

        fn replace_dir_with(src: &Path, dst: &Path) {
            if dst.exists() {
                std::fs::remove_dir_all(dst)?;
            }
            copy_dir_recursive(src, dst);
        }

        let store_id = StoreId::from_digest([0x5D; 32]);
        let fence = FenceEpoch::genesis(store_id);
        let o1 = CommitOrdinal::ZERO.successor()?;
        let o2 = o1.successor()?;
        let o3 = o2.successor()?;

        let root = tempfile::tempdir().into_diagnostic()?;
        let live: PathBuf = root.path().join("live");
        let backup_v1: PathBuf = root.path().join("backup_v1");
        std::fs::create_dir_all(&live)?;

        // Cut 1: write v1, seal content root, back up the on-disk directory.
        let content_v1 = {
            let db = new_fjall_storage(&live)?;
            let mut tx = db.write_tx()?;
            tx.put(b"k00", b"v0")?;
            tx.put(b"k01", b"v1")?;
            tx.commit()?;
            let content = StateRoot::from_merkle(state_root(&db.read_tx()?, big_budget()?)?);
            drop(db);
            content
        };
        copy_dir_recursive(&live, &backup_v1);

        // Advance live store to v3; mint RootChain under the advanced tip.
        let (content_v3, chain) = {
            let db = new_fjall_storage(&live)?;
            {
                let mut tx = db.write_tx()?;
                tx.put(b"k02", b"v2")?;
                tx.commit()?;
            }
            let content_v2 = StateRoot::from_merkle(state_root(&db.read_tx()?, big_budget()?)?);
            {
                let mut tx = db.write_tx()?;
                tx.put(b"k03", b"v3")?;
                tx.commit()?;
            }
            let content_v3 = StateRoot::from_merkle(state_root(&db.read_tx()?, big_budget()?)?);
            assert_ne!(content_v1, content_v2);
            assert_ne!(content_v2, content_v3);

            let mut chain = RootChain::empty();
            chain.append(ChainedStateRoot::mint(
                store_id,
                fence,
                o1,
                content_v1,
                GENESIS_ROOT,
                ChainLinkKind::Ordinary,
            ))?;
            chain.append(ChainedStateRoot::mint(
                store_id,
                fence,
                o2,
                content_v2,
                chain.prior_root(),
                ChainLinkKind::Ordinary,
            ))?;
            chain.append(ChainedStateRoot::mint(
                store_id,
                fence,
                o3,
                content_v3,
                chain.prior_root(),
                ChainLinkKind::Ordinary,
            ))?;
            drop(db);
            (content_v3, chain)
        };

        let tip_prior = chain.prior_root();
        let tip_as_of = as_of_root(&chain, o3)?;
        assert!(roots_equal_at_cut(tip_prior, tip_as_of));

        // Attacker restores the v1 directory under the advanced tip.
        replace_dir_with(&backup_v1, &live);
        let restored_content = {
            let db = new_fjall_storage(&live)?;
            StateRoot::from_merkle(state_root(&db.read_tx()?, big_budget()?)?)
        };

        assert_eq!(
            restored_content, content_v1,
            "dir-restored store must recompute to the v1 content root"
        );
        assert_ne!(
            restored_content, content_v3,
            "restored v1 content root must differ from the live tip content"
        );

        // Detection door (same bind as session verify): restored content under
        // the tip cut's predecessor ≠ stored tip prior.
        let tip_link = link_at_cut(&chain, o3)?;
        let recomputed_at_tip = ChainedStateRoot::mint(
            tip_link.store_id(),
            tip_link.fence_epoch(),
            tip_link.commit_ordinal(),
            restored_content,
            tip_link.predecessor_root(),
            tip_link.link(),
        )
        .root();
        assert!(
            !roots_equal_at_cut(tip_prior, recomputed_at_tip),
            "valid-but-stale on-disk rollback: stored tip prior ≠ tip rebound over restored bytes"
        );

        Ok(())
    }

    /// Adversarial historical_edit: mutate a past commit's content (and thus its
    /// [`ChainedStateRoot`]), then remint every later link with the *same*
    /// forward content roots. If [`chain_bind`] omitted the predecessor, those
    /// forward digests would still match the honest chain — the equality fails
    /// prove coverage. Also: an honest forward link refuses to append after the
    /// edited past tip ([`MerkleChainRefuse::PredecessorMismatch`]).
    #[test]
    fn historical_edit_breaks_every_forward_chained_root() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::open::StoreId;
        use crate::store::sweep::CommitOrdinal;

        use super::{
            ChainLinkKind, ChainedStateRoot, GENESIS_ROOT, MerkleChainRefuse, RootChain,
            as_of_root, roots_equal_at_cut,
        };

        let store_id = StoreId::from_digest([0xED; 32]);
        let fence = FenceEpoch::genesis(store_id);

        let state_v1: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"k00".to_vec(), b"v0".to_vec()),
            (b"k01".to_vec(), b"v1".to_vec()),
        ];
        // Edit past commit bytes — one value byte differs at the historical cut.
        let state_v1_edited: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"k00".to_vec(), b"v0".to_vec()),
            (b"k01".to_vec(), b"vX".to_vec()),
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

        let content_v1 = StateRoot::from_merkle(root_of_pairs(&state_v1)?);
        let content_v1_edited = StateRoot::from_merkle(root_of_pairs(&state_v1_edited)?);
        let content_v2 = StateRoot::from_merkle(root_of_pairs(&state_v2)?);
        let content_v3 = StateRoot::from_merkle(root_of_pairs(&state_v3)?);
        assert_ne!(
            content_v1, content_v1_edited,
            "historical edit must change past content root"
        );

        let o1 = CommitOrdinal::ZERO.successor()?;
        let o2 = o1.successor()?;
        let o3 = o2.successor()?;

        let mut honest = RootChain::empty();
        let h1 = ChainedStateRoot::mint(
            store_id,
            fence,
            o1,
            content_v1,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        );
        honest.append(h1)?;
        let h2 = ChainedStateRoot::mint(
            store_id,
            fence,
            o2,
            content_v2,
            honest.prior_root(),
            ChainLinkKind::Ordinary,
        );
        honest.append(h2)?;
        let h3 = ChainedStateRoot::mint(
            store_id,
            fence,
            o3,
            content_v3,
            honest.prior_root(),
            ChainLinkKind::Ordinary,
        );
        honest.append(h3)?;

        let honest_r1 = as_of_root(&honest, o1)?;
        let honest_r2 = as_of_root(&honest, o2)?;
        let honest_r3 = as_of_root(&honest, o3)?;

        // historical_edit past commit → rebind every forward link (same content).
        let mut edited = RootChain::empty();
        let e1 = ChainedStateRoot::mint(
            store_id,
            fence,
            o1,
            content_v1_edited,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        );
        edited.append(e1)?;
        assert!(
            !roots_equal_at_cut(honest_r1, as_of_root(&edited, o1)?),
            "historical_edit changes the past ChainedStateRoot"
        );

        let e2 = ChainedStateRoot::mint(
            store_id,
            fence,
            o2,
            content_v2,
            edited.prior_root(),
            ChainLinkKind::Ordinary,
        );
        edited.append(e2)?;
        let e3 = ChainedStateRoot::mint(
            store_id,
            fence,
            o3,
            content_v3,
            edited.prior_root(),
            ChainLinkKind::Ordinary,
        );
        edited.append(e3)?;

        // Every forward root breaks — fails if chain_bind drops the predecessor.
        assert!(
            !roots_equal_at_cut(honest_r2, as_of_root(&edited, o2)?),
            "historical_edit: forward chained root at o2 must break"
        );
        assert!(
            !roots_equal_at_cut(honest_r3, as_of_root(&edited, o3)?),
            "historical_edit: forward chained root at o3 must break"
        );

        // Chain verification: honest forward link cannot cover the edited past tip.
        let mut verify = RootChain::empty();
        verify.append(e1)?;
        assert_eq!(
            verify.append(h2),
            Err(MerkleChainRefuse::PredecessorMismatch),
            "historical_edit: honest forward link refuses edited past tip"
        );

        Ok(())
    }

    /// Point-in-time (§57): [`as_of_root`] is the who-believed-what-when anchor
    /// at a *real past cut* — not a tip-only digest. Multi-commit chain; ask at
    /// an earlier [`CommitOrdinal`]; equals that cut's [`StateRoot`]; differs
    /// from tip and from a later cut.
    #[test]
    fn as_of_root_point_in_time_returns_past_cut_anchor() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::open::StoreId;
        use crate::store::sweep::CommitOrdinal;

        use super::{
            ChainLinkKind, ChainedStateRoot, GENESIS_ROOT, RootChain, as_of_root,
            roots_equal_at_cut,
        };

        let store_id = StoreId::from_digest([0x57; 32]);
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

        let content_v1 = StateRoot::from_merkle(root_of_pairs(&state_v1)?);
        let content_v2 = StateRoot::from_merkle(root_of_pairs(&state_v2)?);
        let content_v3 = StateRoot::from_merkle(root_of_pairs(&state_v3)?);
        assert_ne!(content_v1, content_v2);
        assert_ne!(content_v2, content_v3);

        let o1 = CommitOrdinal::ZERO.successor()?;
        let o2 = o1.successor()?;
        let o3 = o2.successor()?;

        let mut chain = RootChain::empty();
        let link1 = ChainedStateRoot::mint(
            store_id,
            fence,
            o1,
            content_v1,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        );
        let cut_root_o1 = link1.root();
        chain.append(link1)?;

        let link2 = ChainedStateRoot::mint(
            store_id,
            fence,
            o2,
            content_v2,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        );
        let cut_root_o2 = link2.root();
        chain.append(link2)?;

        let link3 = ChainedStateRoot::mint(
            store_id,
            fence,
            o3,
            content_v3,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        );
        let tip_root = link3.root();
        chain.append(link3)?;

        // Past cut: as_of_root equals the StateRoot minted at that cut.
        let as_of_o1 = as_of_root(&chain, o1)?;
        assert!(
            roots_equal_at_cut(as_of_o1, cut_root_o1),
            "point-in-time: as_of_root at o1 equals that cut's StateRoot"
        );

        // Mid cut likewise.
        let as_of_o2 = as_of_root(&chain, o2)?;
        assert!(
            roots_equal_at_cut(as_of_o2, cut_root_o2),
            "point-in-time: as_of_root at o2 equals that cut's StateRoot"
        );

        // Past ≠ tip; past ≠ later cut — who-believed-what-when is cut-scoped.
        let tip_as_of = as_of_root(&chain, o3)?;
        assert!(
            roots_equal_at_cut(tip_as_of, tip_root),
            "point-in-time: as_of_root at tip equals tip StateRoot"
        );
        assert!(
            !roots_equal_at_cut(as_of_o1, tip_as_of),
            "point-in-time: past cut as_of_root differs from tip"
        );
        assert!(
            !roots_equal_at_cut(as_of_o1, as_of_o2),
            "point-in-time: earlier cut differs from a later cut"
        );
        assert!(
            !roots_equal_at_cut(as_of_o2, tip_as_of),
            "point-in-time: mid cut differs from tip"
        );

        Ok(())
    }

    /// Empty [`RootChain`]: [`as_of_root`] refuses — no cut exists before genesis.
    #[test]
    fn as_of_root_refuses_empty_chain() -> Result<()> {
        use crate::store::sweep::CommitOrdinal;

        use super::{MerkleChainRefuse, RootChain, as_of_root};

        let chain = RootChain::empty();
        let cut = CommitOrdinal::ZERO.successor()?;
        assert_eq!(
            as_of_root(&chain, cut),
            Err(MerkleChainRefuse::CutBeforeGenesis)
        );

        Ok(())
    }

    /// [`build_consistency_proof`] refuses inverted ordinal range and a gap
    /// where the chain has no link exactly at `older`.
    #[test]
    fn build_consistency_proof_refuses_inverted_range_and_ordinal_gap() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::open::StoreId;
        use crate::store::sweep::CommitOrdinal;

        use super::{
            ChainLinkKind, ChainedStateRoot, GENESIS_ROOT, MerkleChainRefuse, RootChain,
            build_consistency_proof,
        };

        let store_id = StoreId::from_digest([0xCF; 32]);
        let fence = FenceEpoch::genesis(store_id);
        let o1 = CommitOrdinal::ZERO.successor()?;
        let o2 = o1.successor()?;
        let o3 = o2.successor()?;

        let mut chain = RootChain::empty();
        let c1 = StateRoot::from_merkle(root_of_pairs(&[(b"a".to_vec(), b"1".to_vec())])?);
        let c3 = StateRoot::from_merkle(root_of_pairs(&[
            (b"a".to_vec(), b"1".to_vec()),
            (b"c".to_vec(), b"3".to_vec()),
        ])?);
        chain.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o1,
            c1,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        ))?;
        // Skip o2 deliberately — gap between o1 and o3.
        chain.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o3,
            c3,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        ))?;

        assert_eq!(
            build_consistency_proof(&chain, o3, o1),
            Err(MerkleChainRefuse::ConsistencyProofFailed),
            "inverted range (older > newer) must refuse"
        );
        assert_eq!(
            build_consistency_proof(&chain, o2, o3),
            Err(MerkleChainRefuse::ConsistencyProofFailed),
            "ordinal gap: no link at older cut must refuse"
        );
        // Empty chain: no covering links.
        assert_eq!(
            build_consistency_proof(&RootChain::empty(), o1, o3),
            Err(MerkleChainRefuse::CutBeforeGenesis)
        );

        Ok(())
    }

    /// [`fork_equivalence`] is true only when fork-point root, predecessor
    /// store, fence, and commit ordinal all agree — not path/URL sameness.
    #[test]
    fn fork_equivalence_true_only_on_identical_fork_points() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::open::StoreId;
        use crate::store::sweep::CommitOrdinal;

        use super::{ForkPoint, fork_equivalence};

        let store_a = StoreId::from_digest([0xF1; 32]);
        let store_b = StoreId::from_digest([0xF2; 32]);
        let fence_a = FenceEpoch::genesis(store_a);
        let fence_a_later = FenceEpoch::from_raw(store_a, 1);
        let o1 = CommitOrdinal::ZERO.successor()?;
        let o2 = o1.successor()?;
        let root = StateRoot::from_merkle(root_of_pairs(&[(b"k".to_vec(), b"v".to_vec())])?);
        let other_root =
            StateRoot::from_merkle(root_of_pairs(&[(b"k".to_vec(), b"OTHER".to_vec())])?);

        let a = ForkPoint::seal(root, store_a, fence_a, o1);
        let same = ForkPoint::seal(root, store_a, fence_a, o1);
        assert!(fork_equivalence(&a, &same));

        assert!(
            !fork_equivalence(&a, &ForkPoint::seal(other_root, store_a, fence_a, o1)),
            "divergent fork-point root must not equate"
        );
        assert!(
            !fork_equivalence(&a, &ForkPoint::seal(root, store_b, fence_a, o1)),
            "foreign predecessor StoreId must not equate"
        );
        assert!(
            !fork_equivalence(&a, &ForkPoint::seal(root, store_a, fence_a_later, o1)),
            "foreign fence epoch must not equate"
        );
        assert!(
            !fork_equivalence(&a, &ForkPoint::seal(root, store_a, fence_a, o2)),
            "foreign commit ordinal must not equate"
        );

        Ok(())
    }

    // ── content-addressing: same content ⇒ same root, 1 bit ⇒ different ──

    /// Write a set of pairs into a fresh fjall store in the given commit
    /// batching, then return the whole-store root. The batching (how the
    /// puts are split across commits, and their order) is the "write
    /// history" that must NOT affect the root.
    fn root_after_history(content: &[(Vec<u8>, Vec<u8>)], batches: &[usize]) -> Result<MerkleHash> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let mut idx = 0usize;
        for &batch in batches {
            let mut tx = db.write_tx()?;
            for _ in 0..batch {
                if idx >= content.len() {
                    break;
                }
                let (k, v) = &content[idx];
                tx.put(k, v)?;
                idx += 1;
            }
            tx.commit()?;
        }
        // Any remainder in one last commit.
        if idx < content.len() {
            let mut tx = db.write_tx()?;
            for (k, v) in &content[idx..] {
                tx.put(k, v)?;
            }
            tx.commit()?;
        }
        let tx = db.read_tx()?;
        state_root(&tx, big_budget()?)
    }

    #[test]
    fn same_content_different_history_same_root() -> Result<()> {
        let mut content: Vec<(Vec<u8>, Vec<u8>)> = (0..64u32)
            .map(|i| {
                (
                    format!("rel:{:03}", (i * 37) % 64).into_bytes(),
                    format!("value-{i}").into_bytes(),
                )
            })
            .collect();
        content.sort();
        content.dedup_by(|a, b| a.0 == b.0);

        // History A: insertion order = content order, one big commit.
        let root_a = root_after_history(&content, &[content.len()])?;

        // History B: reversed insertion order, split across many small,
        // uneven commits.
        let mut reversed = content.clone();
        reversed.reverse();
        let root_b = root_after_history(&reversed, &[1, 5, 2, 9, 3, 7, 11])?;

        // History C: a shuffle (deterministic), yet another batching.
        let mut shuffled = content.clone();
        // simple deterministic permutation
        shuffled.sort_by_key(|(k, _)| {
            let mut s = 0u64;
            for b in k {
                // INVARIANT(djb2): classic djb2 string hash; wrap is the published mix.
                s = s.wrapping_mul(131).wrapping_add(u64::from(*b));
            }
            s
        });
        let root_c = root_after_history(&shuffled, &[13, 13, 13, 13, 13])?;

        assert_eq!(root_a, root_b, "write history changed the root (A vs B)");
        assert_eq!(root_a, root_c, "write history changed the root (A vs C)");

        Ok(())
    }

    #[test]
    fn single_byte_difference_changes_the_root() -> Result<()> {
        let base: Vec<(Vec<u8>, Vec<u8>)> = (0..32u32)
            .map(|i| {
                (
                    format!("k{i:03}").into_bytes(),
                    format!("v{i:03}").into_bytes(),
                )
            })
            .collect();
        let root = root_after_history(&base, &[8, 8, 8, 8])?;

        // Flip one byte of one value.
        let mut edited_val = base.clone();
        edited_val[10].1[1] ^= 0x01;
        assert_ne!(root, root_after_history(&edited_val, &[32])?, "value edit");

        // Flip one byte of one key.
        let mut edited_key = base.clone();
        edited_key[20].0[1] ^= 0x01;
        edited_key.sort();
        assert_ne!(root, root_after_history(&edited_key, &[32])?, "key edit");

        // Drop one pair entirely.
        let mut dropped = base.clone();
        dropped.remove(5);
        assert_ne!(root, root_after_history(&dropped, &[31])?, "missing pair");

        Ok(())
    }

    #[test]
    fn root_is_stable_across_reopen() -> Result<()> {
        let content: Vec<(Vec<u8>, Vec<u8>)> = (0..20u32)
            .map(|i| {
                (
                    format!("k{i:03}").into_bytes(),
                    format!("v{i}").into_bytes(),
                )
            })
            .collect();
        let dir = tempfile::tempdir().into_diagnostic()?;
        let first = {
            let db = new_fjall_storage(dir.path())?;
            let mut tx = db.write_tx()?;
            for (k, v) in &content {
                tx.put(k, v)?;
            }
            tx.commit()?;
            let tx = db.read_tx()?;
            state_root(&tx, big_budget()?)?
        };
        // Reopen the same directory; the root must be byte-identical.
        let db = new_fjall_storage(dir.path())?;
        let tx = db.read_tx()?;
        assert_eq!(first, state_root(&tx, big_budget()?)?);

        Ok(())
    }

    // ── per-relation roots and typed refusals ────────────────────────────

    #[test]
    fn relation_root_covers_exactly_its_prefix() -> Result<()> {
        // Two relations sharing the keyspace, separated by the 8-byte id
        // prefix. The per-relation root must equal a root over just that
        // relation's rows, and must be blind to the other relation.
        let rel_a = RelationId::new(7).ok_or_else(|| miette!("relation id"))?;
        let rel_b = RelationId::new(9).ok_or_else(|| miette!("relation id"))?;
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let mut tx = db.write_tx()?;
        let mut a_pairs = Vec::new();
        for i in 0..10u32 {
            let mut k = rel_a.raw().to_be_bytes().to_vec();
            k.extend_from_slice(format!("row{i:02}").as_bytes());
            let v = format!("a{i}").into_bytes();
            tx.put(&k, &v)?;
            a_pairs.push((k, v));
        }
        for i in 0..5u32 {
            let mut k = rel_b.raw().to_be_bytes().to_vec();
            k.extend_from_slice(format!("row{i:02}").as_bytes());
            tx.put(&k, format!("b{i}").as_bytes())?;
        }
        tx.commit()?;
        let tx = db.read_tx()?;

        let via_relation = relation_root(&tx, rel_a, big_budget()?)?;
        let via_pairs = root_over(
            Box::new(
                a_pairs
                    .into_iter()
                    .map(|(k, v)| Ok((Slice::from(k), Slice::from(v)))),
            ),
            big_budget()?,
        )?;
        assert_eq!(via_relation, via_pairs);

        // Editing relation B leaves relation A's root untouched.
        let mut tx = db.write_tx()?;
        let mut k = rel_b.raw().to_be_bytes().to_vec();
        k.extend_from_slice(b"row00");
        tx.put(&k, b"changed")?;
        tx.commit()?;
        let tx = db.read_tx()?;
        assert_eq!(via_relation, relation_root(&tx, rel_a, big_budget()?)?);

        Ok(())
    }

    #[test]
    fn empty_relation_roots_to_the_empty_hash() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let tx = db.read_tx()?;
        assert_eq!(
            relation_root(
                &tx,
                RelationId::new(3).ok_or_else(|| miette!("relation id"))?,
                big_budget()?
            )?,
            empty_hash()
        );
        assert_eq!(state_root(&tx, big_budget()?)?, empty_hash());

        Ok(())
    }

    #[test]
    fn scan_ceiling_is_a_typed_refusal() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let mut tx = db.write_tx()?;
        for i in 0..10u32 {
            tx.put(format!("k{i}").as_bytes(), b"v")?;
        }
        tx.commit()?;
        let tx = db.read_tx()?;
        // Ceiling below the entry count ⇒ refuse, never a partial root.
        let err = state_root(&tx, NonZeroU64::new(5).ok_or_else(|| miette!("nonzero"))?)
            .expect_err("must refuse when the scan exceeds the ceiling");
        assert!(err.to_string().contains("ceiling"), "{err}");
        // Ceiling at the exact count succeeds.
        assert!(state_root(&tx, NonZeroU64::new(10).ok_or_else(|| miette!("nonzero"))?).is_ok());

        Ok(())
    }

    // ── replica-equivalence: recompute-and-compare (§58) ─────────────────

    /// Two-instance check (single transport): each side independently
    /// recomputes its chained root from ordered facts; compare via
    /// [`replica_equivalence_at_cut`] / [`roots_equal_at_cut`]. Load-bearing:
    /// a peer-delivered root that matches instance A must *not* make divergent
    /// instance B look equivalent — trusting the received digest would pass
    /// the trap control; the protocol forces both sides to recompute.
    #[test]
    fn replica_equivalence_two_instance_recompute_and_compare() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::open::StoreId;
        use crate::store::sweep::CommitOrdinal;

        use super::{
            ChainLinkKind, GENESIS_ROOT, MerkleChainRefuse, PathUrlSamenessClaim,
            ReplicaCutRecompute, refuse_path_url_sameness, replica_equivalence_at_cut,
            roots_equal_at_cut,
        };

        let store_id = StoreId::from_digest([0x58; 32]);
        let fence = FenceEpoch::genesis(store_id);
        let ordinal = CommitOrdinal::ZERO.successor()?;

        // Same ordered facts on both instances (single-transport replay).
        let facts: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"k00".to_vec(), b"v0".to_vec()),
            (b"k01".to_vec(), b"v1".to_vec()),
            (b"k02".to_vec(), b"v2".to_vec()),
        ];
        let content_a = StateRoot::from_merkle(root_of_pairs(&facts)?);
        let content_b = StateRoot::from_merkle(root_of_pairs(&facts)?);
        assert_eq!(
            content_a, content_b,
            "independent cold folds of the same ordered facts must agree"
        );

        let instance_a = ReplicaCutRecompute::from_local(
            store_id,
            fence,
            ordinal,
            content_a,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        );
        let instance_b = ReplicaCutRecompute::from_local(
            store_id,
            fence,
            ordinal,
            content_b,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        );

        assert!(
            replica_equivalence_at_cut(instance_a, instance_b),
            "two-instance: same ordered facts → matching roots by independent recompute"
        );
        assert!(
            roots_equal_at_cut(instance_a.recompute(), instance_b.recompute()),
            "comparison basis is roots_equal_at_cut over recomputed digests"
        );

        // Divergent facts on B — independent recompute must disagree.
        let mut facts_divergent = facts.clone();
        facts_divergent[1].1 = b"vX".to_vec();
        let content_divergent = StateRoot::from_merkle(root_of_pairs(&facts_divergent)?);
        assert_ne!(content_a, content_divergent);
        let instance_b_divergent = ReplicaCutRecompute::from_local(
            store_id,
            fence,
            ordinal,
            content_divergent,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        );
        assert!(
            !replica_equivalence_at_cut(instance_a, instance_b_divergent),
            "two-instance: divergent ordered facts must not be replica-equivalent"
        );

        // Trust trap: A delivers its recomputed root; B's content diverges.
        // Comparing the delivered digest to itself (or skipping B's recompute)
        // would falsely claim equivalence. The protocol never takes a received
        // root — both sides recompute from local material.
        let delivered_from_a = instance_a.recompute();
        assert!(
            roots_equal_at_cut(delivered_from_a, delivered_from_a),
            "control: trusting a received root against itself would pass"
        );
        assert!(
            !replica_equivalence_at_cut(instance_a, instance_b_divergent),
            "recompute-and-compare: delivered root is not the comparison basis"
        );
        assert_ne!(
            delivered_from_a,
            instance_b_divergent.recompute(),
            "B's independent recompute differs from A's delivered root"
        );

        // Path/URL "same store" must refuse — location is not equivalence.
        let path_claim =
            PathUrlSamenessClaim::claim("/var/lib/kyzo/replica-a", "/var/lib/kyzo/replica-a");
        assert_eq!(path_claim.left_location(), path_claim.right_location());
        assert_eq!(
            refuse_path_url_sameness(path_claim),
            MerkleChainRefuse::PathUrlSameness,
            "path/URL sameness claim must refuse — not store equivalence"
        );

        Ok(())
    }

    // ── meaning × WAL byte-chain composition (§24 + §56) ─────────────────

    /// Load-bearing: [`DurableCommitCut`] binds meaning tip and WAL
    /// `final_hash` together. Breaking either chain's tip at the boundary
    /// breaks composed-cut equality. If `compose_durable_cut` omitted
    /// `wal_final_hash`, a broken WAL tip would still equal the honest cut.
    #[test]
    fn durable_commit_cut_composition_binds_meaning_and_wal_hash() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::open::StoreId;
        use crate::store::sweep::CommitOrdinal;
        use crate::store::wal::{GENESIS_PREDECESSOR, WalPayload, WalRecord};

        use super::{ChainLinkKind, ChainedStateRoot, DurableCommitCut, GENESIS_ROOT, cuts_equal};

        let store_id = StoreId::from_digest([0x24; 32]);
        let fence = FenceEpoch::genesis(store_id);
        let ordinal = CommitOrdinal::ZERO.successor()?;
        let content = StateRoot::from_merkle(root_of_pairs(&[(b"k".to_vec(), b"v".to_vec())])?);

        let meaning = ChainedStateRoot::mint(
            store_id,
            fence,
            ordinal,
            content,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        );

        // Honest WAL Commit record: body binds the meaning tip; tip = final_hash.
        let honest_record = WalRecord::seal(
            GENESIS_PREDECESSOR,
            WalPayload::Commit {
                commit_ordinal: ordinal,
                body: meaning.root().as_bytes().to_vec(),
            },
        )?;
        let honest_wal = honest_record.record_hash();
        let honest = DurableCommitCut::compose(&meaning, honest_wal);
        assert_eq!(honest.wal_final_hash(), honest_wal);
        assert_eq!(honest.meaning_root(), meaning.root());
        assert!(cuts_equal(
            honest,
            DurableCommitCut::compose(&meaning, honest_wal)
        ));

        // Break meaning bind (different content → different chained root).
        let other_content =
            StateRoot::from_merkle(root_of_pairs(&[(b"k".to_vec(), b"X".to_vec())])?);
        let broken_meaning = ChainedStateRoot::mint(
            store_id,
            fence,
            ordinal,
            other_content,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        );
        let meaning_broken = DurableCommitCut::compose(&broken_meaning, honest_wal);
        assert!(
            !cuts_equal(honest, meaning_broken),
            "breaking meaning tip at the boundary must break composed cut"
        );

        // Break WAL bind (different body under same predecessor → different final_hash).
        let broken_wal_record = WalRecord::seal(
            GENESIS_PREDECESSOR,
            WalPayload::Commit {
                commit_ordinal: ordinal,
                body: vec![0xDE, 0xAD],
            },
        )?;
        let wal_broken = DurableCommitCut::compose(&meaning, broken_wal_record.record_hash());
        assert_ne!(
            honest.wal_final_hash(),
            wal_broken.wal_final_hash(),
            "WAL body edit must change final_hash"
        );
        assert!(
            !cuts_equal(honest, wal_broken),
            "breaking WAL tip at the boundary must break composed cut"
        );

        // Control: a composition that omitted wal_final_hash would still match
        // when only the WAL tip changes — prove the bind covers WalHash.
        let meaning_only = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"kyzo.durable_commit_cut.v1");
            h.update(honest.meaning_root().as_bytes());
            // deliberately omit wal_final_hash
            h.update(u64::to_be_bytes(ordinal.get()));
            let d: [u8; 32] = h.finalize().into();
            d
        };
        let meaning_only_broken_wal = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"kyzo.durable_commit_cut.v1");
            h.update(honest.meaning_root().as_bytes());
            h.update(u64::to_be_bytes(ordinal.get()));
            let d: [u8; 32] = h.finalize().into();
            d
        };
        assert_eq!(
            meaning_only, meaning_only_broken_wal,
            "control: omitting WalHash hides a WAL tip break"
        );
        assert_ne!(
            honest.composed(),
            wal_broken.composed(),
            "real compose covers wal_final_hash — WAL tip break changes composed"
        );

        Ok(())
    }

    // ── STH gossip non-equivocation (CT; seats 2/56/58/69) ────────────────

    fn sth_demo_chain() -> Result<(
        crate::store::open::StoreId,
        crate::store::epoch::FenceEpoch,
        super::RootChain,
        crate::store::sweep::CommitOrdinal,
        crate::store::sweep::CommitOrdinal,
        crate::store::sweep::CommitOrdinal,
    )> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::open::StoreId;
        use crate::store::sweep::CommitOrdinal;

        use super::{ChainLinkKind, ChainedStateRoot, GENESIS_ROOT, RootChain};

        let store_id = StoreId::from_digest([0xC7; 32]);
        let fence = FenceEpoch::genesis(store_id);
        let o1 = CommitOrdinal::ZERO.successor()?;
        let o2 = o1.successor()?;
        let o3 = o2.successor()?;

        let c1 = StateRoot::from_merkle(root_of_pairs(&[(b"a".to_vec(), b"1".to_vec())])?);
        let c2 = StateRoot::from_merkle(root_of_pairs(&[
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ])?);
        let c3 = StateRoot::from_merkle(root_of_pairs(&[
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
            (b"c".to_vec(), b"3".to_vec()),
        ])?);

        let mut chain = RootChain::empty();
        chain.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o1,
            c1,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        ))?;
        chain.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o2,
            c2,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        ))?;
        chain.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o3,
            c3,
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        ))?;
        Ok((store_id, fence, chain, o1, o2, o3))
    }

    /// Nasty: equivocating Store shows divergent histories to two peers —
    /// same ordinal, different roots → Detected-on-gossip before chains meet.
    #[test]
    fn sth_gossip_split_view_detected_before_chains_meet() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::open::StoreId;
        use crate::store::sweep::CommitOrdinal;

        use super::{
            ChainLinkKind, ChainedStateRoot, GENESIS_ROOT, GossipConsistency, MerkleChainRefuse,
            RootChain, StateRootHead, check_sth_gossip,
        };

        let store_id = StoreId::from_digest([0xE1; 32]);
        let fence = FenceEpoch::genesis(store_id);
        let o1 = CommitOrdinal::ZERO.successor()?;
        let o2 = o1.successor()?;

        // Peer A observes honest chain tip at o2.
        let mut honest = RootChain::empty();
        let c1 = StateRoot::from_merkle(root_of_pairs(&[(b"k".to_vec(), b"v".to_vec())])?);
        let c2 = StateRoot::from_merkle(root_of_pairs(&[
            (b"k".to_vec(), b"v".to_vec()),
            (b"m".to_vec(), b"n".to_vec()),
        ])?);
        honest.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o1,
            c1,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        ))?;
        honest.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o2,
            c2,
            honest.prior_root(),
            ChainLinkKind::Ordinary,
        ))?;
        let head_a = StateRootHead::from_chain_tip(&honest)?;

        // Peer B observes an equivocating fork: same StoreId/epoch/ordinal,
        // divergent content under o2 (split-view).
        let mut evil = RootChain::empty();
        let evil_c1 = StateRoot::from_merkle(root_of_pairs(&[(b"k".to_vec(), b"v".to_vec())])?);
        let evil_c2 = StateRoot::from_merkle(root_of_pairs(&[
            (b"k".to_vec(), b"v".to_vec()),
            (b"m".to_vec(), b"EVIL".to_vec()),
        ])?);
        evil.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o1,
            evil_c1,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        ))?;
        evil.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o2,
            evil_c2,
            evil.prior_root(),
            ChainLinkKind::Ordinary,
        ))?;
        let head_b = StateRootHead::from_chain_tip(&evil)?;

        assert_eq!(head_a.store_id(), head_b.store_id());
        assert_eq!(head_a.commit_ordinal(), head_b.commit_ordinal());
        assert_ne!(
            head_a.root(),
            head_b.root(),
            "equivocating Store produced divergent tips at the same cut"
        );

        // Detection BEFORE the two chains are ever merged/compared as full
        // histories — gossip of compact heads alone is enough.
        assert_eq!(
            check_sth_gossip(&head_a, &head_b, None),
            Err(MerkleChainRefuse::SplitViewDetected),
            "split-view must be Detected-on-gossip, not Unexposed-until-chains-meet"
        );

        // Control: identical observations are consistent.
        assert_eq!(
            check_sth_gossip(&head_a, &head_a, None),
            Ok(GossipConsistency::Identical)
        );

        Ok(())
    }

    /// Distinct Store identities on an STH pair → [`MerkleChainRefuse::SthStoreMismatch`]
    /// from both gossip and consistency-proof verify (never soft-pass).
    #[test]
    fn sth_store_mismatch_refuses_cross_store_gossip_and_proof() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::open::StoreId;
        use crate::store::sweep::CommitOrdinal;

        use super::{
            ChainLinkKind, ChainedStateRoot, GENESIS_ROOT, MerkleChainRefuse, RootChain,
            StateRootHead, build_consistency_proof, check_sth_gossip, verify_consistency_proof,
        };

        let store_a = StoreId::from_digest([0xA1; 32]);
        let store_b = StoreId::from_digest([0xB2; 32]);
        let fence_a = FenceEpoch::genesis(store_a);
        let fence_b = FenceEpoch::genesis(store_b);
        let o1 = CommitOrdinal::ZERO.successor()?;
        let o2 = o1.successor()?;

        let mut chain_a = RootChain::empty();
        let c1 = StateRoot::from_merkle(root_of_pairs(&[(b"x".to_vec(), b"1".to_vec())])?);
        let c2 = StateRoot::from_merkle(root_of_pairs(&[
            (b"x".to_vec(), b"1".to_vec()),
            (b"y".to_vec(), b"2".to_vec()),
        ])?);
        chain_a.append(ChainedStateRoot::mint(
            store_a,
            fence_a,
            o1,
            c1,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        ))?;
        chain_a.append(ChainedStateRoot::mint(
            store_a,
            fence_a,
            o2,
            c2,
            chain_a.prior_root(),
            ChainLinkKind::Ordinary,
        ))?;

        let mut chain_b = RootChain::empty();
        chain_b.append(ChainedStateRoot::mint(
            store_b,
            fence_b,
            o1,
            c1,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        ))?;

        let head_a = StateRootHead::from_cut(&chain_a, o1)?;
        let head_b = StateRootHead::from_cut(&chain_b, o1)?;
        assert_ne!(head_a.store_id(), head_b.store_id());

        assert_eq!(
            check_sth_gossip(&head_a, &head_b, None),
            Err(MerkleChainRefuse::SthStoreMismatch)
        );

        let proof = build_consistency_proof(&chain_a, o1, o2)?;
        let newer_a = StateRootHead::from_cut(&chain_a, o2)?;
        assert_eq!(
            verify_consistency_proof(&head_b, &newer_a, &proof),
            Err(MerkleChainRefuse::SthStoreMismatch),
            "cross-store heads must refuse even with a well-formed same-store proof"
        );

        Ok(())
    }

    /// Honest extension: older→newer STH verifies under a consistency proof.
    #[test]
    fn sth_consistency_proof_honest_extension_verifies() -> Result<()> {
        use super::{
            GossipConsistency, StateRootHead, build_consistency_proof, check_sth_gossip,
            verify_consistency_proof,
        };

        let (_store, _fence, chain, o1, _o2, o3) = sth_demo_chain()?;
        let older = StateRootHead::from_cut(&chain, o1)?;
        let newer = StateRootHead::from_cut(&chain, o3)?;
        let proof = build_consistency_proof(&chain, o1, o3)?;
        assert!(verify_consistency_proof(&older, &newer, &proof).is_ok());
        assert_eq!(
            check_sth_gossip(&older, &newer, Some(&proof)),
            Ok(GossipConsistency::ConsistentExtension)
        );
        // Missing proof between unequal ordinals refuses (does not soft-pass).
        assert!(matches!(
            check_sth_gossip(&older, &newer, None),
            Err(super::MerkleChainRefuse::ConsistencyProofRequired)
        ));

        Ok(())
    }

    /// Divergent histories cannot forge a consistency proof that binds both heads.
    #[test]
    fn sth_consistency_proof_rejects_equivocating_extension() -> Result<()> {
        use crate::store::epoch::FenceEpoch;
        use crate::store::open::StoreId;
        use crate::store::sweep::CommitOrdinal;

        use super::{
            ChainLinkKind, ChainedStateRoot, GENESIS_ROOT, MerkleChainRefuse, RootChain,
            StateRootHead, build_consistency_proof, check_sth_gossip, verify_consistency_proof,
        };

        let store_id = StoreId::from_digest([0xE2; 32]);
        let fence = FenceEpoch::genesis(store_id);
        let o1 = CommitOrdinal::ZERO.successor()?;
        let o2 = o1.successor()?;

        let mut honest = RootChain::empty();
        let c1 = StateRoot::from_merkle(root_of_pairs(&[(b"x".to_vec(), b"1".to_vec())])?);
        let c2 = StateRoot::from_merkle(root_of_pairs(&[
            (b"x".to_vec(), b"1".to_vec()),
            (b"y".to_vec(), b"2".to_vec()),
        ])?);
        honest.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o1,
            c1,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        ))?;
        honest.append(ChainedStateRoot::mint(
            store_id,
            fence,
            o2,
            c2,
            honest.prior_root(),
            ChainLinkKind::Ordinary,
        ))?;

        let older = StateRootHead::from_cut(&honest, o1)?;
        // Equivocating "newer" tip: same ordinal family but forged root bytes
        // presented as a head without a binding proof from the honest chain.
        let forged_newer = {
            // Build evil chain that shares o1 content then diverges.
            let mut evil = RootChain::empty();
            evil.append(ChainedStateRoot::mint(
                store_id,
                fence,
                o1,
                c1,
                GENESIS_ROOT,
                ChainLinkKind::Ordinary,
            ))?;
            let evil_c2 = StateRoot::from_merkle(root_of_pairs(&[
                (b"x".to_vec(), b"1".to_vec()),
                (b"y".to_vec(), b"FORGED".to_vec()),
            ])?);
            evil.append(ChainedStateRoot::mint(
                store_id,
                fence,
                o2,
                evil_c2,
                evil.prior_root(),
                ChainLinkKind::Ordinary,
            ))?;
            StateRootHead::from_chain_tip(&evil)?
        };

        // Honest proof cannot bind the forged newer head.
        let honest_proof = build_consistency_proof(&honest, o1, o2)?;
        assert!(matches!(
            verify_consistency_proof(&older, &forged_newer, &honest_proof),
            Err(MerkleChainRefuse::ConsistencyProofFailed)
        ));
        assert_eq!(
            check_sth_gossip(&older, &forged_newer, Some(&honest_proof)),
            Err(MerkleChainRefuse::SplitViewDetected),
            "forged extension must surface as split-view on gossip"
        );

        Ok(())
    }
}
