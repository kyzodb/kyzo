/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Some ideas are from https://github.com/schelterlabs/rust-minhash

/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0), re-architected for the KyzoDB kernel:
 *
 * - FORMAT FIX (ratified, on-disk): the original persisted MinHash
 *   permutations and band-chunk bytes by `unsafe` reinterpretation of
 *   `Vec<u32>` as `&[u8]` — NATIVE-ENDIAN on disk (non-portable between
 *   architectures) and undefined behavior on the read side (unaligned
 *   `*const u32` dereference). [`HashPermutations::to_bytes`] /
 *   [`from_bytes`](HashPermutations::from_bytes) and
 *   [`HashValues::to_bytes`] now spell EXPLICIT LITTLE-ENDIAN
 *   `to_le_bytes`/`from_le_bytes`, safely — the engine's
 *   `#![forbid(unsafe_code)]` forces the rewrite; little-endian is the
 *   ratified format decision.
 * - DETERMINISM + PORTABLE SIGNATURES (ratified, on-disk): the permutation
 *   seeds are now drawn deterministically from a pinned seed (splitmix64,
 *   the fixed-rule / HNSW house pattern), so two databases build the SAME
 *   index from the SAME facts — not just a rebuild from the persisted
 *   manifest. And the signature VALUES hash the memcmp-ENCODED bytes of each
 *   element through a seeded, portable xxHash32 stream, replacing the
 *   original's `std::hash::Hash` (native-endian integer / word-sized length
 *   writes, unpinned across Rust versions). Same facts ⇒ byte-identical band
 *   buckets ⇒ same near-duplicate answers, on every run, platform, and
 *   toolchain. See [`HashPermutations::new`] and the manifest's `perms` doc.
 * - The engine is PURE FUNCTIONS over the kernel's [`ReadTx`]/[`WriteTx`]
 *   species ([`lsh_put`], [`lsh_del`], [`lsh_search`]); the original's
 *   `SessionTx` methods die with `SessionTx`'s old shape. The RA operator
 *   tier drives the search; the mutation tier drives put/del.
 * - Law 5: the original's `unreachable!()`s on decoded inverse-index rows
 *   are the typed [`IndexRowCorrupt`] with the row's key context;
 *   `HashPermutations::from_bytes` refuses a byte length that is not a
 *   multiple of 4 (the original silently truncated); band arithmetic that
 *   does not add up is a typed manifest error, not a slice panic.
 * - [`lsh_search`] returns candidates in DETERMINISTIC order (sorted by
 *   base key): the original iterated an `FxHashSet` and, under `k`,
 *   truncated an unspecified subset. Which `k` of a larger candidate set
 *   survive is still unranked (LSH yields a candidate SET, not a ranking)
 *   — but it is now the same subset on every run and every platform.
 * - `LshParams::find_optimal_params` is ported intact (same numeric
 *   integration, same search loop), with a determinism test.
 * - Callers building the `TextAnalyzer` for these functions must key the
 *   tokenizer cache by the FULL index handle name (`{base}:{idx}`) — the
 *   original keyed inconsistently at create vs write time, so same-named
 *   LSH indices on different relations could tokenize writes with the
 *   wrong analyzer. The cache lives with the lifecycle tier; the contract
 *   is recorded here where the analyzer is consumed.
 */

//! The MinHash-LSH near-duplicate index engine: signature maintenance and
//! candidate search, against the kernel's transaction species.
//!
//! An LSH index is TWO stored relations plus a persisted manifest:
//!
//! - the **index relation** ([`lsh_index_metadata`]): one row per band,
//!   keyed `[band_chunk_bytes, base_key…]` with no value columns — a pure
//!   posting of "this row's signature has this band";
//! - the **inverse relation** ([`lsh_inv_index_metadata`]): keyed by the
//!   base key, holding the row's current band chunks (so deletion and
//!   re-put can find them without re-tokenizing).
//!
//! A row's text (or list) is min-hashed with [`HashPermutations`] into a
//! [`HashValues`] signature; the signature is split into `n_bands` bands of
//! `n_rows_in_band` hashes; two rows sharing any band are candidate
//! near-duplicates. [`LshParams::find_optimal_params`] chooses `(b, r)` to
//! minimize weighted false-positive/false-negative probability at the
//! declared similarity threshold.
//!
//! ## Result contract
//!
//! [`lsh_search`] returns a candidate SET, not a ranking: rows whose
//! signature collides with the query's in at least one band, in deterministic
//! base-key order. `k` keeps the SMALLEST `k` by base key — the SAME subset
//! whether or not a filter is present (both paths select smallest-k-by-key);
//! it does not pick the "most similar" `k`. Post-filtering with an actual
//! similarity
//! predicate is the caller's (Datalog's) job — the coherence thesis: fusion
//! and ranking are joins and score expressions, not API surface.
//!
//! ## Projection kind (story #305)
//!
//! [`Lsh`] is this engine's `K` parameterization of the shared
//! [`crate::engines::projection`] build→seal→query machine. Build→seal→query
//! goes through that machine; there is no bespoke per-engine seal or
//! freshness protocol. Relation-backed [`lsh_put`] / [`lsh_search`] remain
//! the kernel MinHash algorithms.
//!
//! ## Seams
//!
//! - **Lifecycle tier**: `::lsh create/drop` — creates both relations from
//!   the metadata minters here, runs [`LshParams::find_optimal_params`],
//!   draws [`HashPermutations`], backfills via [`lsh_put`], attaches the
//!   manifest (`IndexKind::Lsh`, naming the inverse relation) keeping
//!   `indices` sorted by name, and keys the tokenizer cache by FULL index
//!   handle name (see the header block).
//! - **`FtsIndexConfig` singularity** (story #305 T4, discharged): one
//!   declared-config spelling — [`crate::parse::sys::FtsIndexConfig`] —
//!   consumed by both parse and mutation; the engines/text twin is gone.

use std::cmp::min;
use std::collections::BTreeSet;
use std::hash::Hasher;

use miette::{Result, bail, miette};
use quadrature::integrate;
use smartstring::{LazyCompact, SmartString};
use twox_hash::XxHash32;

use crate::data::expr::Expr;
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::value::{DataValue, Tuple, append_canonical};
use crate::engines::IndexRowCorrupt;
use crate::engines::projection::ProjectionKind;
use crate::engines::text::TokenizerConfig;
use crate::engines::text::tokenizer::TextAnalyzer;
use crate::runtime::relation::RelationHandle;
use crate::storage::{ReadTx, WriteTx};

// ---------------------------------------------------------------------------
// Projection kind — `K` of the shared build→seal→query machine (#305).
// ---------------------------------------------------------------------------

