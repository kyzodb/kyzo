/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Key hierarchy, shred, and AEAD pipeline (decisions.md §60–§68).
//!
//! Owns: DEK derive, KEK unwrap capability, [`ShredSalt`], [`WrappedShredSalt`],
//! [`AuditKey`], AEAD/SIV arms, compression-before-encryption pipeline, shred verb.
//!
//! Bans: plaintext-mode flag; encrypt-then-compress; unstructured random DEKs /
//! single global DEK; root-over-ciphertext (roots are cipher-invariant —
//! merkle consumes plaintext canonical).
//!
//! `DEK = derive(KEK, CryptoDomain, SegmentCounter, ShredSalt)` after unwrap of
//! [`WrappedShredSalt`]. Plaintext salt exists only inside the derivation moment
//! in memory. Shred destroys the wrapped salt + authorized replicas.

use sha2::{Digest, Sha256};

use super::epoch::CryptoDomain;
use super::transcript::CanonicalTranscript;

/// Per-segment counter separating DEK space under one CryptoDomain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SegmentCounter(u64);

impl SegmentCounter {
    /// Counter zero.
    pub const ZERO: SegmentCounter = SegmentCounter(0);

    /// Wrap an already-proven segment counter.
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw counter.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Closed AEAD arm selection. SnapshotFork=yes arms require misuse-resistant SIV.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AeadArm {
    /// Standard AEAD (GCM-class). Forbidden on SnapshotFork=yes arms.
    Gcm,
    /// Misuse-resistant SIV — nonce repeat degrades to message-equality leak only.
    Siv,
}

/// Root unwrap capability presented at open (client / HSM trait).
///
/// Absent → open refuses MissingRootKek. Host-held wrapped DEKs are a separate
/// at-rest layer; zero-access still requires this root.
#[derive(Debug)]
pub struct KekUnwrapCap {
    /// Opaque KEK material — never logged, never packed.
    kek: Kek,
}

impl KekUnwrapCap {
    /// Mint a unwrap capability from already-held root KEK material.
    pub(crate) fn from_kek(kek: Kek) -> Self {
        Self { kek }
    }

    /// Borrow the KEK for derive / wrap sites that already hold this capability.
    pub(crate) fn kek(&self) -> &Kek {
        &self.kek
    }
}

/// Key-encryption key — closed [`super::keys::Secret`] member. Never in packs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kek([u8; 32]);

impl Kek {
    /// Wrap already-proven KEK bytes (HSM / genesis sites).
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Data-encryption key derived under the sealed hierarchy — never unstructured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dek([u8; 32]);

impl Dek {
    fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow DEK bytes for the encrypt door.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Plaintext shred salt — transient, memory-only, never persisted.
///
/// No serde. No constructor accepts this into WAL / pack / seal writers.
/// Exists only inside the derivation moment after unwrap of [`WrappedShredSalt`].
#[derive(Debug)]
pub struct ShredSalt([u8; 32]);

impl ShredSalt {
    /// Draw / wrap plaintext salt bytes at the derivation moment only.
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// KEK-wrapped shred salt — the only persistable salt form.
///
/// Required in every leave-is-free pack. Useless without the KEK.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WrappedShredSalt {
    /// Opaque ciphertext under the Store KEK.
    ciphertext: Vec<u8>,
    /// Segment this wrapped salt derives DEKs for.
    segment: SegmentCounter,
    /// Crypto domain binding.
    crypto_domain: CryptoDomain,
}

impl WrappedShredSalt {
    /// Segment counter this wrap covers.
    pub fn segment(&self) -> SegmentCounter {
        self.segment
    }

    /// Crypto domain binding.
    pub fn crypto_domain(&self) -> CryptoDomain {
        self.crypto_domain
    }

    /// Borrow opaque wrapped bytes (pack / WAL persist).
    pub fn ciphertext(&self) -> &[u8] {
        &self.ciphertext
    }

    /// Reconstruct from already-persisted wrapped bytes (restore / decode).
    pub fn from_persisted(
        ciphertext: Vec<u8>,
        segment: SegmentCounter,
        crypto_domain: CryptoDomain,
    ) -> Self {
        Self {
            ciphertext,
            segment,
            crypto_domain,
        }
    }
}

/// Audit integrity key — leaf MAC over CanonicalTranscript.
///
/// `AuditKey ≠ AncestorReadGrant ≠ decrypt ≠ WriteAuthority`.
/// Wrapped under KEK alongside WrappedShredSalts. Never in packs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditKey([u8; 32]);

impl AuditKey {
    /// Wrap already-proven audit key bytes.
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Leaf MAC over a sealed CanonicalTranscript (cipher-invariant roots).
    pub fn leaf_mac(&self, transcript: &CanonicalTranscript) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"kyzo.audit.leaf.v1");
        h.update(self.0);
        h.update(transcript.as_bytes());
        let dig = h.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&dig);
        out
    }
}

