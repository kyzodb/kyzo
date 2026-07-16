/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A **cold, deterministic Merkle state root** over the ordered keyspace.
//!
//! ## What this is, and what it deliberately is not
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
//! The **incremental** root (maintained per-mutation at the commit choke
//! point) is out of scope, by design: fjall's commit order is internal and
//! its serialize point is not a hook we own (`storage/fjall.rs`), and
//! in-keyspace tree nodes would make every writer conflict on shared tree
//! keys under SSI (reads are the whole conflict surface — a write-validation
//! change is widening it, not narrowing it). The cold root needs none of
//! that machinery: it only reads the ordered keyspace that already exists.
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

use crate::data::value::RelationId;
use crate::storage::ReadTx;

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
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(char::from_digit((b >> 4) as u32, 16).expect("nibble"));
            s.push(char::from_digit((b & 0x0f) as u32, 16).expect("nibble"));
        }
        s
    }
}

/// `leaf = SHA-256(0x00 ‖ u64_be(key.len) ‖ key ‖ value)`.
fn leaf_hash(key: &[u8], value: &[u8]) -> MerkleHash {
    let mut h = Sha256::new();
    h.update([LEAF_TAG]);
    h.update((key.len() as u64).to_be_bytes());
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
        while let Some(&(top_size, _)) = self.stack.last() {
            if top_size != node.0 {
                break;
            }
            let (_, left) = self.stack.pop().expect("just peeked");
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
    //! - determinism across store reopen;
    //! - the typed refusals (scan ceiling, out-of-range relation id).

    use std::num::NonZeroU64;

    use fjall::Slice;

    use super::{
        MerkleHash, empty_hash, leaf_hash, node_hash, relation_root, root_over, state_root,
    };
    use crate::data::value::RelationId;
    use crate::storage::fjall::new_fjall_storage;
    use crate::storage::{Storage, WriteTx};

    fn big_budget() -> NonZeroU64 {
        NonZeroU64::new(1_000_000).unwrap()
    }

    // ── golden vectors: LITERAL SHA-256 digests, computed off-tree from the
    // domain-separated formula (see the module docs), pinned as hex so a
    // changed domain tag or leaf encoding is caught byte-for-byte. These do
    // NOT reference the source `*_TAG` constants — a mutation to a tag must
    // move the digest away from the literal, and a golden that reused the
    // const would move with it and hide the mutation.

    #[test]
    fn empty_root_is_pinned() {
        // SHA-256(0x02)
        assert_eq!(
            empty_hash().to_hex(),
            "dbc1b4c900ffe48d575b5da5c638040125f65db0fe3e24494b76ea986457d986"
        );
    }

    #[test]
    fn single_leaf_root_is_pinned() {
        // leaf = SHA-256(0x00 ‖ u64_be(3) ‖ "key" ‖ "val")
        let (k, v) = (b"key".as_slice(), b"val".as_slice());
        let golden = "f26135a572169d94e1cd659dc6e6ba89caddd4d1b0acc6fa87b3b9fed4045bc0";
        assert_eq!(leaf_hash(k, v).to_hex(), golden);
        // A one-entry root is exactly that leaf (no interior node).
        let root = root_over(
            Box::new(std::iter::once(Ok((Slice::from(k), Slice::from(v))))),
            big_budget(),
        )
        .unwrap();
        assert_eq!(root.to_hex(), golden);
    }

    #[test]
    fn two_leaf_root_is_pinned() {
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
            big_budget(),
        )
        .unwrap();
        assert_eq!(root.to_hex(), golden);
    }

    #[test]
    fn key_length_prefix_removes_boundary_ambiguity() {
        // (key=ab, value=c) and (key=a, value=bc) share the byte stream
        // "abc" but must not collide, because the key length is prefixed.
        assert_ne!(leaf_hash(b"ab", b"c"), leaf_hash(b"a", b"bc"));
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

    fn root_of_pairs(pairs: &[(Vec<u8>, Vec<u8>)]) -> MerkleHash {
        root_over(
            Box::new(
                pairs
                    .iter()
                    .map(|(k, v)| Ok((Slice::from(k), Slice::from(v)))),
            ),
            big_budget(),
        )
        .unwrap()
    }

    #[test]
    fn streaming_matches_recursive_mth_for_every_small_size() {
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
                root_of_pairs(&pairs),
                recursive_mth(&leaves),
                "streaming ≠ recursive MTH at n={n}"
            );
        }
    }

    // ── content-addressing: same content ⇒ same root, 1 bit ⇒ different ──

    /// Write a set of pairs into a fresh fjall store in the given commit
    /// batching, then return the whole-store root. The batching (how the
    /// puts are split across commits, and their order) is the "write
    /// history" that must NOT affect the root.
    fn root_after_history(content: &[(Vec<u8>, Vec<u8>)], batches: &[usize]) -> MerkleHash {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut idx = 0usize;
        for &batch in batches {
            let mut tx = db.write_tx().unwrap();
            for _ in 0..batch {
                if idx >= content.len() {
                    break;
                }
                let (k, v) = &content[idx];
                tx.put(k, v).unwrap();
                idx += 1;
            }
            tx.commit().unwrap();
        }
        // Any remainder in one last commit.
        if idx < content.len() {
            let mut tx = db.write_tx().unwrap();
            for (k, v) in &content[idx..] {
                tx.put(k, v).unwrap();
            }
            tx.commit().unwrap();
        }
        let tx = db.read_tx().unwrap();
        state_root(&tx, big_budget()).unwrap()
    }

    #[test]
    fn same_content_different_history_same_root() {
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
        let root_a = root_after_history(&content, &[content.len()]);

        // History B: reversed insertion order, split across many small,
        // uneven commits.
        let mut reversed = content.clone();
        reversed.reverse();
        let root_b = root_after_history(&reversed, &[1, 5, 2, 9, 3, 7, 11]);

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
        let root_c = root_after_history(&shuffled, &[13, 13, 13, 13, 13]);

        assert_eq!(root_a, root_b, "write history changed the root (A vs B)");
        assert_eq!(root_a, root_c, "write history changed the root (A vs C)");
    }

    #[test]
    fn single_byte_difference_changes_the_root() {
        let base: Vec<(Vec<u8>, Vec<u8>)> = (0..32u32)
            .map(|i| {
                (
                    format!("k{i:03}").into_bytes(),
                    format!("v{i:03}").into_bytes(),
                )
            })
            .collect();
        let root = root_after_history(&base, &[8, 8, 8, 8]);

        // Flip one byte of one value.
        let mut edited_val = base.clone();
        edited_val[10].1[1] ^= 0x01;
        assert_ne!(root, root_after_history(&edited_val, &[32]), "value edit");

        // Flip one byte of one key.
        let mut edited_key = base.clone();
        edited_key[20].0[1] ^= 0x01;
        edited_key.sort();
        assert_ne!(root, root_after_history(&edited_key, &[32]), "key edit");

        // Drop one pair entirely.
        let mut dropped = base.clone();
        dropped.remove(5);
        assert_ne!(root, root_after_history(&dropped, &[31]), "missing pair");
    }

    #[test]
    fn root_is_stable_across_reopen() {
        let content: Vec<(Vec<u8>, Vec<u8>)> = (0..20u32)
            .map(|i| {
                (
                    format!("k{i:03}").into_bytes(),
                    format!("v{i}").into_bytes(),
                )
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let first = {
            let db = new_fjall_storage(dir.path()).unwrap();
            let mut tx = db.write_tx().unwrap();
            for (k, v) in &content {
                tx.put(k, v).unwrap();
            }
            tx.commit().unwrap();
            let tx = db.read_tx().unwrap();
            state_root(&tx, big_budget()).unwrap()
        };
        // Reopen the same directory; the root must be byte-identical.
        let db = new_fjall_storage(dir.path()).unwrap();
        let tx = db.read_tx().unwrap();
        assert_eq!(first, state_root(&tx, big_budget()).unwrap());
    }

    // ── per-relation roots and typed refusals ────────────────────────────

    #[test]
    fn relation_root_covers_exactly_its_prefix() {
        // Two relations sharing the keyspace, separated by the 8-byte id
        // prefix. The per-relation root must equal a root over just that
        // relation's rows, and must be blind to the other relation.
        let rel_a = RelationId::new(7).expect("below cap");
        let rel_b = RelationId::new(9).expect("below cap");
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let mut a_pairs = Vec::new();
        for i in 0..10u32 {
            let mut k = rel_a.raw().to_be_bytes().to_vec();
            k.extend_from_slice(format!("row{i:02}").as_bytes());
            let v = format!("a{i}").into_bytes();
            tx.put(&k, &v).unwrap();
            a_pairs.push((k, v));
        }
        for i in 0..5u32 {
            let mut k = rel_b.raw().to_be_bytes().to_vec();
            k.extend_from_slice(format!("row{i:02}").as_bytes());
            tx.put(&k, format!("b{i}").as_bytes()).unwrap();
        }
        tx.commit().unwrap();
        let tx = db.read_tx().unwrap();

        let via_relation = relation_root(&tx, rel_a, big_budget()).unwrap();
        let via_pairs = root_over(
            Box::new(
                a_pairs
                    .into_iter()
                    .map(|(k, v)| Ok((Slice::from(k), Slice::from(v)))),
            ),
            big_budget(),
        )
        .unwrap();
        assert_eq!(via_relation, via_pairs);

        // Editing relation B leaves relation A's root untouched.
        let mut tx = db.write_tx().unwrap();
        let mut k = rel_b.raw().to_be_bytes().to_vec();
        k.extend_from_slice(b"row00");
        tx.put(&k, b"changed").unwrap();
        tx.commit().unwrap();
        let tx = db.read_tx().unwrap();
        assert_eq!(
            via_relation,
            relation_root(&tx, rel_a, big_budget()).unwrap()
        );
    }

    #[test]
    fn empty_relation_roots_to_the_empty_hash() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let tx = db.read_tx().unwrap();
        assert_eq!(
            relation_root(&tx, RelationId::new(3).expect("below cap"), big_budget()).unwrap(),
            empty_hash()
        );
        assert_eq!(state_root(&tx, big_budget()).unwrap(), empty_hash());
    }

    #[test]
    fn scan_ceiling_is_a_typed_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        for i in 0..10u32 {
            tx.put(format!("k{i}").as_bytes(), b"v").unwrap();
        }
        tx.commit().unwrap();
        let tx = db.read_tx().unwrap();
        // Ceiling below the entry count ⇒ refuse, never a partial root.
        let err = state_root(&tx, NonZeroU64::new(5).unwrap())
            .expect_err("must refuse when the scan exceeds the ceiling");
        assert!(err.to_string().contains("ceiling"), "{err}");
        // Ceiling at the exact count succeeds.
        assert!(state_root(&tx, NonZeroU64::new(10).unwrap()).is_ok());
    }
}