/// MinHash-LSH as a projection kind: one `K` of
/// [`ProjectionBuilder`](crate::engines::projection::ProjectionBuilder) /
/// [`Sealed`](crate::engines::projection::Sealed).
///
/// Relation-backed signature maintenance and candidate search ([`lsh_put`],
/// [`lsh_search`]) are the kernel algorithms — not a second build/seal/
/// freshness protocol.
///
/// Constructed at seal sites once generation freshness is seated (T5 /
/// projections-views); the type is live under the machine's tests today.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct Lsh;

impl ProjectionKind for Lsh {
    type Query = LshSearchParams;
    /// Optional smallest-k-by-key bound from the search law.
    type Candidates = Option<usize>;

    fn search(&self, query: &Self::Query) -> Self::Candidates {
        query.k
    }
}

// ---------------------------------------------------------------------------
// The manifest: the index's persisted description.
// ---------------------------------------------------------------------------

/// The persisted description of one MinHash-LSH index. Serialized (msgpack,
/// struct maps) as the payload of the base relation's `IndexKind::Lsh`
/// catalog entry — **its wire form is an on-disk format**, pinned by the
/// pinned-bytes test below; changing it is a migration decision.
/// Stored MinHash permutation seed bytes for an LSH index.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde_derive::Serialize, serde_derive::Deserialize)]
#[repr(transparent)]
pub(crate) struct LshPermutationBytes(pub(crate) Vec<u8>);

const _: () = assert!(std::mem::size_of::<LshPermutationBytes>() == std::mem::size_of::<Vec<u8>>());
const _: () = assert!(std::mem::align_of::<LshPermutationBytes>() == std::mem::align_of::<Vec<u8>>());

impl std::ops::Deref for LshPermutationBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] { &self.0 }
}
impl std::ops::DerefMut for LshPermutationBytes {
    fn deref_mut(&mut self) -> &mut [u8] { &mut self.0 }
}
impl AsRef<[u8]> for LshPermutationBytes {
    fn as_ref(&self) -> &[u8] { &self.0 }
}
impl From<Vec<u8>> for LshPermutationBytes {
    fn from(v: Vec<u8>) -> Self { Self(v) }
}


#[derive(Debug, Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct MinHashLshIndexManifest {
    pub(crate) base_relation: SmartString<LazyCompact>,
    pub(crate) index_name: SmartString<LazyCompact>,
    /// The row-extraction expression as a PARSED typed substance (serde
    /// round-trips it through the value plane's `Expr` codec, arity re-proven
    /// on decode); never source text re-parsed at build time.
    pub(crate) extractor: crate::data::expr::Expr,
    pub(crate) n_gram: usize,
    pub(crate) tokenizer: TokenizerConfig,
    pub(crate) filters: Vec<TokenizerConfig>,

    pub(crate) num_perm: usize,
    pub(crate) n_bands: usize,
    pub(crate) n_rows_in_band: usize,
    pub(crate) threshold: f64,
    /// The permutation seeds as EXPLICIT LITTLE-ENDIAN u32 bytes (the ratified
    /// format fix; the original wrote native-endian by unsafe reinterpretation).
    /// The seeds are drawn DETERMINISTICALLY from the index's `seed` (default
    /// [`DEFAULT_PERM_SEED`]) via splitmix64, so two fresh builds of the same
    /// index produce byte-identical permutations — see [`HashPermutations::new`].
    ///
    /// FORMAT-RELEVANT (resolved, not a residual): the signature VALUES minted
    /// from these seeds hash the **memcmp-encoded** bytes of each element (and
    /// each n-gram's tokens) through a seeded, portable xxHash32 stream — NOT
    /// `std::hash::Hash`, whose integer and length writes are native-endian and
    /// unpinned across Rust versions. The stored band chunks are therefore
    /// portable across architectures and toolchains, and pinned by
    /// `signature_bytes_are_pinned_and_portable`. Changing the element encoding
    /// or the hash is an on-disk-format migration.
    pub(crate) perms: LshPermutationBytes,
}

impl MinHashLshIndexManifest {
    /// Decode the stored permutation seeds. Fallible: the bytes are a
    /// catalog payload and may be corrupt (the original truncated odd
    /// lengths silently).
    pub(crate) fn get_hash_perms(&self) -> Result<HashPermutations> {
        HashPermutations::from_bytes(&self.perms.0).map_err(|reason| {
            miette!(IndexRowCorrupt::new(
                &self.index_name,
                &[],
                format!("stored LSH permutations: {reason}"),
            ))
        })
    }
}

/// Mint the index relation's column metadata for an LSH index over `base`:
/// keys `[hash: Bytes, src_*…]`, no value columns.
pub(crate) fn lsh_index_metadata(base: &StoredRelationMetadata) -> StoredRelationMetadata {
    let mut keys = vec![ColumnDef {
        name: SmartString::from("hash"),
        typing: NullableColType {
            coltype: ColType::Bytes,
            nullable: false,
        },
        default_gen: None,
    }];
    for k in base.keys.iter() {
        keys.push(ColumnDef {
            name: format!("src_{}", k.name).into(),
            typing: k.typing.clone(),
            default_gen: None,
        });
    }
    StoredRelationMetadata {
        keys,
        non_keys: vec![],
    }
}

/// Mint the inverse relation's column metadata: keyed by the base key,
/// holding the row's band chunks.
///
/// Fix on port: the original declared this column `Bytes` but always
/// stored a LIST of bytes into it; the declaration now says what is
/// stored.
pub(crate) fn lsh_inv_index_metadata(base: &StoredRelationMetadata) -> StoredRelationMetadata {
    StoredRelationMetadata {
        keys: base.keys.clone(),
        non_keys: vec![ColumnDef {
            name: SmartString::from("minhash"),
            typing: NullableColType {
                coltype: ColType::List {
                    eltype: Box::new(NullableColType {
                        coltype: ColType::Bytes,
                        nullable: false,
                    }),
                    len: None,
                },
                nullable: false,
            },
            default_gen: None,
        }],
    }
}

// ---------------------------------------------------------------------------
// MinHash: permutations and signatures.
// ---------------------------------------------------------------------------

/// The default seed for the permutation draw when a `::lsh create` carries no
/// explicit `seed`. Pinned, like the fixed-rule tier's `SeededRng::DEFAULT_SEED`
/// and the HNSW level seed: two databases building the same index from the same
/// facts must draw the SAME permutations, or their band buckets — and hence
/// their near-duplicate answers — diverge at build time (a determinism-law
/// violation, and a falsification of the "replicas provably interchangeable"
/// claim). Changing this re-projects every future default-seed index.
pub(crate) const DEFAULT_PERM_SEED: u64 = 0x4c53_485f_5045_524d; // "LSH_PERM"

/// One splitmix64 step — the `storage::sim` / `fixed_rule::rng` house PRNG,
/// inlined here to draw permutation seeds deterministically. A pure function of
/// its state; no platform-dependent word size or endianness, so the seed pins
/// the drawn permutations on every target.
#[inline]
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The per-index hash seeds ("permutations"): one 32-bit seed per signature
/// position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HashPermutations(pub(crate) Vec<u32>);