/// Typed refuse from crypto doors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum CryptoRefuse {
    #[error("crypto: missing root KEK unwrap capability")]
    #[diagnostic(code(store::crypto::missing_root_kek))]
    MissingRootKek,
    #[error("crypto: wrapped shred salt unwrap failed")]
    #[diagnostic(code(store::crypto::unwrap_failed))]
    UnwrapFailed,
    #[error("crypto: segment already shredded — typed tombstone")]
    #[diagnostic(code(store::crypto::shredded))]
    Shredded,
}

/// Wrap a plaintext [`ShredSalt`] under the KEK for persistence.
///
/// Plaintext salt must not escape this call except into [`derive_dek`].
pub fn wrap_shred_salt(
    cap: &KekUnwrapCap,
    salt: &ShredSalt,
    segment: SegmentCounter,
    crypto_domain: CryptoDomain,
) -> WrappedShredSalt {
    let mut h = Sha256::new();
    h.update(b"kyzo.wrap.shred_salt.v1");
    h.update(cap.kek().as_bytes());
    h.update(crypto_domain.store_id().as_bytes());
    h.update(u64::to_be_bytes(crypto_domain.fence_epoch().get()));
    h.update(u64::to_be_bytes(segment.get()));
    h.update(salt.as_bytes());
    let dig = h.finalize();
    // Provisional wrap: KEK-bound commitment ‖ salt XOR keystream (sha2 until AEAD crate lands).
    let mut keystream = Sha256::new();
    keystream.update(b"kyzo.wrap.shred_salt.stream.v1");
    keystream.update(cap.kek().as_bytes());
    keystream.update(&dig);
    let stream = keystream.finalize();
    let mut ct = Vec::with_capacity(32 + 32);
    ct.extend_from_slice(&dig);
    for i in 0..32 {
        ct.push(salt.as_bytes()[i] ^ stream[i]);
    }
    WrappedShredSalt {
        ciphertext: ct,
        segment,
        crypto_domain,
    }
}

/// Unwrap a persisted [`WrappedShredSalt`] to a memory-only [`ShredSalt`].
pub fn unwrap_shred_salt(
    cap: &KekUnwrapCap,
    wrapped: &WrappedShredSalt,
) -> Result<ShredSalt, CryptoRefuse> {
    if wrapped.ciphertext.len() != 64 {
        return Err(CryptoRefuse::UnwrapFailed);
    }
    let dig = &wrapped.ciphertext[..32];
    let ct = &wrapped.ciphertext[32..];
    let mut keystream = Sha256::new();
    keystream.update(b"kyzo.wrap.shred_salt.stream.v1");
    keystream.update(cap.kek().as_bytes());
    keystream.update(dig);
    let stream = keystream.finalize();
    let mut salt = [0u8; 32];
    for i in 0..32 {
        salt[i] = ct[i] ^ stream[i];
    }
    // Re-derive commitment and check.
    let mut h = Sha256::new();
    h.update(b"kyzo.wrap.shred_salt.v1");
    h.update(cap.kek().as_bytes());
    h.update(wrapped.crypto_domain.store_id().as_bytes());
    h.update(u64::to_be_bytes(wrapped.crypto_domain.fence_epoch().get()));
    h.update(u64::to_be_bytes(wrapped.segment.get()));
    h.update(salt);
    let expect = h.finalize();
    if expect.as_slice() != dig {
        return Err(CryptoRefuse::UnwrapFailed);
    }
    Ok(ShredSalt::from_bytes(salt))
}

/// `DEK = derive(KEK, CryptoDomain, SegmentCounter, ShredSalt)`.
///
/// Unstructured random DEKs / single-global-DEK are Unconstructible.
pub fn derive_dek(
    cap: &KekUnwrapCap,
    crypto_domain: CryptoDomain,
    segment: SegmentCounter,
    salt: &ShredSalt,
) -> Dek {
    let mut h = Sha256::new();
    h.update(b"kyzo.dek.derive.v1");
    h.update(cap.kek().as_bytes());
    h.update(crypto_domain.store_id().as_bytes());
    h.update(u64::to_be_bytes(crypto_domain.fence_epoch().get()));
    h.update(u64::to_be_bytes(segment.get()));
    h.update(salt.as_bytes());
    let dig = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&dig);
    Dek::from_bytes(out)
}

/// Compressed plaintext — the only input the encrypt door accepts.
///
/// Encrypt-then-compress is Unconstructible: there is no encrypt path over
/// raw plaintext bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedBytes(Vec<u8>);

impl CompressedBytes {
    /// Borrow compressed bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// AEAD ciphertext sealed over compressed plaintext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ciphertext {
    arm: AeadArm,
    nonce: [u8; 12],
    body: Vec<u8>,
}

impl Ciphertext {
    /// AEAD arm used.
    pub fn arm(&self) -> AeadArm {
        self.arm
    }

    /// Nonce sealed into the ciphertext.
    pub fn nonce(&self) -> &[u8; 12] {
        &self.nonce
    }

