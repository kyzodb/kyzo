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
 *   [`decode`](HashPermutations::decode) and
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
 * - Law 5: the original's abort-on-impossible arms on decoded inverse-index rows
 *   are the typed [`IndexRowCorrupt`] with the row's key context;
 *   `HashPermutations::decode` refuses a byte length that is not a
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
//! [`crate::project::projection`] build→seal→query machine. Build→seal→query
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

use crate::project::contract::{IndexCorruptReason, IndexRowCorrupt};
use crate::project::projection::{ProjectionKind, RelationIndexSearch};
use crate::project::text::TokenizerConfig;
use crate::project::text::tokenizer::TextAnalyzer;
use crate::session::catalog::RelationHandle;
use crate::store::{ReadTx, WriteTx};
use kyzo_model::SourceSpan;
use kyzo_model::data_value_any;
use kyzo_model::program::expr::Expr;
use kyzo_model::schema::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use kyzo_model::value::{DataValue, Tuple, append_canonical};

// ---------------------------------------------------------------------------
// Projection kind — `K` of the shared build→seal→query machine (#305).
// ---------------------------------------------------------------------------

/// MinHash-LSH as a projection kind: one `K` of
/// [`ProjectionBuilder`](crate::project::projection::ProjectionBuilder) /
/// [`Sealed`](crate::project::projection::Sealed).
///
/// Relation-backed signature maintenance and candidate search ([`lsh_put`],
/// [`Lsh::search_index`]) are the kernel algorithms — not a second
/// build/seal/freshness protocol. Search is owned by
/// [`RelationIndexSearch::search_relation`] (P103); [`Lsh::search_index`]
/// is the UFCS alias into that door.
#[cfg(test)]
use kyzo_model::program::expr::BindingPos;


use crate::exec::stdlib::convert::{f64_to_f32, usize_to_f64};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lsh;

impl ProjectionKind for Lsh {}

// ---------------------------------------------------------------------------
// The manifest: the index's persisted description.
// ---------------------------------------------------------------------------

/// The persisted description of one MinHash-LSH index. Serialized (msgpack,
/// struct maps) as the payload of the base relation's `IndexKind::Lsh`
/// catalog entry — **its wire form is an on-disk format**, pinned by the
/// pinned-bytes test below; changing it is a migration decision.
/// Stored MinHash permutation seed bytes for an LSH index.
///
/// Prefer [`Self::from_perms`] at mint sites. `From<Vec<u8>>` is gone so an
/// arbitrary byte vector is not type-equal to a permutation payload; the
/// length law is re-proven by [`HashPermutations::decode`] on read.
/// Field is private — mint only via [`Self::from_perms`].
#[derive(
    Debug, Clone, PartialEq, Eq, Default, serde_derive::Serialize, serde_derive::Deserialize,
)]
#[repr(transparent)]
pub(crate) struct LshPermutationBytes(Vec<u8>);

const _: () = assert!(std::mem::size_of::<LshPermutationBytes>() == std::mem::size_of::<Vec<u8>>());
const _: () =
    assert!(std::mem::align_of::<LshPermutationBytes>() == std::mem::align_of::<Vec<u8>>());

impl LshPermutationBytes {
    /// Encode live permutation seeds for the catalog payload (always
    /// length-lawful: `4 * n_perms` bytes).
    pub(crate) fn from_perms(perms: &HashPermutations) -> Self {
        Self(perms.to_bytes())
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Test-only: truncate the payload so [`HashPermutations::decode`]
    /// refuses — exercises the corrupt-manifest path without forging via
    /// a public `Vec` field.
    #[cfg(test)]
    pub(crate) fn corrupt_truncate_last_byte_for_test(&mut self) {
        match self.0.pop() {
            value => core::mem::drop(value),
        }
    }
}

impl std::ops::Deref for LshPermutationBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}
impl AsRef<[u8]> for LshPermutationBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct MinHashLshIndexManifest {
    pub(crate) base_relation: SmartString<LazyCompact>,
    pub(crate) index_name: SmartString<LazyCompact>,
    /// The row-extraction expression as a PARSED typed substance (serde
    /// round-trips it through the value plane's `Expr` codec, arity re-proven
    /// on decode); never source text re-parsed at build time.
    pub(crate) extractor: kyzo_model::program::expr::Expr,
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
        HashPermutations::decode(self.perms.as_bytes()).map_err(|reason| {
            miette!(IndexRowCorrupt::new(
                &self.index_name,
                &[],
                IndexCorruptReason::LshPermutations(reason),
            ))
        })
    }
}

/// Mint the index relation's column metadata for an LSH index over `base`:
/// keys `[hash: Bytes, src_*…]`, no value columns.
pub(crate) fn lsh_index_metadata(base: &StoredRelationMetadata) -> StoredRelationMetadata {
    let mut keys = vec![ColumnDef {
        name: SmartString::from("hash"),
        typing: NullableColType::required(ColType::Bytes),
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
            typing: NullableColType::required(ColType::List {
                eltype: Box::new(NullableColType::required(ColType::Bytes)),
                len: None,
            }),
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
    // INVARIANT(splitmix64): modular mix per the splitmix64 contract; wrap is the PRNG.
    *state = (std::num::Wrapping(*state) + std::num::Wrapping(0x9E37_79B9_7F4A_7C15)).0;
    let mut z = std::num::Wrapping(*state);
    z = (z ^ (z >> 30)) * std::num::Wrapping(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)) * std::num::Wrapping(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)).0
}

/// Named refusal when persisted permutation bytes fail the length law.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub(crate) enum LshPermutationDecodeRefused {
    #[error("permutation byte length {len} is not a multiple of 4")]
    #[diagnostic(code(index::lsh::permutation_bytes))]
    LengthNotMultipleOfFour { len: usize },
}