impl HashPermutations {
    /// Draw `n_perms` permutation seeds DETERMINISTICALLY from `seed` (the
    /// lifecycle tier passes the `::lsh create` config's seed, defaulting to
    /// [`DEFAULT_PERM_SEED`]). Same seed ⇒ byte-identical permutations ⇒
    /// byte-identical band buckets on a fresh rebuild, on every run and every
    /// platform. The CozoDB original drew these from OS entropy
    /// (`rand::thread_rng`), so two builds of the same index diverged.
    pub(crate) fn new(n_perms: usize, seed: u64) -> Self {
        let mut state = seed;
        let mut perms = Vec::with_capacity(n_perms);
        for _ in 0..n_perms {
            // High 32 bits of a splitmix64 word (the finalizer diffuses the
            // whole word, so the high half is equidistributed).
            perms.push((splitmix64(&mut state) >> 32) as u32);
        }
        Self(perms)
    }

    /// The persisted form: each seed as EXPLICIT little-endian bytes (the
    /// ratified format fix; the original reinterpreted the `Vec<u32>`'s
    /// memory, persisting whatever the machine's endianness was).
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.0.len() * 4);
        for p in &self.0 {
            out.extend_from_slice(&p.to_le_bytes());
        }
        out
    }

    /// The inverse of [`to_bytes`](Self::to_bytes). Safe and fallible: a
    /// length that is not a multiple of 4 is corrupt, not silently
    /// truncated (and the original's unaligned `*const u32` read was UB).
    pub(crate) fn from_bytes(bytes: &[u8]) -> std::result::Result<Self, String> {
        if !bytes.len().is_multiple_of(4) {
            return Err(format!("length {} is not a multiple of 4", bytes.len()));
        }
        Ok(Self(
            bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        ))
    }
}

/// A MinHash signature: for each permutation seed, the minimum 32-bit hash
/// over the input's elements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HashValues(pub(crate) Vec<u32>);

/// The canonical, PORTABLE byte encoding of one indexed element (a list
/// member): its memcmp encoding — the pinned on-disk value order — rather than
/// its `std::hash::Hash` byte stream. This is the format fix: `Hash` for an
/// integer or a collection writes native-endian words and word-sized length
/// prefixes (`write_usize`), so the same value produced a different MinHash
/// signature on a different platform or Rust version — the same index, a
/// different near-duplicate answer. The canonical encoding is version- and platform-pinned.
fn element_bytes(v: &DataValue) -> Vec<u8> {
    let mut b = Vec::new();
    append_canonical(&mut b, v);
    b
}

/// The canonical byte encoding of one n-gram (a sequence of tokens): each token
/// memcmp-encoded as a string, concatenated. Portable for the same reason as
/// [`element_bytes`]; the string encoding's terminator keeps token boundaries
/// unambiguous, so distinct n-grams cannot alias.
fn ngram_bytes(tokens: &[SmartString<LazyCompact>]) -> Vec<u8> {
    let mut b = Vec::new();
    for t in tokens {
        append_canonical(&mut b, &DataValue::Str(t.to_string()));
    }
    b
}

impl HashValues {
    pub(crate) fn new(values: impl Iterator<Item = Vec<u8>>, perms: &HashPermutations) -> Self {
        let mut ret = Self::init(perms);
        ret.update(values, perms);
        ret
    }

    pub(crate) fn init(perms: &HashPermutations) -> Self {
        Self(vec![u32::MAX; perms.0.len()])
    }

    /// Fold each element's CANONICAL bytes (see [`element_bytes`] /
    /// [`ngram_bytes`]) into the running minimum, per permutation seed. The
    /// bytes are fed through `Hasher::write`, whose xxHash byte-stream is
    /// portable — unlike the original's `value.hash()`, which serialized
    /// integers and lengths native-endian.
    pub(crate) fn update(
        &mut self,
        values: impl Iterator<Item = Vec<u8>>,
        perms: &HashPermutations,
    ) {
        for v in values {
            for (i, seed) in perms.0.iter().enumerate() {
                let mut hasher = XxHash32::with_seed(*seed);
                hasher.write(&v);
                let hash = hasher.finish() as u32;
                self.0[i] = min(self.0[i], hash);
            }
        }
    }

    /// Estimated Jaccard similarity against another signature drawn from
    /// the same permutations.
    #[cfg(test)]
    pub(crate) fn jaccard(&self, other_minhash: &Self) -> f32 {
        let matches = self
            .0
            .iter()
            .zip(&other_minhash.0)
            .filter(|(left, right)| left == right)
            .count();
        matches as f32 / self.0.len() as f32
    }