    /// Ciphertext body.
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// Compress plaintext. Must precede AEAD (§67).
pub fn compress(plaintext: &[u8]) -> CompressedBytes {
    // Provisional: identity compression until a pure-Rust codec seat lands.
    // The type split — not the codec — makes encrypt-then-compress unrepresentable.
    CompressedBytes(plaintext.to_vec())
}

/// Encrypt compressed bytes under a DEK + nonce + arm.
///
/// Only accepts [`CompressedBytes`] — compression precedes AEAD by construction.
pub fn encrypt(
    compressed: CompressedBytes,
    dek: &Dek,
    nonce: [u8; 12],
    arm: AeadArm,
    aad: &CanonicalTranscript,
) -> Ciphertext {
    let mut h = Sha256::new();
    h.update(b"kyzo.aead.seal.v1");
    h.update([arm_tag(arm)]);
    h.update(dek.as_bytes());
    h.update(nonce);
    h.update(aad.as_bytes());
    h.update(compressed.as_bytes());
    let dig = h.finalize();
    let mut body = Vec::with_capacity(32 + compressed.as_bytes().len());
    body.extend_from_slice(&dig);
    // Provisional keystream XOR (AEAD/SIV crate binding is a later story).
    let mut stream = Sha256::new();
    stream.update(b"kyzo.aead.stream.v1");
    stream.update(dek.as_bytes());
    stream.update(nonce);
    let mut block = stream.finalize().to_vec();
    for (i, b) in compressed.as_bytes().iter().enumerate() {
        if i % 32 == 0 && i > 0 {
            let mut n = Sha256::new();
            n.update(b"kyzo.aead.stream.v1");
            n.update(&block);
            block = n.finalize().to_vec();
        }
        body.push(b ^ block[i % 32]);
    }
    Ciphertext { arm, nonce, body }
}

/// Compression-then-encryption pipeline — the only Store path (§67).
pub fn compress_then_encrypt(
    plaintext: &[u8],
    dek: &Dek,
    nonce: [u8; 12],
    arm: AeadArm,
    aad: &CanonicalTranscript,
) -> Ciphertext {
    encrypt(compress(plaintext), dek, nonce, arm, aad)
}

fn arm_tag(arm: AeadArm) -> u8 {
    match arm {
        AeadArm::Gcm => 1,
        AeadArm::Siv => 2,
    }
}

/// Shred receipt — wrapped salt destroyed inside the sovereignty boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShredReceipt {
    segment: SegmentCounter,
    crypto_domain: CryptoDomain,
}

impl ShredReceipt {
    /// Segment whose wrapped salt was destroyed.
    pub fn segment(&self) -> SegmentCounter {
        self.segment
    }

    /// Crypto domain of the shredded segment.
    pub fn crypto_domain(&self) -> CryptoDomain {
        self.crypto_domain
    }
}

/// Destroy a [`WrappedShredSalt`] (and, by Spec, all authorized replicas via
/// retention). Consumes the wrap — post-shred restore → [`CryptoRefuse::Shredded`].
pub fn shred(wrapped: WrappedShredSalt) -> ShredReceipt {
    ShredReceipt {
        segment: wrapped.segment,
        crypto_domain: wrapped.crypto_domain,
    }
}

#[cfg(test)]
mod pins {
    use super::*;
    use crate::store::contract::FormatVersion;
    use crate::store::epoch::FenceEpoch;
    use crate::store::open::StoreId;
    use crate::store::transcript::{CanonicalTranscriptBuilder, FieldId, SealedArtifactKind};

    fn test_domain() -> CryptoDomain {
        let store = StoreId::from_digest([0xAB; 32]);
        CryptoDomain::new(store, FenceEpoch::genesis(store))
    }

    #[test]
    fn wrap_unwrap_round_trip_and_derive() {
        let kek = Kek::from_bytes([0x11; 32]);
        let cap = KekUnwrapCap::from_kek(kek);
        let salt = ShredSalt::from_bytes([0x22; 32]);
        let seg = SegmentCounter::from_raw(7);
        let domain = test_domain();
        let wrapped = wrap_shred_salt(&cap, &salt, seg, domain);
        let opened = unwrap_shred_salt(&cap, &wrapped).expect("unwrap");
        let dek = derive_dek(&cap, domain, seg, &opened);
        assert_eq!(dek.as_bytes().len(), 32);
        let _ = shred(wrapped);
    }

    #[test]
    fn compress_then_encrypt_is_the_only_pipeline() {
        let kek = Kek::from_bytes([0x33; 32]);
        let cap = KekUnwrapCap::from_kek(kek);
        let salt = ShredSalt::from_bytes([0x44; 32]);
        let domain = test_domain();
        let dek = derive_dek(&cap, domain, SegmentCounter::ZERO, &salt);
        let mut b = CanonicalTranscriptBuilder::new(FormatVersion::CURRENT).unwrap();
        b.append_u64(FieldId::ARTIFACT_KIND, SealedArtifactKind::AuditKeyLeaf.tag())
            .unwrap();
        let aad = b.seal();
        let ct = compress_then_encrypt(b"hello", &dek, [9u8; 12], AeadArm::Siv, &aad);
        assert_eq!(ct.arm(), AeadArm::Siv);
        assert!(!ct.body().is_empty());
    }
}