/// Named refusal when band arithmetic or an indexed value shape is illegal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub(crate) enum LshManifestRefused {
    #[error(
        "LSH manifest corrupt: {n_bands} bands of {n_rows_in_band} rows do not fit a signature of {sig_len} hashes"
    )]
    #[diagnostic(code(index::lsh::band_arithmetic))]
    BandArithmeticMismatch {
        n_bands: usize,
        n_rows_in_band: usize,
        sig_len: usize,
    },
    #[error("LSH manifest corrupt: {n_bands} bands exceed the u16 band tag")]
    #[diagnostic(code(index::lsh::too_many_bands))]
    TooManyBands { n_bands: usize },
}

/// Named refusal when a put/search value is not a list or string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub(crate) enum LshValueRefused {
    #[error("cannot put value into an LSH index (need list or string)")]
    #[diagnostic(code(index::lsh::put_unsupported))]
    PutUnsupported,
    #[error("cannot search for value in an LSH index (need list or string)")]
    #[diagnostic(code(index::lsh::search_unsupported))]
    SearchUnsupported,
}

/// The per-index hash seeds ("permutations"): one 32-bit seed per signature
/// position. Inner vec is private — mint via [`Self::new`] / [`Self::decode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HashPermutations(Vec<u32>);

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
            perms.push(match u32::try_from(splitmix64(&mut state) >> 32) { Ok(v) => v, Err(_e) => 0 });
        }
        Self(perms)
    }

    /// Borrow the seed words.
    #[cfg(test)]
    pub(crate) fn as_slice(&self) -> &[u32] {
        &self.0
    }

    /// Seed count (= signature width).
    pub(crate) fn len(&self) -> usize {
        self.0.len()
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
    pub(crate) fn decode(
        bytes: &[u8],
    ) -> std::result::Result<Self, LshPermutationDecodeRefused> {
        if !bytes.len().is_multiple_of(4) {
            return Err(LshPermutationDecodeRefused::LengthNotMultipleOfFour { len: bytes.len() });
        }
        Ok(Self(
            bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        ))
    }

    #[cfg(test)]
    fn from_seeds_for_test(seeds: Vec<u32>) -> Self {
        Self(seeds)
    }
}

/// A MinHash signature: for each permutation seed, the minimum 32-bit hash
/// over the input's elements. Inner vec is private — mint via [`Self::new`] /
/// [`Self::init`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HashValues(Vec<u32>);

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
    #[cfg(test)]
    fn from_hashes_for_test(hashes: Vec<u32>) -> Self {
        Self(hashes)
    }

    #[cfg(test)]
    fn as_slice(&self) -> &[u32] {
        &self.0
    }

    pub(crate) fn new(values: impl Iterator<Item = Vec<u8>>, perms: &HashPermutations) -> Self {
        let mut ret = Self::init(perms);
        ret.update(values, perms);
        ret
    }

    pub(crate) fn init(perms: &HashPermutations) -> Self {
        Self(vec![u32::MAX; perms.len()])
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
                let hash = match u32::try_from(hasher.finish() & 0xFFFF_FFFF) { Ok(v) => v, Err(_e) => 0 };
                self.0[i] = min(self.0[i], hash);
            }
        }
    }

    /// Estimated Jaccard similarity against another signature drawn from
    /// the same permutations. Campaign-proven by [`tests::minhash_jaccard`]
    /// (exact 2/3 ± 0.02 at 20k perms) — the estimator the silent-wrong-
    /// answer audit requires, not a "< 1.0" theater assert. Production
    /// post-filter similarity stays in Datalog; this seat is the MinHash
    /// estimator under that oracle.
    #[cfg(test)]
    pub(crate) fn jaccard(&self, other_minhash: &Self) -> f32 {
        let matches = self
            .0
            .iter()
            .zip(&other_minhash.0)
            .filter(|(left, right)| left == right)
            .count();
        f64_to_f32(usize_to_f64(matches) / usize_to_f64(self.0.len().max(1)))
    }

    /// True iff the two signatures share at least one band of `r` consecutive
    /// hashes under `b` bands (`b * r == sig_len`). The banding collision
    /// event that [`LshParams::false_positive_probability`] /
    /// [`false_negative_probability`] integrate — the ground-truth meter for
    /// the collision-rate campaign.
    #[cfg(test)]
    fn shares_any_band(&self, other: &Self, b: usize, r: usize) -> bool {
        assert_eq!(self.0.len(), other.0.len());
        assert_eq!(b * r, self.0.len());
        (0..b).any(|band| {
            let start = band * r;
            self.0[start..start + r] == other.0[start..start + r]
        })
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
            bail!(LshManifestRefused::BandArithmeticMismatch {
                n_bands,
                n_rows_in_band,
                sig_len: self.0.len(),
            });
        }
        if n_bands > usize::from(u16::MAX) {
            bail!(LshManifestRefused::TooManyBands { n_bands });
        }
        let bytes = self.to_bytes();
        let chunk_size = n_rows_in_band * 4;
        Ok((0..n_bands)
            .map(|i| {
                let mut byte_range = bytes[i * chunk_size..(i + 1) * chunk_size].to_vec();
                byte_range.extend_from_slice(&match u16::try_from(i) { Ok(v) => v, Err(_e) => u16::MAX }.to_le_bytes());
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
        let r_i32 = match i32::try_from(r) { Ok(v) => v, Err(_e) => 0 };
        let b_f = usize_to_f64(b);
        let probability = |s| -> f64 { 1. - f64::powf(1. - f64::powi(s, r_i32), b_f) };
        integrate(probability, 0.0, threshold, ALLOWED_INTEGRATE_ERR).integral
    }

    fn false_negative_probability(threshold: f64, b: usize, r: usize) -> f64 {
        let r_i32 = match i32::try_from(r) {
            Ok(v) => v,
            Err(_e) => 0,
        };
        let b_f = usize_to_f64(b);
        let probability =
            |s| -> f64 { 1. - (1. - f64::powf(1. - f64::powi(s, r_i32), b_f)) };
        integrate(probability, threshold, 1.0, ALLOWED_INTEGRATE_ERR).integral
    }
}

// ---------------------------------------------------------------------------
// The engine's entry points.
// ---------------------------------------------------------------------------

/// Decode an inverse-index row's value (the row's stored band chunks). The
/// original hit an abort-on-impossible arm on any other shape; a wrong shape is
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
                other @ (data_value_any!()) => {
                    match other {
                        value => core::mem::drop(value),
                    }
                    Err(miette!(IndexRowCorrupt::new(
                        &inv_idx.name,
                        key,
                        IndexCorruptReason::LshInvChunkNotBytes,
                    )))
                }
            })
            .collect(),
        other => {
            match other {
                value => core::mem::drop(value),
            }
            bail!(IndexRowCorrupt::new(
                &inv_idx.name,
                key,
                IndexCorruptReason::LshInvNotChunkList,
            ))
        }
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
            IndexCorruptReason::RowShorterThanKey,
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
/// Seats for [`lsh_put`].
pub(crate) struct LshPutSpec<'a> {
    pub(crate) tuple: &'a [DataValue],
    pub(crate) extractor: &'a Expr,
    pub(crate) tokenizer: &'a TextAnalyzer,
    pub(crate) base: &'a RelationHandle,
    pub(crate) idx: &'a RelationHandle,
    pub(crate) inv_idx: &'a RelationHandle,
    pub(crate) manifest: &'a MinHashLshIndexManifest,
    pub(crate) perms: &'a HashPermutations,
}