    /// The signature as explicit little-endian bytes (see the header
    /// block: the original reinterpreted memory, native-endian).
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.0.len() * 4);
        for v in &self.0 {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Split the signature into its stored band chunks: for band `i`, the
    /// little-endian bytes of its `n_rows_in_band` hashes followed by `i`
    /// as little-endian `u16` (so identical band contents in different
    /// bands cannot collide).
    ///
    /// Typed errors where the original sliced unchecked: a signature/band
    /// arithmetic mismatch is a corrupt manifest, and more than `u16::MAX`
    /// bands would silently alias band tags (the original wrapped).
    pub(crate) fn band_chunks(
        &self,
        n_bands: usize,
        n_rows_in_band: usize,
    ) -> Result<Vec<Vec<u8>>> {
        if n_bands * n_rows_in_band != self.0.len() {
            bail!(
                "LSH manifest corrupt: {} bands of {} rows do not fit a signature of {} hashes",
                n_bands,
                n_rows_in_band,
                self.0.len()
            );
        }
        if n_bands > u16::MAX as usize {
            bail!("LSH manifest corrupt: {n_bands} bands exceed the u16 band tag");
        }
        let bytes = self.to_bytes();
        let chunk_size = n_rows_in_band * 4;
        Ok((0..n_bands)
            .map(|i| {
                let mut byte_range = bytes[i * chunk_size..(i + 1) * chunk_size].to_vec();
                byte_range.extend_from_slice(&(i as u16).to_le_bytes());
                byte_range
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Parameter search: bands × rows from threshold and error weights.
// ---------------------------------------------------------------------------

/// The banding parameters: `b` bands of `r` rows (`b * r` = permutations).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LshParams {
    pub b: usize,
    pub r: usize,
}

/// False-positive and false-negative weights for the parameter search.
#[derive(Clone)]
pub(crate) struct Weights(pub(crate) f64, pub(crate) f64);

const ALLOWED_INTEGRATE_ERR: f64 = 0.001;

// code is mostly from https://github.com/schelterlabs/rust-minhash/blob/81ea3fec24fd888a330a71b6932623643346b591/src/minhash_lsh.rs
impl LshParams {
    /// Exhaustively search `(b, r)` with `b * r <= num_perm` for the
    /// combination minimizing the weighted false-positive/false-negative
    /// probability at `threshold`. Ported intact from the original;
    /// deterministic (pure numeric integration, no randomness) — pinned by
    /// the determinism test.
    pub fn find_optimal_params(threshold: f64, num_perm: usize, weights: &Weights) -> LshParams {
        let Weights(false_positive_weight, false_negative_weight) = weights;
        let mut min_error = f64::INFINITY;
        let mut opt = LshParams { b: 0, r: 0 };
        for b in 1..num_perm + 1 {
            let max_r = num_perm / b;
            for r in 1..max_r + 1 {
                let false_pos = LshParams::false_positive_probability(threshold, b, r);
                let false_neg = LshParams::false_negative_probability(threshold, b, r);
                let error = false_pos * false_positive_weight + false_neg * false_negative_weight;
                if error < min_error {
                    min_error = error;
                    opt = LshParams { b, r };
                }
            }
        }
        opt
    }

    fn false_positive_probability(threshold: f64, b: usize, r: usize) -> f64 {
        let probability = |s| -> f64 { 1. - f64::powf(1. - f64::powi(s, r as i32), b as f64) };
        integrate(probability, 0.0, threshold, ALLOWED_INTEGRATE_ERR).integral
    }

    fn false_negative_probability(threshold: f64, b: usize, r: usize) -> f64 {
        let probability =
            |s| -> f64 { 1. - (1. - f64::powf(1. - f64::powi(s, r as i32), b as f64)) };
        integrate(probability, threshold, 1.0, ALLOWED_INTEGRATE_ERR).integral
    }
}

// ---------------------------------------------------------------------------
// The engine's entry points.
// ---------------------------------------------------------------------------

/// Decode an inverse-index row's value (the row's stored band chunks). The
/// original hit `unreachable!()` on any other shape; a wrong shape is
/// stored-byte corruption, typed with the row's key context.
fn decode_inv_chunks(
    mut found: Tuple,
    inv_idx: &RelationHandle,
    key: &[DataValue],
) -> Result<Vec<Vec<u8>>> {
    match found.pop() {
        Some(DataValue::List(l)) => l
            .into_iter()
            .map(|chunk| match chunk {
                DataValue::Bytes(b) => Ok(b),
                other => Err(miette!(IndexRowCorrupt::new(
                    &inv_idx.name,
                    key,
                    format!("inverse LSH row holds a non-bytes chunk: {other:?}"),
                ))),
            })
            .collect(),
        other => bail!(IndexRowCorrupt::new(
            &inv_idx.name,
            key,
            format!("inverse LSH row is not a list of chunks: {other:?}"),
        )),
    }
}

/// Un-index one base-relation row: delete its band postings and its
/// inverse-index row. `chunks` short-circuits the inverse lookup when the
/// caller (re-put) already has them.
///
/// Contract: the mutation tier calls this before deleting the row from the
/// base relation, in the same transaction.
pub(crate) fn lsh_del<T: WriteTx>(
    tx: &mut T,
    tuple: &[DataValue],
    chunks: Option<Vec<Vec<u8>>>,
    idx: &RelationHandle,
    inv_idx: &RelationHandle,
) -> Result<()> {
    let key_len = inv_idx.metadata.keys.len();
    if tuple.len() < key_len {
        bail!(IndexRowCorrupt::new(
            &inv_idx.name,
            tuple,
            "row shorter than the base relation's key",
        ));
    }
    let key_part = &tuple[..key_len];
    let chunks = match chunks {
        Some(c) => c,
        None => {
            let Some(found) = inv_idx.get_val_only(tx, key_part)? else {
                return Ok(());
            };
            // Decode BEFORE deleting: a corrupt inverse row must not be
            // half-consumed by its own error path (the original deleted
            // first and then hit `unreachable!`).
            let decoded = decode_inv_chunks(found, inv_idx, key_part)?;
            let inv_key = inv_idx.encode_key_for_store(key_part, SourceSpan::default())?;
            tx.del(&inv_key)?;
            decoded
        }
    };
    // Placeholder slot: every loop below overwrites key[0] before use.
    let mut key = Vec::with_capacity(key_len + 1);
    key.push(DataValue::Null);
    key.extend_from_slice(key_part);
    for chunk in chunks {
        key[0] = DataValue::Bytes(chunk.clone());
        let key_bytes = idx.encode_key_for_store(&key, SourceSpan::default())?;
        tx.del(&key_bytes)?;
    }
    Ok(())
}

/// Index one base-relation row: evaluate the compiled extractor, min-hash
/// the extracted text (n-grams through `tokenizer`) or list, and write one
/// posting per band plus the inverse row. A `Null` extraction indexes
/// nothing; re-putting first removes the row's previous postings.
///
/// Contracts: the mutation tier calls this after every put on the base
/// relation, in the same transaction. `tokenizer` must be the analyzer
/// built from THIS index's manifest — cache it keyed by the FULL index
/// handle name (`{base}:{idx}`; see the header block). `perms` is
/// `manifest.get_hash_perms()?`, hoisted by the caller so a batch pays the
/// decode once.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lsh_put<T: WriteTx>(
    tx: &mut T,
    tuple: &[DataValue],
    extractor: &Expr,
    tokenizer: &TextAnalyzer,
    base: &RelationHandle,
    idx: &RelationHandle,
    inv_idx: &RelationHandle,
    manifest: &MinHashLshIndexManifest,
    perms: &HashPermutations,
) -> Result<()> {
    let key_len = base.metadata.keys.len();
    if tuple.len() < key_len {
        bail!(IndexRowCorrupt::new(
            &base.name,
            tuple,
            "row shorter than the base relation's key",
        ));
    }
    let inv_key_part = &tuple[..key_len];
    if let Some(found) = inv_idx.get_val_only(tx, inv_key_part)? {
        let chunks = decode_inv_chunks(found, inv_idx, inv_key_part)?;
        lsh_del(tx, tuple, Some(chunks), idx, inv_idx)?;
    }
    let to_index = extractor.eval(tuple)?;
    let min_hash = match &to_index {
        DataValue::Null => return Ok(()),
        DataValue::List(l) => HashValues::new(l.iter().map(element_bytes), perms),
        DataValue::Str(s) => {
            let n_grams = tokenizer.unique_ngrams(s, manifest.n_gram);
            HashValues::new(n_grams.iter().map(|t| ngram_bytes(t)), perms)
        }
        _ => bail!("cannot put value {to_index:?} into an LSH index"),
    };
    let chunks = min_hash.band_chunks(manifest.n_bands, manifest.n_rows_in_band)?;

    // Placeholder slot: every loop below overwrites key[0] before use.
    let mut key = Vec::with_capacity(key_len + 1);
    key.push(DataValue::Null);
    key.extend_from_slice(inv_key_part);
    for chunk in chunks.iter() {
        key[0] = DataValue::Bytes(chunk.clone());
        let key_bytes = idx.encode_key_for_store(&key, SourceSpan::default())?;
        // Postings carry no value; an empty value decodes as a key-only
        // tuple (pinned kernel behavior).
        tx.put(&key_bytes, &[])?;
    }

    let inv_val_part = vec![DataValue::List(
        chunks.into_iter().map(DataValue::Bytes).collect(),
    )];
    let inv_key = inv_idx.encode_key_for_store(inv_key_part, SourceSpan::default())?;
    let inv_val = inv_idx.encode_val_only_for_store(&inv_val_part, SourceSpan::default())?;
    tx.put(&inv_key, &inv_val)?;
    Ok(())
}

/// The parameters of one LSH candidate search; the RA operator tier
/// constructs this from the resolved search atom.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LshSearchParams {
    /// Truncate the candidate set to at most `k` rows. UNRANKED but
    /// CONSISTENT: the `k` kept are the SMALLEST `k` by base key — the same
    /// subset whether or not a filter is present — not similarity-ordered.
    ///
    /// Unranked truncation is a decision, not a leftover. Banding
    /// guarantees a collision PROBABILITY, not an ordering; ranking the
    /// cut by estimated Jaccard would silently drop true near-duplicates
    /// on the noise of a signature estimate while dressing the result as
    /// a ranking the structure cannot honor. Smallest-k-by-key keeps the
    /// operator's contract honest (a candidate SET with a deterministic,
    /// filter-invariant bound), and ranking stays where the engine can do
    /// it exactly: the relational tier, where candidates join like any
    /// relation and an explicit similarity expression orders them.
    pub(crate) k: Option<usize>,
}

/// Candidate near-duplicates of `q`: base rows whose stored signature
/// shares at least one band with `q`'s, in deterministic base-key order,
/// optionally filtered and truncated to `k`.
///
/// This returns a candidate SET, not a similarity ranking (see the module
/// docs). A `Null` query yields no candidates. `perms`/`tokenizer` follow
/// the same caller contracts as [`lsh_put`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn lsh_search(
    cancel: &crate::fixed_rule::CancelFlag,
    tx: &impl ReadTx,
    q: &DataValue,
    manifest: &MinHashLshIndexManifest,
    base: &RelationHandle,
    idx: &RelationHandle,
    params: &LshSearchParams,
    filter_code: &Option<Expr>,
    perms: &HashPermutations,
    tokenizer: &TextAnalyzer,
) -> Result<Vec<Tuple>> {
    let min_hash = match q {
        DataValue::Null => return Ok(vec![]),
        DataValue::List(l) => HashValues::new(l.iter().map(element_bytes), perms),
        DataValue::Str(s) => {
            let n_grams = tokenizer.unique_ngrams(s, manifest.n_gram);
            HashValues::new(n_grams.iter().map(|t| ngram_bytes(t)), perms)
        }
        _ => bail!("cannot search for value {q:?} in an LSH index"),
    };
    let chunks = min_hash.band_chunks(manifest.n_bands, manifest.n_rows_in_band)?;
    // Collect EVERY colliding candidate into a BTreeSet (sorted by base key),
    // then take the smallest `k` by key below. Both the filtered and the
    // unfiltered path select the same way — smallest-k-by-key — so the same
    // query with and without a trivially-true filter returns the SAME subset.
    // (The earlier scan-order early-stop made the unfiltered path return "the
    // first k keys the scan happened to reach", a scan-order-dependent subset;
    // smallest-k-by-key cannot early-stop, since a later band may hold a
    // smaller key.)
    let mut found_tuples: BTreeSet<Tuple> = BTreeSet::new();
    let mut key_prefix = Tuple::with_capacity(1);
    for chunk in chunks {
        key_prefix.clear();
        key_prefix.push(DataValue::Bytes(chunk.clone()));
        for ks in idx.scan_prefix(tx, &key_prefix) {
            cancel.check()?;
            let ks = ks?;
            if ks.is_empty() {
                bail!(IndexRowCorrupt::new(
                    &idx.name,
                    ks.as_slice(),
                    "empty LSH posting"
                ));
            }
            found_tuples.insert(Tuple::from_vec(ks.as_slice()[1..].to_vec()));
        }
    }
    let mut ret = vec![];
    for key in found_tuples {
        let orig_tuple = base.get(tx, key.as_slice())?.ok_or_else(|| {
            miette!(IndexRowCorrupt::new(
                &idx.name,
                key.as_slice(),
                "LSH index references a base row that does not exist",
            ))
        })?;
        if let Some(filter_code) = filter_code
            && !filter_code.eval_pred(&orig_tuple)?
        {
            continue;
        }
        ret.push(orig_tuple);
        if let Some(k) = params.k
            && ret.len() >= k
        {
            break;
        }
    }
    Ok(ret)
}

// ---------------------------------------------------------------------------
// Tests: the engine's executable law.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::relation::KeyspaceKind;

    use crate::data::program::InputRelationHandle;
    use crate::data::symb::Symbol;
    use crate::fixed_rule::CancelFlag;
    use crate::runtime::relation::create_relation;
    use crate::storage::Storage;
    use crate::storage::fjall::new_fjall_storage;

    /// The ratified LE format, byte for byte: known seeds encode to their
    /// little-endian bytes on EVERY platform, and decoding asserts the
    /// same layout.
    #[test]
    fn permutation_bytes_are_little_endian_and_round_trip() {
        let perms = HashPermutations(vec![1, 0x0102_0304, u32::MAX]);
        let bytes = perms.to_bytes();
        assert_eq!(
            bytes,
            vec![
                1, 0, 0, 0, // 1u32, LE
                0x04, 0x03, 0x02, 0x01, // 0x01020304, LE
                0xff, 0xff, 0xff, 0xff, // u32::MAX
            ],
            "explicit little-endian, independent of the host"
        );
        let back = HashPermutations::from_bytes(&bytes).unwrap();
        assert_eq!(back.0, perms.0, "round trip");

        // A length that is not a multiple of 4 is corrupt, not truncated.
        assert!(HashPermutations::from_bytes(&bytes[..7]).is_err());
        // And through the manifest it is the typed corruption error.
        let mut m = manifest_with_perms(vec![1, 2, 3, 4], 4);
        m.perms.0.pop();
        let err = m.get_hash_perms().unwrap_err();
        assert!(err.downcast_ref::<IndexRowCorrupt>().is_some());
    }

    /// Band chunks: LE hash bytes plus an LE u16 band tag, and the
    /// arithmetic is checked instead of sliced blind.
    #[test]
    fn band_chunks_are_little_endian_and_checked() {
        let sig = HashValues(vec![0x0102_0304, 5, 6, 7]);
        let chunks = sig.band_chunks(2, 2).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks[0],
            vec![0x04, 0x03, 0x02, 0x01, 5, 0, 0, 0, /* band tag */ 0, 0]
        );
        assert_eq!(chunks[1], vec![6, 0, 0, 0, 7, 0, 0, 0, /* band tag */ 1, 0]);
        // Mismatched banding is a typed error, not a slice panic — the guard is
        // an EQUALITY, so it must reject BOTH product > len and product < len
        // (a `!=` weakened to `>` would silently accept the short case, dropping
        // signature hashes; a `<` would accept the long case and slice-panic).
        assert!(
            sig.band_chunks(3, 2).is_err(),
            "product 6 > len 4 must error"
        );
        assert!(
            sig.band_chunks(1, 2).is_err(),
            "product 2 < len 4 must error"
        );
        assert!(
            sig.band_chunks(1, 3).is_err(),
            "product 3 < len 4 must error"
        );
    }

    /// `find_optimal_params` is deterministic: same inputs, same bands and
    /// rows — and the result respects its own constraints.
    #[test]
    fn find_optimal_params_is_deterministic() {
        let cases = [
            (0.5, 200, Weights(0.5, 0.5)),
            (0.9, 128, Weights(0.1, 0.9)),
            (0.2, 64, Weights(0.9, 0.1)),
        ];
        for (threshold, num_perm, weights) in cases {
            let a = LshParams::find_optimal_params(threshold, num_perm, &weights);
            let b = LshParams::find_optimal_params(threshold, num_perm, &weights);
            assert_eq!(a, b, "same inputs, same params");
            assert!(a.b >= 1 && a.r >= 1, "non-degenerate: {a:?}");
            assert!(a.b * a.r <= num_perm, "fits the permutation budget: {a:?}");
        }
    }

    /// Encode a set of integers as canonical element bytes, the way `lsh_put`
    /// encodes a `List`'s members.
    fn int_bytes(xs: &[i64]) -> Vec<Vec<u8>> {
        xs.iter()
            .map(|x| element_bytes(&DataValue::from(*x)))
            .collect()
    }

    /// The original's MinHash law, ported: identical sets agree exactly,
    /// divergence lowers the Jaccard estimate, and the permutations
    /// round-trip through their bytes.
    #[test]
    fn minhash_jaccard() {
        let perms = HashPermutations::new(20000, DEFAULT_PERM_SEED);
        let mut m1 = HashValues::new(int_bytes(&[1, 2, 3, 4, 5, 6]).into_iter(), &perms);
        let m2 = HashValues::new(int_bytes(&[4, 3, 2, 1, 5, 6]).into_iter(), &perms);
        assert_eq!(m1.0, m2.0, "same set (any order) ⇒ same signature");
        assert_eq!(m1.jaccard(&m2), 1.0);
        m1.update(int_bytes(&[7, 8, 9]).into_iter(), &perms);
        assert!(m1.jaccard(&m2) < 1.0);
        assert_eq!(
            perms.0,
            HashPermutations::from_bytes(&perms.to_bytes()).unwrap().0
        );
    }

    /// Determinism (obligation): the permutation draw is a pure function of its
    /// seed — two fresh draws with the same seed are byte-identical, a
    /// different seed diverges, and NO OS entropy is involved.
    #[test]
    fn permutations_are_seeded_and_deterministic() {
        let a = HashPermutations::new(64, DEFAULT_PERM_SEED);
        let b = HashPermutations::new(64, DEFAULT_PERM_SEED);
        assert_eq!(a.0, b.0, "same seed ⇒ byte-identical permutations");
        let c = HashPermutations::new(64, DEFAULT_PERM_SEED ^ 1);
        assert_ne!(a.0, c.0, "a different seed draws a different projection");
        assert_eq!(a.0.len(), 64);
    }

    /// Portability + pin (obligation): a fixed input under fixed seeds produces
    /// a fixed signature, BYTE FOR BYTE. Because the elements are hashed via
    /// their memcmp encoding through xxHash32 (not `std::hash::Hash`), this
    /// value is the same on every platform and Rust version; any drift in the
    /// element encoding or the hash fails here, loudly, as the format event it
    /// is. Independently reproducible: encode `Int(1)`, `Int(2)`, `Int(3)` with
    /// the memcmp encoder and xxHash32 under seeds 10/20/30, take the min.
    #[test]
    fn signature_bytes_are_pinned_and_portable() {
        // INDEPENDENT ANCHOR. The element bytes are the canonical encoding
        // of `Int(1..3)`, derived from the format law by hand (the value
        // tag `0x10` = `Tag::Num`, then the 13-byte Num key pinned
        // byte-for-byte by `data::value::number::format_v1_golden_vectors`
        // -- `int(1) = 03 04 39 80 00..`, `int(2) = 03 04 3a 80 00..`,
        // `int(3) = 03 04 3a c0 00..`). If the production encoder drifts
        // from the format, THIS equality fails first, independent of the
        // signature below.
        let hand_derived: Vec<Vec<u8>> = vec![
            vec![0x10, 0x03, 0x04, 0x39, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            vec![0x10, 0x03, 0x04, 0x3a, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            vec![0x10, 0x03, 0x04, 0x3a, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        ];
        assert_eq!(
            int_bytes(&[1, 2, 3]),
            hand_derived,
            "element encoding drifted from the hand-derived canonical format"
        );

        let perms = HashPermutations(vec![10, 20, 30]);
        // The signature is the seeded-xxHash32 MinHash of the FORMAT-VERIFIED
        // element bytes above -- so this pin catches drift in the MinHash
        // algorithm, the encoding drift having already been caught.
        let sig = HashValues::new(hand_derived.into_iter(), &perms);
        assert_eq!(
            sig.0, PINNED_SIGNATURE,
            "the MinHash signature wire value drifted; this is an on-disk \
             format event (the hash algorithm changed), not a test to bump"
        );
    }

    /// Pinned signature of `{Int(1), Int(2), Int(3)}` under seeds `[10, 20, 30]`
    /// (memcmp element bytes → seeded xxHash32 → per-seed minimum). Regenerate
    /// ONLY as a deliberate format migration.
    const PINNED_SIGNATURE: [u32; 3] = [741026819, 588752230, 918467525];

    /// The determinism law at the whole-index level: two databases building the
    /// SAME index from the SAME facts — with the SEEDED permutation draw, not a
    /// rebuild from a shared manifest — produce byte-identical index and inverse
    /// relations. This is what "replicas provably interchangeable" requires.
    #[test]
    fn two_fresh_builds_are_byte_identical() {
        // A manifest whose permutations come from the seeded draw.
        let seeded_manifest = || {
            let perms = HashPermutations::new(64, DEFAULT_PERM_SEED);
            manifest_with_perms(perms.0, 16)
        };
        let rows: Vec<(i64, &str)> = vec![
            (1, "the quick brown fox jumps over the lazy dog"),
            (2, "the quick brown fox jumps over the lazy cat"),
            (3, "entirely unrelated text about database engines"),
            (4, "the quick brown fox jumps over the lazy dog again"),
        ];
        // Compare structure independent of relation-id allocation: strip the
        // 8-byte id prefix (key) and header (value).
        let build_and_dump = || -> Vec<(Vec<u8>, Vec<u8>)> {
            let dir = tempfile::tempdir().unwrap();
            let db = new_fjall_storage(dir.path()).unwrap();
            let m = seeded_manifest();
            let base_meta = StoredRelationMetadata {
                keys: vec![col("k", ColType::Int)],
                non_keys: vec![col("v", ColType::String)],
            };
            let mut tx = db.write_tx().unwrap();
            let base = create_relation(
                &mut tx,
                input_handle("docs", base_meta.clone()),
                KeyspaceKind::Facts,
            )
            .unwrap();
            let idx = create_relation(
                &mut tx,
                input_handle("docs:by_text", lsh_index_metadata(&base_meta)),
                KeyspaceKind::AlgorithmState,
            )
            .unwrap();
            let inv = create_relation(
                &mut tx,
                input_handle("docs:by_text:inv", lsh_inv_index_metadata(&base_meta)),
                KeyspaceKind::AlgorithmState,
            )
            .unwrap();
            let tokenizer = m.tokenizer.build(&m.filters).unwrap();
            let extractor = Expr::Binding {
                var: Symbol::new("v", SourceSpan(0, 0)),
                tuple_pos: Some(1),
            };
            let perms = m.get_hash_perms().unwrap();
            for (k, text) in &rows {
                let row = vec![DataValue::from(*k), DataValue::from(*text)];
                base.put_fact(
                    &mut tx,
                    &row,
                    crate::data::value::ValidityTs::from_raw(0),
                    SourceSpan(0, 0),
                )
                .unwrap();
                lsh_put(
                    &mut tx, &row, &extractor, &tokenizer, &base, &idx, &inv, &m, &perms,
                )
                .unwrap();
            }
            tx.commit().unwrap();
            let rtx = db.read_tx().unwrap();
            let mut out = vec![];
            for rel in [&idx, &inv] {
                let lower = crate::data::value::encode_key_with_suffix(rel.id, &[], &[]);
                let upper = (rel.id.raw() + 1).to_be_bytes();
                for kv in rtx.range_scan(lower.as_bytes(), &upper) {
                    let (k, v) = kv.unwrap();
                    out.push((k[8..].to_vec(), v.get(8..).unwrap_or(&[]).to_vec()));
                }
            }
            out
        };
        let a = build_and_dump();
        let b = build_and_dump();
        assert!(!a.is_empty());
        assert_eq!(
            a, b,
            "two fresh builds of the same index must be byte-identical"
        );
    }

    // -- end to end against a real store ---------------------------------

    fn col(name: &str, coltype: ColType) -> ColumnDef {
        ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType {
                coltype,
                nullable: false,
            },
            default_gen: None,
        }
    }

    fn input_handle(name: &str, metadata: StoredRelationMetadata) -> InputRelationHandle {
        let key_bindings = metadata
            .keys
            .iter()
            .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
            .collect();
        let dep_bindings = metadata
            .non_keys
            .iter()
            .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
            .collect();
        InputRelationHandle {
            name: Symbol::new(name, SourceSpan(0, 0)),
            metadata,
            key_bindings,
            dep_bindings,
            span: SourceSpan(0, 0),
        }
    }

    fn manifest_with_perms(perm_seeds: Vec<u32>, n_bands: usize) -> MinHashLshIndexManifest {
        let params = LshParams {
            b: n_bands,
            r: perm_seeds.len() / n_bands,
        };
        MinHashLshIndexManifest {
            base_relation: SmartString::from("docs"),
            index_name: SmartString::from("by_text"),
            extractor: crate::parse::parse_expressions("v", &std::collections::BTreeMap::new())
                .unwrap(),
            n_gram: 3,
            tokenizer: TokenizerConfig {
                name: SmartString::from("Simple"),
                args: vec![],
            },
            filters: vec![],
            num_perm: perm_seeds.len(),
            n_bands: params.b,
            n_rows_in_band: params.r,
            threshold: 0.5,
            perms: LshPermutationBytes(HashPermutations(perm_seeds).to_bytes()),
        }
    }

    /// Deterministic seeds so the end-to-end test is replayable. 16 bands
    /// of 4 rows: a ~0.75-Jaccard near-duplicate collides with
    /// overwhelming probability (1 - (1 - 0.75^4)^16 ≈ 0.998), and the
    /// fixed seeds make the outcome the SAME on every run.
    fn test_manifest() -> MinHashLshIndexManifest {
        manifest_with_perms((0u32..64).map(|i| i.wrapping_mul(2654435761)).collect(), 16)
    }

    struct Fixture {
        base: RelationHandle,
        idx: RelationHandle,
        inv: RelationHandle,
        manifest: MinHashLshIndexManifest,
        tokenizer: TextAnalyzer,
        extractor: Expr,
    }

    fn setup(db: &impl Storage, rows: &[(i64, &str)]) -> Fixture {
        let manifest = test_manifest();
        let base_meta = StoredRelationMetadata {
            keys: vec![col("k", ColType::Int)],
            non_keys: vec![col("v", ColType::String)],
        };
        let mut tx = db.write_tx().unwrap();
        let base = create_relation(
            &mut tx,
            input_handle("docs", base_meta.clone()),
            KeyspaceKind::Facts,
        )
        .unwrap();
        let idx = create_relation(
            &mut tx,
            input_handle("docs:by_text", lsh_index_metadata(&base_meta)),
            KeyspaceKind::AlgorithmState,
        )
        .unwrap();
        let inv = create_relation(
            &mut tx,
            input_handle("docs:by_text:inv", lsh_inv_index_metadata(&base_meta)),
            KeyspaceKind::AlgorithmState,
        )
        .unwrap();
        let tokenizer = manifest.tokenizer.build(&manifest.filters).unwrap();
        // The compiled extractor: project the text column (position 1).
        let extractor = Expr::Binding {
            var: Symbol::new("v", SourceSpan(0, 0)),
            tuple_pos: Some(1),
        };
        let perms = manifest.get_hash_perms().unwrap();
        for (k, text) in rows {
            let row = vec![DataValue::from(*k), DataValue::from(*text)];
            base.put_fact(
                &mut tx,
                &row,
                crate::data::value::ValidityTs::from_raw(0),
                SourceSpan(0, 0),
            )
            .unwrap();
            lsh_put(
                &mut tx, &row, &extractor, &tokenizer, &base, &idx, &inv, &manifest, &perms,
            )
            .unwrap();
        }
        tx.commit().unwrap();
        Fixture {
            base,
            idx,
            inv,
            manifest,
            tokenizer,
            extractor,
        }
    }

    /// Put, search, and delete on a real store: near-duplicates collide,
    /// unrelated text does not, deletion withdraws the postings.
    #[test]
    fn put_search_del_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let f = setup(
            &db,
            &[
                (1, "the quick brown fox jumps over the lazy dog"),
                (2, "the quick brown fox jumps over the lazy cat"),
                (3, "entirely unrelated text about database engines"),
            ],
        );
        let perms = f.manifest.get_hash_perms().unwrap();

        let rtx = db.read_tx().unwrap();
        let hits = lsh_search(
            &CancelFlag::default(),
            &rtx,
            &DataValue::from("the quick brown fox jumps over the lazy dog"),
            &f.manifest,
            &f.base,
            &f.idx,
            &LshSearchParams { k: None },
            &None,
            &perms,
            &f.tokenizer,
        )
        .unwrap();
        let keys: Vec<i64> = hits.iter().map(|t| t[0].get_int().unwrap()).collect();
        assert!(keys.contains(&1), "the row itself is a candidate");
        assert!(keys.contains(&2), "the near-duplicate collides");
        assert!(!keys.contains(&3), "unrelated text does not collide");
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "deterministic order");

        // A Null query yields nothing; a non-indexable query is an error.
        assert!(
            lsh_search(
                &CancelFlag::default(),
                &rtx,
                &DataValue::Null,
                &f.manifest,
                &f.base,
                &f.idx,
                &LshSearchParams { k: None },
                &None,
                &perms,
                &f.tokenizer,
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            lsh_search(
                &CancelFlag::default(),
                &rtx,
                &DataValue::from(42),
                &f.manifest,
                &f.base,
                &f.idx,
                &LshSearchParams { k: None },
                &None,
                &perms,
                &f.tokenizer,
            )
            .is_err()
        );
        drop(rtx);

        // Delete row 1: it stops being a candidate; its inverse row and
        // postings are gone.
        let mut tx = db.write_tx().unwrap();
        let row1 = vec![
            DataValue::from(1),
            DataValue::from("the quick brown fox jumps over the lazy dog"),
        ];
        lsh_del(&mut tx, &row1, None, &f.idx, &f.inv).unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let hits = lsh_search(
            &CancelFlag::default(),
            &rtx,
            &DataValue::from("the quick brown fox jumps over the lazy dog"),
            &f.manifest,
            &f.base,
            &f.idx,
            &LshSearchParams { k: None },
            &None,
            &perms,
            &f.tokenizer,
        )
        .unwrap();
        let keys: Vec<i64> = hits.iter().map(|t| t[0].get_int().unwrap()).collect();
        assert!(!keys.contains(&1), "deleted row withdrawn");
        assert!(keys.contains(&2), "the near-duplicate remains");
        assert!(f.inv.get_val_only(&rtx, &row1[..1]).unwrap().is_none());
        // Deleting a row that was never indexed is a quiet no-op.
        drop(rtx);
        let mut tx = db.write_tx().unwrap();
        lsh_del(
            &mut tx,
            &[DataValue::from(99), DataValue::from("ghost")],
            None,
            &f.idx,
            &f.inv,
        )
        .unwrap();
    }

    /// A corrupt inverse-index row is a typed error with key context,
    /// never the original's `unreachable!()` panic.
    #[test]
    fn corrupt_inverse_row_is_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let f = setup(&db, &[(1, "some indexed text here")]);
        let perms = f.manifest.get_hash_perms().unwrap();

        // Overwrite the inverse row's value with a wrong-shaped (but
        // well-formed) tuple: a string where the chunk list belongs.
        let mut tx = db.write_tx().unwrap();
        let inv_key = f
            .inv
            .encode_key_for_store(&[DataValue::from(1)], SourceSpan(0, 0))
            .unwrap();
        let bad_val = f
            .inv
            .encode_val_only_for_store(&[DataValue::from("not a chunk list")], SourceSpan(0, 0))
            .unwrap();
        tx.put(&inv_key, &bad_val).unwrap();
        tx.commit().unwrap();

        let mut tx = db.write_tx().unwrap();
        let row = vec![
            DataValue::from(1),
            DataValue::from("some indexed text here"),
        ];
        let err = lsh_del(&mut tx, &row, None, &f.idx, &f.inv).unwrap_err();
        assert!(
            err.downcast_ref::<IndexRowCorrupt>().is_some(),
            "typed corruption, got: {err:?}"
        );
        // Re-put hits the same guard on its remove-first path.
        let err = lsh_put(
            &mut tx,
            &row,
            &f.extractor,
            &f.tokenizer,
            &f.base,
            &f.idx,
            &f.inv,
            &f.manifest,
            &perms,
        )
        .unwrap_err();
        assert!(err.downcast_ref::<IndexRowCorrupt>().is_some());

        // Byte-level garbage in the value is an error too (kernel decode).
        let mut garbage = vec![0u8; 8];
        garbage.push(0xc1);
        tx.put(&inv_key, &garbage).unwrap();
        assert!(lsh_del(&mut tx, &row, None, &f.idx, &f.inv).is_err());
    }

    /// The manifest's wire form round-trips and its bytes are pinned: it
    /// is persisted inside the base relation's catalog row, so any change
    /// is a format migration, not a refactor.
    #[test]
    fn manifest_wire_format_round_trips_and_is_pinned() {
        use serde::Serialize;
        let m = manifest_with_perms(vec![1, 0x0102_0304, 7, 8], 4);
        let mut bytes = vec![];
        m.serialize(&mut rmp_serde::Serializer::new(&mut bytes).with_struct_map())
            .unwrap();
        let decoded: MinHashLshIndexManifest = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded, m, "wire round trip");

        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, PINNED_MANIFEST_HEX,
            "the LSH manifest wire format changed; this is an on-disk \
             format migration, not a refactor"
        );
        assert!(
            rmp_serde::from_slice::<MinHashLshIndexManifest>(&bytes[..bytes.len() / 2]).is_err()
        );
    }

    /// The pinned wire bytes of the canonical manifest above (msgpack,
    /// struct maps). Regenerate ONLY as part of a deliberate format
    /// migration.
    const PINNED_MANIFEST_HEX: &str = "8bad626173655f72656c6174696f6ea4646f6373aa696e6465785f6e616d65a762795f74657874a9657874726163746f7281a742696e64696e6782a376617281a46e616d65a176a97475706c655f706f73c0a66e5f6772616d03a9746f6b656e697a657282a46e616d65a653696d706c65a46172677390a766696c7465727390a86e756d5f7065726d04a76e5f62616e647304ae6e5f726f77735f696e5f62616e6401a97468726573686f6c64cb3fe0000000000000a57065726d73dc001001000000040302010700000008000000";
}