pub(crate) fn lsh_put<T: WriteTx>(tx: &mut T, spec: LshPutSpec<'_>) -> Result<()> {
    let LshPutSpec {
        tuple,
        extractor,
        tokenizer,
        base,
        idx,
        inv_idx,
        manifest,
        perms,
    } = spec;
    let key_len = base.metadata.keys.len();
    if tuple.len() < key_len {
        bail!(IndexRowCorrupt::new(
            &base.name,
            tuple,
            IndexCorruptReason::RowShorterThanKey,
        ));
    }
    let inv_key_part = &tuple[..key_len];
    if let Some(found) = inv_idx.get_val_only(tx, inv_key_part)? {
        let chunks = decode_inv_chunks(found, inv_idx, inv_key_part)?;
        lsh_del(tx, tuple, Some(chunks), idx, inv_idx)?;
    }
    let to_index = crate::exec::expr::eval_expr(extractor, tuple)?;
    let min_hash = match &to_index {
        DataValue::Null => return Ok(()),
        DataValue::List(l) => HashValues::new(l.iter().map(element_bytes), perms),
        DataValue::Str(s) => {
            let n_grams = tokenizer.unique_ngrams(s, manifest.n_gram);
            HashValues::new(n_grams.iter().map(|t| ngram_bytes(t)), perms)
        }
        data_value_any!() => {
            match to_index {
                value => core::mem::drop(value),
            }
            bail!(LshValueRefused::PutUnsupported)
        }
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

/// One LSH relation-backed candidate search — [`RelationIndexSearch::Request`]
/// for [`Lsh`] (P103).
#[derive(Clone, Copy)]
pub(crate) struct LshSearchRequest<'a> {
    pub(crate) cancel: &'a crate::rules::contract::CancelFlag,
    pub(crate) q: &'a DataValue,
    pub(crate) manifest: &'a MinHashLshIndexManifest,
    pub(crate) base: &'a RelationHandle,
    pub(crate) idx: &'a RelationHandle,
    pub(crate) params: &'a LshSearchParams,
    pub(crate) filter_code: &'a Option<Expr>,
    pub(crate) perms: &'a HashPermutations,
    pub(crate) tokenizer: &'a TextAnalyzer,
}

impl RelationIndexSearch for Lsh {
    type Request<'a> = LshSearchRequest<'a>;

    fn search_relation<Tx: ReadTx>(
        tx: &Tx,
        request: Self::Request<'_>,
    ) -> Result<kyzo_model::value::SearchHits> {
        crate::project::contract::admit_relation_search_hits(lsh_search_body(tx, request)?)
    }
}

/// Candidate near-duplicates of `q`: base rows whose stored signature
/// shares at least one band with `q`'s, in deterministic base-key order,
/// optionally filtered and truncated to `k`.
///
/// This returns a candidate SET, not a similarity ranking (see the module
/// docs). A `Null` query yields no candidates. `perms`/`tokenizer` follow
/// the same caller contracts as [`lsh_put`].
#[cfg(test)]
impl Lsh {
    /// Relation-backed LSH candidate search — UFCS door into
    /// [`RelationIndexSearch::search_relation`] (P103). Formerly the free
    /// function `lsh_search`. Live host dispatch uses the trait method
    /// (`exec/plan/search.rs`); this inherent is the UFCS-friendly alias.
    pub(crate) fn search_index(
        tx: &impl ReadTx,
        request: LshSearchRequest<'_>,
    ) -> Result<kyzo_model::value::SearchHits> {
        Self::search_relation(tx, request)
    }
}

fn lsh_search_body(tx: &impl ReadTx, request: LshSearchRequest<'_>) -> Result<Vec<Tuple>> {
    let LshSearchRequest {
        cancel,
        q,
        manifest,
        base,
        idx,
        params,
        filter_code,
        perms,
        tokenizer,
    } = request;
    let min_hash = match q {
        DataValue::Null => return Ok(vec![]),
        DataValue::List(l) => HashValues::new(l.iter().map(element_bytes), perms),
        DataValue::Str(s) => {
            let n_grams = tokenizer.unique_ngrams(s, manifest.n_gram);
            HashValues::new(n_grams.iter().map(|t| ngram_bytes(t)), perms)
        }
        data_value_any!() => {
            match q {
                value => core::mem::drop(value),
            }
            bail!(LshValueRefused::SearchUnsupported)
        }
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
                    IndexCorruptReason::LshEmptyPosting,
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
                IndexCorruptReason::BaseRowMissing,
            ))
        })?;
        if let Some(filter_code) = filter_code
            && !crate::exec::expr::eval_pred(filter_code, &orig_tuple)?
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
    use crate::session::catalog::KeyspaceKind;

    use crate::rules::contract::CancelFlag;
    use crate::session::catalog::create_relation;
    use crate::store::Storage;
    use crate::store::fjall::new_fjall_storage;
    use kyzo_model::program::InputRelationHandle;
    use kyzo_model::program::symbol::Symbol;
    use miette::{IntoDiagnostic, Result, miette};

    macro_rules! lsh_rows {
        ($cancel:expr, $tx:expr, $q:expr, $manifest:expr, $base:expr, $idx:expr, $params:expr, $filter:expr, $perms:expr, $tokenizer:expr $(,)?) => {{
            crate::project::contract::search_rows(Lsh::search_index(
                $tx,
                LshSearchRequest {
                    cancel: $cancel,
                    q: $q,
                    manifest: $manifest,
                    base: $base,
                    idx: $idx,
                    params: $params,
                    filter_code: $filter,
                    perms: $perms,
                    tokenizer: $tokenizer,
                },
            )?)?
        }};
    }

    /// The ratified LE format, byte for byte: known seeds encode to their
    /// little-endian bytes on EVERY platform, and decoding asserts the
    /// same layout.
    #[test]
    fn permutation_bytes_are_little_endian_and_round_trip() -> Result<()> {
        let perms = HashPermutations::from_seeds_for_test(vec![1, 0x0102_0304, u32::MAX]);
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
        let back = HashPermutations::decode(&bytes)?;
        assert_eq!(back.as_slice(), perms.as_slice(), "round trip");

        // A length that is not a multiple of 4 is corrupt, not truncated.
        assert!(HashPermutations::decode(&bytes[..7]).is_err());
        // And through the manifest it is the typed corruption error.
        let mut m = manifest_with_perms(vec![1, 2, 3, 4], 4)?;
        m.perms.corrupt_truncate_last_byte_for_test();
        let err = m.get_hash_perms().err().ok_or_else(|| miette!("expected error"))?;
        assert!(err.downcast_ref::<IndexRowCorrupt>().is_some());
        Ok(())
    }

    /// Band chunks: LE hash bytes plus an LE u16 band tag, and the
    /// arithmetic is checked instead of sliced blind.
    #[test]
    fn band_chunks_are_little_endian_and_checked() -> Result<()> {
        let sig = HashValues::from_hashes_for_test(vec![0x0102_0304, 5, 6, 7]);
        let chunks = sig.band_chunks(2, 2)?;
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
        Ok(())
    }

    /// `find_optimal_params` is deterministic: same inputs, same bands and
    /// rows — and the result respects its own constraints.
    #[test]
    fn find_optimal_params_is_deterministic() -> Result<()> {
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
        Ok(())
    }

    /// Encode a set of integers as canonical element bytes, the way `lsh_put`
    /// encodes a `List`'s members.
    fn int_bytes(xs: &[i64]) -> Vec<Vec<u8>> {
        xs.iter()
            .map(|x| element_bytes(&DataValue::from(*x)))
            .collect()
    }

    /// MinHash estimator pin: identical sets agree exactly; after extending
    /// `{1..6}` with `{7,8,9}` the true Jaccard against `{1..6}` is exactly
    /// 6/9 = 2/3, and 20k permutations must estimate that within ±0.02.
    /// A "< 1.0" assert would green on any drift; this pin fails if the
    /// estimator (or the element hash) is silently wrong.
    #[test]
    fn minhash_jaccard() -> Result<()> {
        let perms = HashPermutations::new(20000, DEFAULT_PERM_SEED);
        let mut m1 = HashValues::new(int_bytes(&[1, 2, 3, 4, 5, 6]).into_iter(), &perms);
        let m2 = HashValues::new(int_bytes(&[4, 3, 2, 1, 5, 6]).into_iter(), &perms);
        assert_eq!(
            m1.as_slice(),
            m2.as_slice(),
            "same set (any order) ⇒ same signature"
        );
        assert_eq!(m1.jaccard(&m2), 1.0);
        // |{1..6} ∪ {7,8,9}| = 9, |∩ with {1..6}| = 6 → Jaccard = 2/3.
        m1.update(int_bytes(&[7, 8, 9]).into_iter(), &perms);
        let est = f64::from(m1.jaccard(&m2));
        let truth = 2.0 / 3.0;
        assert!(
            (est - truth).abs() <= 0.02,
            "minhash Jaccard estimate {est} must pin 2/3 ± 0.02 (got Δ={})",
            (est - truth).abs()
        );
        assert_eq!(
            perms.as_slice(),
            HashPermutations::decode(&perms.to_bytes())?
                .as_slice()
        );
        Ok(())
    }

    /// Theoretical band-collision probability for Jaccard similarity `s`
    /// under `b` bands of `r` rows: `1 - (1 - s^r)^b`. The integrand of
    /// [`LshParams::false_positive_probability`] /
    /// [`false_negative_probability`].
    fn predicted_collision_rate(s: f64, b: usize, r: usize) -> f64 {
        1.0 - (1.0 - s.powi(match i32::try_from(r) { Ok(v) => v, Err(_e) => 0 }))
            .powi(match i32::try_from(b) { Ok(v) => v, Err(_e) => 0 })
    }

    /// Two equal-sized integer sets with exact Jaccard `inter / (2n - inter)`.
    fn jaccard_pair(n: usize, inter: usize, offset: i64) -> (Vec<i64>, Vec<i64>, f64) {
        assert!(inter <= n);
        let n_i = match i64::try_from(n) { Ok(v) => v, Err(_e) => 0 };
        let inter_i = match i64::try_from(inter) { Ok(v) => v, Err(_e) => 0 };
        let a: Vec<i64> = (0..n_i).map(|i| offset + i).collect();
        let mut b: Vec<i64> = (0..inter_i).map(|i| offset + i).collect();
        b.extend((0..(n_i - inter_i)).map(|i| offset + n_i + i));
        let union = 2 * n - inter;
        let s = usize_to_f64(inter) / usize_to_f64(union);
        (a, b, s)
    }

    /// Empirical band-collision rate over `trials` independent pair/seed
    /// draws at a controlled Jaccard, vs the closed-form prediction.
    fn empirical_collision_rate(
        n: usize,
        inter: usize,
        b: usize,
        r: usize,
        trials: usize,
        seed0: u64,
    ) -> (f64, f64) {
        let num_perm = b * r;
        let mut hits = 0usize;
        let mut s_sum = 0.0;
        for t in 0..trials {
            let (a, bset, s) = jaccard_pair(n, inter, match i64::try_from(t) { Ok(v) => v, Err(_e) => 0 } * 10_000);
            s_sum += s;
            // INVARIANT(trial_seed_mix): trial index mixes into seed0 by modular add; wrap is intentional diffusion.
            let perms = HashPermutations::new(
                num_perm,
                (std::num::Wrapping(seed0)
                    + std::num::Wrapping(match u64::try_from(t) {
                        Ok(v) => v,
                        Err(_e) => 0,
                    }))
                .0,
            );
            let ha = HashValues::new(int_bytes(&a).into_iter(), &perms);
            let hb = HashValues::new(int_bytes(&bset).into_iter(), &perms);
            if ha.shares_any_band(&hb, b, r) {
                hits += 1;
            }
        }
        let empirical = usize_to_f64(hits) / usize_to_f64(trials);
        let s_mean = s_sum / usize_to_f64(trials);
        (empirical, s_mean)
    }

    /// AUDIT-lsh-oracle: the FP/FN integrands of `find_optimal_params` are
    /// the band-collision curve `1-(1-s^r)^b`. At controlled Jaccard bands,
    /// the empirical collision rate under real MinHash signatures must match
    /// that prediction within ±10 percentage points — ground truth the
    /// numeric integration never had.
    #[test]
    fn lsh_collision_rate_matches_prediction_within_10pp() -> Result<()> {
        // Params a real create would pick near threshold 0.5 / 128 perms.
        let params = LshParams::find_optimal_params(0.5, 128, &Weights(0.5, 0.5));
        assert!(params.b >= 1 && params.r >= 1);
        let (b, r) = (params.b, params.r);
        // Controlled Jaccard bands via equal-sized sets (n=80).
        // inter → s = inter/(160-inter): 20→0.143, 36→0.290, 48→0.429,
        // 55→0.524, 64→0.667, 72→0.818.
        let bands: &[(usize, &str)] = &[
            (20, "low"),
            (36, "mid-low"),
            (48, "near-threshold-below"),
            (55, "near-threshold-above"),
            (64, "high"),
            (72, "very-high"),
        ];
        const TRIALS: usize = 400;
        const TOL_PP: f64 = 0.10; // ±10 percentage points
        for &(inter, label) in bands {
            let (emp, s) = empirical_collision_rate(80, inter, b, r, TRIALS, DEFAULT_PERM_SEED);
            let pred = predicted_collision_rate(s, b, r);
            let delta = (emp - pred).abs();
            assert!(
                delta <= TOL_PP,
                "band {label}: s={s:.3} b={b} r={r}: empirical={emp:.3} \
                 predicted={pred:.3} |Δ|={delta:.3} exceeds ±{TOL_PP} ({TRIALS} trials)"
            );
        }
        Ok(())
    }

    /// Threshold-boundary discrimination: under `find_optimal_params` at
    /// threshold 0.5, pairs at ~0.45 Jaccard must collide LESS often than
    /// pairs at ~0.55 — the curve the FP/FN weights optimize against must
    /// actually separate the two sides of the declared threshold.
    #[test]
    fn lsh_threshold_boundary_pairs_discriminate() -> Result<()> {
        let params = LshParams::find_optimal_params(0.5, 128, &Weights(0.5, 0.5));
        let (b, r) = (params.b, params.r);
        // n=100: inter=62 → s≈0.449; inter=71 → s≈0.550.
        let (emp_lo, s_lo) = empirical_collision_rate(100, 62, b, r, 500, DEFAULT_PERM_SEED ^ 0x45);
        let (emp_hi, s_hi) = empirical_collision_rate(100, 71, b, r, 500, DEFAULT_PERM_SEED ^ 0x55);
        assert!(
            (s_lo - 0.45).abs() < 0.01,
            "low band must be ~0.45, got {s_lo}"
        );
        assert!(
            (s_hi - 0.55).abs() < 0.01,
            "high band must be ~0.55, got {s_hi}"
        );
        let pred_lo = predicted_collision_rate(s_lo, b, r);
        let pred_hi = predicted_collision_rate(s_hi, b, r);
        assert!(
            pred_hi > pred_lo,
            "prediction itself must rise across threshold: {pred_lo} vs {pred_hi}"
        );
        assert!(
            emp_hi > emp_lo,
            "empirical collision must discriminate ~0.45 vs ~0.55: \
             P(s≈{s_lo:.3})={emp_lo:.3} vs P(s≈{s_hi:.3})={emp_hi:.3} \
             (predicted {pred_lo:.3} vs {pred_hi:.3}; b={b} r={r})"
        );
        // Each side also stays within the campaign tolerance of its prediction.
        assert!(
            (emp_lo - pred_lo).abs() <= 0.10,
            "lo band off prediction: emp={emp_lo} pred={pred_lo}"
        );
        assert!(
            (emp_hi - pred_hi).abs() <= 0.10,
            "hi band off prediction: emp={emp_hi} pred={pred_hi}"
        );
        Ok(())
    }

    /// Determinism (obligation): the permutation draw is a pure function of its
    /// seed — two fresh draws with the same seed are byte-identical, a
    /// different seed diverges, and NO OS entropy is involved.
    #[test]
    fn permutations_are_seeded_and_deterministic() -> Result<()> {
        let a = HashPermutations::new(64, DEFAULT_PERM_SEED);
        let b = HashPermutations::new(64, DEFAULT_PERM_SEED);
        assert_eq!(
            a.as_slice(),
            b.as_slice(),
            "same seed ⇒ byte-identical permutations"
        );
        let c = HashPermutations::new(64, DEFAULT_PERM_SEED ^ 1);
        assert_ne!(
            a.as_slice(),
            c.as_slice(),
            "a different seed draws a different projection"
        );
        assert_eq!(a.len(), 64);
        Ok(())
    }

    /// Portability + pin (obligation): a fixed input under fixed seeds produces
    /// a fixed signature, BYTE FOR BYTE. Because the elements are hashed via
    /// their memcmp encoding through xxHash32 (not `std::hash::Hash`), this
    /// value is the same on every platform and Rust version; any drift in the
    /// element encoding or the hash fails here, loudly, as the format event it
    /// is. Independently reproducible: encode `Int(1)`, `Int(2)`, `Int(3)` with
    /// the memcmp encoder and xxHash32 under seeds 10/20/30, take the min.
    #[test]
    fn signature_bytes_are_pinned_and_portable() -> Result<()> {
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

        let perms = HashPermutations::from_seeds_for_test(vec![10, 20, 30]);
        // The signature is the seeded-xxHash32 MinHash of the FORMAT-VERIFIED
        // element bytes above -- so this pin catches drift in the MinHash
        // algorithm, the encoding drift having already been caught.
        let sig = HashValues::new(hand_derived.into_iter(), &perms);
        assert_eq!(
            sig.as_slice(),
            &PINNED_SIGNATURE,
            "the MinHash signature wire value drifted; this is an on-disk \
             format event (the hash algorithm changed), not a test to bump"
        );
        Ok(())
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
    fn two_fresh_builds_are_byte_identical() -> Result<()> {
        // A manifest whose permutations come from the seeded draw.
        let seeded_manifest = || -> Result<MinHashLshIndexManifest> {
            let perms = HashPermutations::new(64, DEFAULT_PERM_SEED);
            manifest_with_perms(perms.as_slice().to_vec(), 16)
        };
        let rows: Vec<(i64, &str)> = vec![
            (1, "the quick brown fox jumps over the lazy dog"),
            (2, "the quick brown fox jumps over the lazy cat"),
            (3, "entirely unrelated text about database engines"),
            (4, "the quick brown fox jumps over the lazy dog again"),
        ];
        // Compare structure independent of relation-id allocation: strip the
        // 8-byte id prefix (key) and header (value).
        let build_and_dump = || -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
            let dir = tempfile::tempdir().into_diagnostic()?;
            let db = new_fjall_storage(dir.path())?;
            let m = seeded_manifest()?;
            let base_meta = StoredRelationMetadata {
                keys: vec![col("k", ColType::Int)],
                non_keys: vec![col("v", ColType::String)],
            };
            let mut tx = db.write_tx()?;
            let base = create_relation(
                &mut tx,
                input_handle("docs", base_meta.clone()),
                KeyspaceKind::Facts,
            )?;
            let idx = create_relation(
                &mut tx,
                input_handle("docs:by_text", lsh_index_metadata(&base_meta)),
                KeyspaceKind::AlgorithmState,
            )?;
            let inv = create_relation(
                &mut tx,
                input_handle("docs:by_text:inv", lsh_inv_index_metadata(&base_meta)),
                KeyspaceKind::AlgorithmState,
            )?;
            let tokenizer = m.tokenizer.build(&m.filters)?;
            let extractor = Expr::Binding {
                var: Symbol::new("v", SourceSpan(0, 0)),
                tuple_pos: BindingPos::Resolved(1),
            };
            let perms = m.get_hash_perms()?;
            for (k, text) in &rows {
                let row = vec![DataValue::from(*k), DataValue::from(*text)];
                base.put_fact(
                    &mut tx,
                    &row,
                    kyzo_model::value::ValidityTs::of_micros(0),
                    SourceSpan(0, 0),
                )?;
                lsh_put(&mut tx, LshPutSpec { tuple: &row, extractor: &extractor, tokenizer: &tokenizer, base: &base, idx: &idx, inv_idx: &inv, manifest: &m, perms: &perms })?;
            }
            tx.commit().map_err(|e| miette!("{e}"))?;
            let rtx = db.read_tx()?;
            let mut out = vec![];
            for rel in [&idx, &inv] {
                let lower = kyzo_model::value::encode_key_with_suffix(rel.id, &[], &[]);
                let upper = (rel.id.raw() + 1).to_be_bytes();
                for kv in rtx.range_scan(lower.as_bytes(), &upper) {
                    let (k, v) = kv.map_err(|e| miette!("{e}"))?;
                    let val_tail = match v.get(8..) {
                        Some(t) => t,
                        None => &[],
                    };
                    out.push((k[8..].to_vec(), val_tail.to_vec()));
                }
            }
            Ok(out)
        };
        let a = build_and_dump()?;
        let b = build_and_dump()?;
        assert!(!a.is_empty());
        assert_eq!(
            a, b,
            "two fresh builds of the same index must be byte-identical"
        );
        Ok(())
    }

    // -- end to end against a real store ---------------------------------

    fn col(name: &str, coltype: ColType) -> ColumnDef {
        ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType::required(coltype),
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

    fn manifest_with_perms(perm_seeds: Vec<u32>, n_bands: usize) -> Result<MinHashLshIndexManifest> {
        let params = LshParams {
            b: n_bands,
            r: perm_seeds.len() / n_bands,
        };
        Ok(MinHashLshIndexManifest {
            base_relation: SmartString::from("docs"),
            index_name: SmartString::from("by_text"),
            extractor: crate::parse::parse_expressions("v", &std::collections::BTreeMap::new())?,
            n_gram: 3,
            tokenizer: TokenizerConfig::admit("Simple", vec![])?,
            filters: vec![],
            num_perm: perm_seeds.len(),
            n_bands: params.b,
            n_rows_in_band: params.r,
            threshold: 0.5,
            perms: LshPermutationBytes::from_perms(&HashPermutations::from_seeds_for_test(
                perm_seeds,
            )),
        })
    }

    /// Deterministic seeds so the end-to-end test is replayable. 16 bands
    /// of 4 rows: a ~0.75-Jaccard near-duplicate collides with
    /// overwhelming probability (1 - (1 - 0.75^4)^16 ≈ 0.998), and the
    /// fixed seeds make the outcome the SAME on every run.
    fn test_manifest() -> Result<MinHashLshIndexManifest> {
        // INVARIANT(test_perm_mix): fixture permutation tags use modular golden-ratio mix.
        manifest_with_perms(
            (0u32..64)
                .map(|i| (std::num::Wrapping(i) * std::num::Wrapping(2654435761)).0)
                .collect(),
            16,
        )
    }

    struct Fixture {
        base: RelationHandle,
        idx: RelationHandle,
        inv: RelationHandle,
        manifest: MinHashLshIndexManifest,
        tokenizer: TextAnalyzer,
        extractor: Expr,
    }

    fn setup(db: &impl Storage, rows: &[(i64, &str)]) -> Result<Fixture> {
        let manifest = test_manifest()?;
        let base_meta = StoredRelationMetadata {
            keys: vec![col("k", ColType::Int)],
            non_keys: vec![col("v", ColType::String)],
        };
        let mut tx = db.write_tx()?;
        let base = create_relation(
            &mut tx,
            input_handle("docs", base_meta.clone()),
            KeyspaceKind::Facts,
        )?;
        let idx = create_relation(
            &mut tx,
            input_handle("docs:by_text", lsh_index_metadata(&base_meta)),
            KeyspaceKind::AlgorithmState,
        )?;
        let inv = create_relation(
            &mut tx,
            input_handle("docs:by_text:inv", lsh_inv_index_metadata(&base_meta)),
            KeyspaceKind::AlgorithmState,
        )?;
        let tokenizer = manifest.tokenizer.build(&manifest.filters)?;
        // The compiled extractor: project the text column (position 1).
        let extractor = Expr::Binding {
            var: Symbol::new("v", SourceSpan(0, 0)),
            tuple_pos: BindingPos::Resolved(1),
        };
        let perms = manifest.get_hash_perms()?;
        for (k, text) in rows {
            let row = vec![DataValue::from(*k), DataValue::from(*text)];
            base.put_fact(
                &mut tx,
                &row,
                kyzo_model::value::ValidityTs::of_micros(0),
                SourceSpan(0, 0),
            )?;
            lsh_put(&mut tx, LshPutSpec { tuple: &row, extractor: &extractor, tokenizer: &tokenizer, base: &base, idx: &idx, inv_idx: &inv, manifest: &manifest, perms: &perms })?;
        }
        tx.commit().map_err(|e| miette!("{e}"))?;
        Ok(Fixture {
            base,
            idx,
            inv,
            manifest,
            tokenizer,
            extractor,
        })
    }

    /// Put, search, and delete on a real store: near-duplicates collide,
    /// unrelated text does not, deletion withdraws the postings.
    #[test]
    fn put_search_del_round_trip() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let f = setup(
            &db,
            &[
                (1, "the quick brown fox jumps over the lazy dog"),
                (2, "the quick brown fox jumps over the lazy cat"),
                (3, "entirely unrelated text about database engines"),
            ],
        )?;
        let perms = f.manifest.get_hash_perms()?;

        let rtx = db.read_tx()?;
        let hits = lsh_rows!(
            &CancelFlag::inert(),
            &rtx,
            &DataValue::from("the quick brown fox jumps over the lazy dog"),
            &f.manifest,
            &f.base,
            &f.idx,
            &LshSearchParams { k: None },
            &None,
            &perms,
            &f.tokenizer,
        );
        let mut keys = Vec::new();
        for t in &hits {
            keys.push(t[0].get_int().ok_or_else(|| miette!("expected int"))?);
        }
        assert!(keys.contains(&1), "the row itself is a candidate");
        assert!(keys.contains(&2), "the near-duplicate collides");
        assert!(!keys.contains(&3), "unrelated text does not collide");
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "deterministic order");

        // A Null query yields nothing; a non-indexable query is an error.
        assert!(
            Lsh::search_index(&rtx, LshSearchRequest { cancel: &CancelFlag::inert(), q: &DataValue::Null, manifest: &f.manifest, base: &f.base, idx: &f.idx, params: &LshSearchParams { k: None }, filter_code: &None, perms: &perms, tokenizer: &f.tokenizer })?
            .is_empty()
        );
        assert!(
            Lsh::search_index(&rtx, LshSearchRequest { cancel: &CancelFlag::inert(), q: &DataValue::from(42), manifest: &f.manifest, base: &f.base, idx: &f.idx, params: &LshSearchParams { k: None }, filter_code: &None, perms: &perms, tokenizer: &f.tokenizer })
            .is_err()
        );
        drop(rtx);

        // Delete row 1: it stops being a candidate; its inverse row and
        // postings are gone.
        let mut tx = db.write_tx()?;
        let row1 = vec![
            DataValue::from(1),
            DataValue::from("the quick brown fox jumps over the lazy dog"),
        ];
        lsh_del(&mut tx, &row1, None, &f.idx, &f.inv)?;
        tx.commit().map_err(|e| miette!("{e}"))?;

        let rtx = db.read_tx()?;
        let hits = lsh_rows!(
            &CancelFlag::inert(),
            &rtx,
            &DataValue::from("the quick brown fox jumps over the lazy dog"),
            &f.manifest,
            &f.base,
            &f.idx,
            &LshSearchParams { k: None },
            &None,
            &perms,
            &f.tokenizer,
        );
        let mut keys = Vec::new();
        for t in &hits {
            keys.push(t[0].get_int().ok_or_else(|| miette!("expected int"))?);
        }
        assert!(!keys.contains(&1), "deleted row withdrawn");
        assert!(keys.contains(&2), "the near-duplicate remains");
        assert!(f.inv.get_val_only(&rtx, &row1[..1])?.is_none());
        // Deleting a row that was never indexed is a quiet no-op.
        drop(rtx);
        let mut tx = db.write_tx()?;
        lsh_del(
            &mut tx,
            &[DataValue::from(99), DataValue::from("ghost")],
            None,
            &f.idx,
            &f.inv,
        )?;
        match tx.abort() {
            crate::store::tx::Aborted => {}
        }
        Ok(())
    }

    /// A corrupt inverse-index row is a typed error with key context,
    /// never the original's abort-on-impossible panic.
    #[test]
    fn corrupt_inverse_row_is_typed_error() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let f = setup(&db, &[(1, "some indexed text here")])?;
        let perms = f.manifest.get_hash_perms()?;

        // Overwrite the inverse row's value with a wrong-shaped (but
        // well-formed) tuple: a string where the chunk list belongs.
        let mut tx = db.write_tx()?;
        let inv_key = f
            .inv
            .encode_key_for_store(&[DataValue::from(1)], SourceSpan(0, 0))?;
        let bad_val = f
            .inv
            .encode_val_only_for_store(&[DataValue::from("not a chunk list")], SourceSpan(0, 0))?;
        tx.put(&inv_key, &bad_val).map_err(|e| miette!("{e}"))?;
        tx.commit().map_err(|e| miette!("{e}"))?;

        let mut tx = db.write_tx()?;
        let row = vec![
            DataValue::from(1),
            DataValue::from("some indexed text here"),
        ];
        let err = lsh_del(&mut tx, &row, None, &f.idx, &f.inv).err().ok_or_else(|| miette!("expected error"))?;
        assert!(
            err.downcast_ref::<IndexRowCorrupt>().is_some(),
            "typed corruption, got: {err:?}"
        );
        // Re-put hits the same guard on its remove-first path.
        let err = lsh_put(&mut tx, LshPutSpec { tuple: &row, extractor: &f.extractor, tokenizer: &f.tokenizer, base: &f.base, idx: &f.idx, inv_idx: &f.inv, manifest: &f.manifest, perms: &perms })
        .err().ok_or_else(|| miette!("expected error"))?;
        assert!(err.downcast_ref::<IndexRowCorrupt>().is_some());

        // Byte-level garbage in the value is an error too (kernel decode).
        let mut garbage = vec![0u8; 8];
        garbage.push(0xc1);
        tx.put(&inv_key, &garbage).map_err(|e| miette!("{e}"))?;
        assert!(lsh_del(&mut tx, &row, None, &f.idx, &f.inv).is_err());
        match tx.abort() {
            crate::store::tx::Aborted => {}
        }
        Ok(())
    }

    /// The manifest's wire form round-trips and its bytes are pinned: it
    /// is persisted inside the base relation's catalog row, so any change
    /// is a format migration, not a refactor.
    #[test]
    fn manifest_wire_format_round_trips_and_is_pinned() -> Result<()> {
        use serde::Serialize;
        let m = manifest_with_perms(vec![1, 0x0102_0304, 7, 8], 4)?;
        let mut bytes = vec![];
        m.serialize(&mut rmp_serde::Serializer::new(&mut bytes).with_struct_map())
            .map_err(|e| miette!("{e}"))?;
        let decoded: MinHashLshIndexManifest = rmp_serde::from_slice(&bytes).map_err(|e| miette!("{e}"))?;
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
        Ok(())
    }

    /// The pinned wire bytes of the canonical manifest above (msgpack,
    /// struct maps). Regenerate ONLY as part of a deliberate format
    /// migration.
    const PINNED_MANIFEST_HEX: &str = "8bad626173655f72656c6174696f6ea4646f6373aa696e6465785f6e616d65a762795f74657874a9657874726163746f7281a742696e64696e6782a376617281a46e616d65a176a97475706c655f706f73aa556e7265736f6c766564a66e5f6772616d03a9746f6b656e697a657282a46e616d65a653696d706c65a46172677390a766696c7465727390a86e756d5f7065726d04a76e5f62616e647304ae6e5f726f77735f696e5f62616e6401a97468726573686f6c64cb3fe0000000000000a57065726d73dc001001000000040302010700000008000000";
}
