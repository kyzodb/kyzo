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
//!
//! Cipher binding (T11): [`AeadArm::Siv`] → RustCrypto `aes-gcm-siv` (AES-256-GCM-SIV,
//! misuse-resistant; required when SnapshotFork=yes). [`AeadArm::Gcm`] →
//! RustCrypto `chacha20poly1305`. Shred-salt wrap uses the SIV arm under the KEK.

use std::collections::HashSet;

use sha2::{Digest, Sha256};

use super::epoch::CryptoDomain;
use super::transcript::{CanonicalTranscript, encode_key_commitment};

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Per-segment counter separating DEK space under one CryptoDomain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SegmentCounter(u64);

impl SegmentCounter {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Counter zero.
    pub const ZERO: SegmentCounter = SegmentCounter(0);

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Wrap an already-proven segment counter.
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw counter.
    pub fn get(self) -> u64 {
        self.0
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Closed AEAD arm selection. SnapshotFork=yes arms require misuse-resistant SIV.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AeadArm {
    /// Standard AEAD (ChaCha20-Poly1305). Forbidden on SnapshotFork=yes arms.
    Gcm,
    /// Misuse-resistant AES-256-GCM-SIV — nonce repeat degrades to message-equality leak only.
    Siv,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
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
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Mint a unwrap capability from already-held root KEK material.
    pub(crate) fn from_kek(kek: Kek) -> Self {
        Self { kek }
    }

    /// Borrow the KEK for derive / wrap sites that already hold this capability.
    pub(crate) fn kek(&self) -> &Kek {
        &self.kek
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Key-encryption key — closed [`super::keys::Secret`] member. Never in packs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kek([u8; 32]);

impl Kek {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Wrap already-proven KEK bytes (HSM / genesis sites).
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Data-encryption key derived under the sealed hierarchy — never unstructured.
///
/// Carries the [`CryptoDomain`] it was derived under so CMT-1 key-commitment
/// (seat 67a) can bind key-id + domain without a second encrypt-door argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dek {
    bytes: [u8; 32],
    crypto_domain: CryptoDomain,
}

impl Dek {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    fn from_derived(bytes: [u8; 32], crypto_domain: CryptoDomain) -> Self {
        Self {
            bytes,
            crypto_domain,
        }
    }

    /// Borrow DEK bytes for the encrypt door.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// CryptoDomain this DEK was derived under (CMT-1 bind).
    pub fn crypto_domain(&self) -> CryptoDomain {
        self.crypto_domain
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Plaintext shred salt — transient, memory-only, never persisted.
///
/// No serde. No constructor accepts this into WAL / pack / seal writers.
/// Exists only inside the derivation moment after unwrap of [`WrappedShredSalt`].
#[derive(Debug)]
pub struct ShredSalt([u8; 32]);

impl ShredSalt {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Draw / wrap plaintext salt bytes at the derivation moment only.
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// KEK-wrapped shred salt — the only persistable salt form.
///
/// Required in every leave-is-free pack. Useless without the KEK.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WrappedShredSalt {
    /// Opaque ciphertext under the Store KEK (AES-256-GCM-SIV).
    ciphertext: Vec<u8>,
    /// Segment this wrapped salt derives DEKs for.
    segment: SegmentCounter,
    /// Crypto domain binding.
    crypto_domain: CryptoDomain,
}

impl WrappedShredSalt {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
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

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
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

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Audit integrity key — leaf MAC over CanonicalTranscript.
///
/// `AuditKey ≠ AncestorReadGrant ≠ decrypt ≠ WriteAuthority`.
/// Wrapped under KEK alongside WrappedShredSalts. Never in packs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditKey([u8; 32]);

impl AuditKey {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Wrap already-proven audit key bytes.
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
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

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Typed refuse from crypto doors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum CryptoRefuse {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    #[error("crypto: missing root KEK unwrap capability")]
    #[diagnostic(code(store::crypto::missing_root_kek))]
    MissingRootKek,
    #[error("crypto: wrapped shred salt unwrap failed")]
    #[diagnostic(code(store::crypto::unwrap_failed))]
    UnwrapFailed,
    #[error("crypto: segment already shredded — typed tombstone")]
    #[diagnostic(code(store::crypto::shredded))]
    Shredded,
    #[error("crypto: AEAD seal or open failed")]
    #[diagnostic(code(store::crypto::aead_failed))]
    AeadFailed,
    #[error("crypto: AEAD key-commitment mismatch")]
    #[diagnostic(code(store::crypto::key_commitment_mismatch))]
    KeyCommitmentMismatch,
    #[error("crypto: lz4 decompress failed")]
    #[diagnostic(code(store::crypto::decompress_failed))]
    DecompressFailed,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Poly1305 / GCM-SIV authentication tag length (both arms).
const AEAD_TAG_LEN: usize = 16;
#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// SHA-256 key-commitment length appended after ciphertext ‖ tag.
const KEY_COMMIT_LEN: usize = 32;

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// CMT-1 key-commitment: `C = H_canonical(KEY_COMMIT domain-label, key-id, CryptoDomain)`.
///
/// Minted through the ONE [`encode_key_commitment`] CanonicalTranscript constructor
/// (seat 59). The AEAD tag already binds nonce+aad+message; C adds only key-binding.
fn key_commitment(key: &[u8; 32], crypto_domain: CryptoDomain) -> Result<[u8; 32], CryptoRefuse> {
    let transcript =
        encode_key_commitment(key, crypto_domain).map_err(|_| CryptoRefuse::AeadFailed)?;
    let mut h = Sha256::new();
    h.update(transcript.as_bytes());
    let dig = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&dig);
    Ok(out)
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Constant-time equality over a 32-byte commitment.
fn ct_eq_32(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Domain + segment binding bytes used as wrap AAD.
fn wrap_aad(crypto_domain: CryptoDomain, segment: SegmentCounter) -> Vec<u8> {
    let mut aad = Vec::with_capacity(4 + 32 + 8 + 8);
    aad.extend_from_slice(b"WSS1");
    aad.extend_from_slice(crypto_domain.store_id().as_bytes());
    aad.extend_from_slice(&u64::to_be_bytes(crypto_domain.fence_epoch().get()));
    aad.extend_from_slice(&u64::to_be_bytes(segment.get()));
    aad
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Deterministic 96-bit nonce for KEK wrap (SIV makes repeat safe).
fn wrap_nonce(crypto_domain: CryptoDomain, segment: SegmentCounter) -> [u8; 12] {
    let mut h = Sha256::new();
    h.update(b"kyzo.wrap.shred_salt.nonce.v1");
    h.update(crypto_domain.store_id().as_bytes());
    h.update(u64::to_be_bytes(crypto_domain.fence_epoch().get()));
    h.update(u64::to_be_bytes(segment.get()));
    let dig = h.finalize();
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&dig[..12]);
    nonce
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Seal bytes under AES-256-GCM-SIV (misuse-resistant).
fn aes_gcm_siv_seal(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoRefuse> {
    use aes_gcm_siv::aead::{Aead, KeyInit, Payload};
    use aes_gcm_siv::{Aes256GcmSiv, Nonce};

    let cipher = Aes256GcmSiv::new_from_slice(key).map_err(|_| CryptoRefuse::AeadFailed)?;
    let nonce: &Nonce = nonce.into();
    cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoRefuse::AeadFailed)
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Open bytes under AES-256-GCM-SIV.
fn aes_gcm_siv_open(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoRefuse> {
    use aes_gcm_siv::aead::{Aead, KeyInit, Payload};
    use aes_gcm_siv::{Aes256GcmSiv, Nonce};

    let cipher = Aes256GcmSiv::new_from_slice(key).map_err(|_| CryptoRefuse::AeadFailed)?;
    let nonce: &Nonce = nonce.into();
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| CryptoRefuse::AeadFailed)
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Seal bytes under ChaCha20-Poly1305 (Gcm arm).
fn chacha20poly1305_seal(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoRefuse> {
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Nonce};

    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| CryptoRefuse::AeadFailed)?;
    let nonce: &Nonce = nonce.into();
    cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoRefuse::AeadFailed)
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Open bytes under ChaCha20-Poly1305 (Gcm arm).
fn chacha20poly1305_open(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoRefuse> {
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Nonce};

    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| CryptoRefuse::AeadFailed)?;
    let nonce: &Nonce = nonce.into();
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| CryptoRefuse::AeadFailed)
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Base AEAD seal (ciphertext ‖ tag) — no key-commitment.
fn seal_aead_arm(
    arm: AeadArm,
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoRefuse> {
    match arm {
        AeadArm::Siv => aes_gcm_siv_seal(key, nonce, aad, plaintext),
        AeadArm::Gcm => chacha20poly1305_seal(key, nonce, aad, plaintext),
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Base AEAD open over ciphertext ‖ tag — no key-commitment.
fn open_aead_arm(
    arm: AeadArm,
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoRefuse> {
    match arm {
        AeadArm::Siv => aes_gcm_siv_open(key, nonce, aad, ciphertext),
        AeadArm::Gcm => chacha20poly1305_open(key, nonce, aad, ciphertext),
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Committing-AEAD seal: base arm then append CMT-1 `C`.
///
/// Sealed bytes = ciphertext ‖ tag ‖ C, with
/// `C = H_canonical(KEY_COMMIT domain-label, key-id, CryptoDomain)`.
/// KeyCommitment posture is on for all AEAD sites (seat 27 pattern).
fn seal_arm(
    arm: AeadArm,
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    plaintext: &[u8],
    crypto_domain: CryptoDomain,
) -> Result<Vec<u8>, CryptoRefuse> {
    let mut sealed = seal_aead_arm(arm, key, nonce, aad, plaintext)?;
    if sealed.len() < AEAD_TAG_LEN {
        return Err(CryptoRefuse::AeadFailed);
    }
    let c = key_commitment(key, crypto_domain)?;
    sealed.extend_from_slice(&c);
    Ok(sealed)
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Committing-AEAD open: base arm, then constant-time key-commitment check.
///
/// On commitment mismatch returns [`CryptoRefuse::KeyCommitmentMismatch`] and
/// does not release the AEAD plaintext.
fn open_arm(
    arm: AeadArm,
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    sealed: &[u8],
    crypto_domain: CryptoDomain,
) -> Result<Vec<u8>, CryptoRefuse> {
    if sealed.len() < AEAD_TAG_LEN + KEY_COMMIT_LEN {
        return Err(CryptoRefuse::AeadFailed);
    }
    let split = sealed.len() - KEY_COMMIT_LEN;
    let (aead_body, presented_c) = sealed.split_at(split);
    let plaintext = open_aead_arm(arm, key, nonce, aad, aead_body)?;
    let expected = key_commitment(key, crypto_domain)?;
    let mut presented = [0u8; KEY_COMMIT_LEN];
    presented.copy_from_slice(presented_c);
    if !ct_eq_32(&expected, &presented) {
        // never release plaintext on commitment mismatch
        drop(plaintext);
        return Err(CryptoRefuse::KeyCommitmentMismatch);
    }
    Ok(plaintext)
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Wrap a plaintext [`ShredSalt`] under the KEK for persistence.
///
/// Plaintext salt must not escape this call except into [`derive_dek`].
/// Uses AES-256-GCM-SIV under the KEK (misuse-resistant; deterministic nonce).
pub fn wrap_shred_salt(
    cap: &KekUnwrapCap,
    salt: &ShredSalt,
    segment: SegmentCounter,
    crypto_domain: CryptoDomain,
) -> Result<WrappedShredSalt, CryptoRefuse> {
    let nonce = wrap_nonce(crypto_domain, segment);
    let aad = wrap_aad(crypto_domain, segment);
    // KeyCommitment posture on for all AEAD sites — wrap uses SIV + CMT-1.
    let body = seal_arm(
        AeadArm::Siv,
        cap.kek().as_bytes(),
        &nonce,
        &aad,
        salt.as_bytes(),
        crypto_domain,
    )?;
    Ok(WrappedShredSalt {
        ciphertext: body,
        segment,
        crypto_domain,
    })
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Unwrap a persisted [`WrappedShredSalt`] to a memory-only [`ShredSalt`].
///
/// Consults [`ShredLedger`] first: a recorded tombstone → [`CryptoRefuse::Shredded`]
/// even when an old pack still carries the wrapped ciphertext bytes.
pub fn unwrap_shred_salt(
    cap: &KekUnwrapCap,
    wrapped: &WrappedShredSalt,
    ledger: &ShredLedger,
) -> Result<ShredSalt, CryptoRefuse> {
    if ledger.is_shredded(wrapped) {
        return Err(CryptoRefuse::Shredded);
    }
    let nonce = wrap_nonce(wrapped.crypto_domain, wrapped.segment);
    let aad = wrap_aad(wrapped.crypto_domain, wrapped.segment);
    let pt = match open_arm(
        AeadArm::Siv,
        cap.kek().as_bytes(),
        &nonce,
        &aad,
        &wrapped.ciphertext,
        wrapped.crypto_domain,
    ) {
        Ok(pt) => pt,
        Err(CryptoRefuse::KeyCommitmentMismatch) => {
            return Err(CryptoRefuse::KeyCommitmentMismatch);
        }
        Err(_) => return Err(CryptoRefuse::UnwrapFailed),
    };
    if pt.len() != 32 {
        return Err(CryptoRefuse::UnwrapFailed);
    }
    let mut salt = [0u8; 32];
    salt.copy_from_slice(&pt);
    Ok(ShredSalt::from_bytes(salt))
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
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
    Dek::from_derived(out, crypto_domain)
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Compressed plaintext — the only input the encrypt door accepts.
///
/// Encrypt-then-compress is Unconstructible: there is no encrypt path over
/// raw plaintext bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedBytes(Vec<u8>);

impl CompressedBytes {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Borrow compressed bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// AEAD ciphertext sealed over compressed plaintext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ciphertext {
    arm: AeadArm,
    nonce: [u8; 12],
    body: Vec<u8>,
}

impl Ciphertext {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// AEAD arm used.
    pub fn arm(&self) -> AeadArm {
        self.arm
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Nonce sealed into the ciphertext.
    pub fn nonce(&self) -> &[u8; 12] {
        &self.nonce
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Ciphertext body (AEAD ciphertext ‖ tag ‖ key-commitment).
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Compress plaintext with pure-Rust LZ4 (`lz4_flex`). Must precede AEAD (§67).
pub fn compress(plaintext: &[u8]) -> CompressedBytes {
    CompressedBytes(lz4_flex::compress_prepend_size(plaintext))
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Decompress LZ4 size-prepended bytes from [`compress`].
pub fn decompress(compressed: &CompressedBytes) -> Result<Vec<u8>, CryptoRefuse> {
    lz4_flex::decompress_size_prepended(compressed.as_bytes())
        .map_err(|_| CryptoRefuse::DecompressFailed)
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Encrypt compressed bytes under a DEK + nonce + arm.
///
/// Only accepts [`CompressedBytes`] — compression precedes AEAD by construction.
/// [`AeadArm::Siv`] → AES-256-GCM-SIV; [`AeadArm::Gcm`] → ChaCha20-Poly1305.
pub fn encrypt(
    compressed: CompressedBytes,
    dek: &Dek,
    nonce: [u8; 12],
    arm: AeadArm,
    aad: &CanonicalTranscript,
) -> Result<Ciphertext, CryptoRefuse> {
    let body = seal_arm(
        arm,
        dek.as_bytes(),
        &nonce,
        aad.as_bytes(),
        compressed.as_bytes(),
        dek.crypto_domain(),
    )?;
    Ok(Ciphertext { arm, nonce, body })
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Open AEAD ciphertext back to [`CompressedBytes`].
pub fn decrypt(
    ciphertext: &Ciphertext,
    dek: &Dek,
    aad: &CanonicalTranscript,
) -> Result<CompressedBytes, CryptoRefuse> {
    let pt = open_arm(
        ciphertext.arm,
        dek.as_bytes(),
        &ciphertext.nonce,
        aad.as_bytes(),
        &ciphertext.body,
        dek.crypto_domain(),
    )?;
    Ok(CompressedBytes(pt))
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Compression-then-encryption pipeline — the only Store path (§67).
pub fn compress_then_encrypt(
    plaintext: &[u8],
    dek: &Dek,
    nonce: [u8; 12],
    arm: AeadArm,
    aad: &CanonicalTranscript,
) -> Result<Ciphertext, CryptoRefuse> {
    encrypt(compress(plaintext), dek, nonce, arm, aad)
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Shred receipt — wrapped salt destroyed inside the sovereignty boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShredReceipt {
    segment: SegmentCounter,
    crypto_domain: CryptoDomain,
}

impl ShredReceipt {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Segment whose wrapped salt was destroyed.
    pub fn segment(&self) -> SegmentCounter {
        self.segment
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Crypto domain of the shredded segment.
    pub fn crypto_domain(&self) -> CryptoDomain {
        self.crypto_domain
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Durable shred tombstone naming one shredded (CryptoDomain, SegmentCounter).
///
/// Survives so post-shred restore of an old pack that still carries the wrapped
/// ciphertext converges to [`CryptoRefuse::Shredded`], not silent unreadability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShredTombstone {
    segment: SegmentCounter,
    crypto_domain: CryptoDomain,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
impl ShredTombstone {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Segment this tombstone revokes.
    pub fn segment(self) -> SegmentCounter {
        self.segment
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Crypto domain this tombstone revokes under.
    pub fn crypto_domain(self) -> CryptoDomain {
        self.crypto_domain
    }

    /// Whether this tombstone covers a wrapped salt handle.
    pub fn covers(self, wrapped: &WrappedShredSalt) -> bool {
        self.segment == wrapped.segment && self.crypto_domain == wrapped.crypto_domain
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Ledger of shredded segments consulted on unwrap / leave-is-free restore.
#[derive(Debug, Default, Clone)]
pub struct ShredLedger {
    /// (store_id, fence_epoch, segment) keys revoked by shred.
    keys: HashSet<([u8; 32], u64, u64)>,
}

impl ShredLedger {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Empty ledger — no segments shredded.
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Record a tombstone from [`shred`].
    pub fn record(&mut self, tombstone: ShredTombstone) {
        self.keys.insert((
            *tombstone.crypto_domain.store_id().as_bytes(),
            tombstone.crypto_domain.fence_epoch().get(),
            tombstone.segment.get(),
        ));
    }

    /// True when unwrap / restore of this wrap must refuse Shredded.
    pub fn is_shredded(&self, wrapped: &WrappedShredSalt) -> bool {
        self.keys.contains(&(
            *wrapped.crypto_domain.store_id().as_bytes(),
            wrapped.crypto_domain.fence_epoch().get(),
            wrapped.segment.get(),
        ))
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Destroy a [`WrappedShredSalt`] (and, by Spec, all authorized replicas via
/// retention). Consumes the wrap — post-shred restore → [`CryptoRefuse::Shredded`]
/// once the returned [`ShredTombstone`] is recorded in a [`ShredLedger`].
pub fn shred(wrapped: WrappedShredSalt) -> (ShredReceipt, ShredTombstone) {
    let receipt = ShredReceipt {
        segment: wrapped.segment,
        crypto_domain: wrapped.crypto_domain,
    };
    let tombstone = ShredTombstone {
        segment: wrapped.segment,
        crypto_domain: wrapped.crypto_domain,
    };
    // `wrapped` drops here — this handle's ciphertext is gone.
    drop(wrapped);
    (receipt, tombstone)
}

#[cfg(test)]
mod pins {
    use super::*;
    use crate::store::contract::FormatVersion;
    use crate::store::epoch::FenceEpoch;
    use crate::store::open::StoreId;
    use crate::store::transcript::{CanonicalTranscriptBuilder, FieldId, SealedArtifactKind};

    /// GUARDIAN GATE (#376 T2) — CMT-1 key-commitment over the Gcm collision.
    /// Empirically constructed (invisible-salamanders / Partitioning Oracle Attacks): ONE
    /// (ciphertext ‖ tag) that the base ChaCha20-Poly1305 AEAD accepts under TWO distinct
    /// keys, using our exact `wrap_aad` framing. The collision block was solved over
    /// GF(2^130-5) so both keys' Poly1305 tags coincide. With CMT-1 via CanonicalTranscript,
    /// C binds only the opening key (+ CryptoDomain): present (ct‖tag‖C_K1) → opens under K1
    /// and refuses under K2 with [`CryptoRefuse::KeyCommitmentMismatch`]
    /// (K2 recomputes C_K2 ≠ C_K1). Exercises two-key rejection through production
    /// [`open_arm`], not a short-input refuse.
    #[test]
    fn gcm_arm_is_not_key_committing() {
        let k1 = [0x11u8; 32];
        let k2 = [0x22u8; 32];
        let nonce = [0x24u8; 12];
        let domain = test_domain();
        // production wrap_aad framing: "WSS1" || store_id || epoch_be || segment_be.
        let mut aad = Vec::new();
        aad.extend_from_slice(b"WSS1");
        aad.extend_from_slice(&[0x5au8; 32]);
        aad.extend_from_slice(&1u64.to_be_bytes());
        aad.extend_from_slice(&1u64.to_be_bytes());
        // crafted collision: ciphertext (32B) || Poly1305 tag (16B).
        let ct: [u8; 32] = [
            0xba, 0x07, 0x9a, 0x1e, 0x2a, 0xc2, 0x5c, 0x23, 0xfd, 0xaf, 0x16, 0xf3, 0xa6, 0x29,
            0x71, 0x7d, 0x01, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let tag: [u8; 16] = [
            0xed, 0x37, 0x20, 0x04, 0x6f, 0xe1, 0x63, 0xe8, 0xfb, 0xc6, 0x50, 0x60, 0x44, 0x21,
            0xab, 0x77,
        ];
        // Prove the base AEAD still collides under both keys (transform sits above).
        let mut aead_body = ct.to_vec();
        aead_body.extend_from_slice(&tag);
        assert!(
            open_aead_arm(AeadArm::Gcm, &k1, &nonce, &aad, &aead_body).is_ok()
                && open_aead_arm(AeadArm::Gcm, &k2, &nonce, &aad, &aead_body).is_ok(),
            "fixture must still be a two-key Poly1305 collision under the base Gcm arm"
        );
        // Commit under K1 via CanonicalTranscript CMT-1; present (ct‖tag‖C_K1).
        let c_k1 = key_commitment(&k1, domain).expect("mint C_K1");
        let mut msg = aead_body;
        msg.extend_from_slice(&c_k1);
        let o1 = open_arm(AeadArm::Gcm, &k1, &nonce, &aad, &msg, domain);
        let o2 = open_arm(AeadArm::Gcm, &k2, &nonce, &aad, &msg, domain);
        assert!(
            o1.is_ok(),
            "committed ciphertext must open under the committing key K1"
        );
        assert!(
            matches!(o2, Err(CryptoRefuse::KeyCommitmentMismatch)),
            "production open_arm must refuse the K1-committed collision under K2 with \
             KeyCommitmentMismatch (got {o2:?})"
        );
    }

    fn test_domain() -> CryptoDomain {
        let store = StoreId::from_digest([0xAB; 32]);
        CryptoDomain::new(store, FenceEpoch::genesis(store))
    }

    fn test_aad() -> CanonicalTranscript {
        let mut b = CanonicalTranscriptBuilder::new(FormatVersion::CURRENT).unwrap();
        b.append_u64(
            FieldId::ARTIFACT_KIND,
            SealedArtifactKind::AuditKeyLeaf.tag(),
        )
        .unwrap();
        b.seal()
    }

    #[test]
    fn wrap_unwrap_round_trip_and_derive() {
        let kek = Kek::from_bytes([0x11; 32]);
        let cap = KekUnwrapCap::from_kek(kek);
        let salt = ShredSalt::from_bytes([0x22; 32]);
        let seg = SegmentCounter::from_raw(7);
        let domain = test_domain();
        let wrapped = wrap_shred_salt(&cap, &salt, seg, domain).expect("wrap");
        let ledger = ShredLedger::new();
        let opened = unwrap_shred_salt(&cap, &wrapped, &ledger).expect("unwrap");
        let dek = derive_dek(&cap, domain, seg, &opened);
        assert_eq!(dek.as_bytes().len(), 32);
        let (receipt, tombstone) = shred(wrapped);
        assert_eq!(receipt.segment(), seg);
        assert!(tombstone.covers(&WrappedShredSalt::from_persisted(vec![0], seg, domain)));
    }

    #[test]
    fn post_shred_unwrap_refuses_shredded() {
        let kek = Kek::from_bytes([0x11; 32]);
        let cap = KekUnwrapCap::from_kek(kek);
        let salt = ShredSalt::from_bytes([0x22; 32]);
        let seg = SegmentCounter::from_raw(3);
        let domain = test_domain();
        let wrapped = wrap_shred_salt(&cap, &salt, seg, domain).expect("wrap");
        // Old pack still holds a copy of the wrapped ciphertext.
        let stale_copy = wrapped.clone();
        let (_receipt, tombstone) = shred(wrapped);
        let mut ledger = ShredLedger::new();
        ledger.record(tombstone);
        assert!(matches!(
            unwrap_shred_salt(&cap, &stale_copy, &ledger),
            Err(CryptoRefuse::Shredded)
        ));
    }

    /// GUARDIAN NASTY (#376 T2 — cross-domain reinterpretation): the CMT-1 commitment
    /// binds CryptoDomain, but the T2 gate only ever varies the KEY. This drives the
    /// DOMAIN axis under the SAME KEK: a KEK-wrapped ShredSalt sealed for one
    /// CryptoDomain must REFUSE when the identical ciphertext bytes are re-presented
    /// (via the real `from_persisted` forge door) under a DIFFERENT CryptoDomain.
    /// Lifting a segment's wrapped key material across epochs or stores would be
    /// cross-epoch / cross-tenant key-material confusion (seat 62 CryptoDomain
    /// separation). RED if `unwrap` ever returns the salt under a forged domain.
    #[test]
    fn cross_domain_wrapped_salt_reinterpretation_refuses() {
        let cap = KekUnwrapCap::from_kek(Kek::from_bytes([0x77; 32])); // SAME KEK throughout
        let salt = ShredSalt::from_bytes([0x99; 32]);
        let seg = SegmentCounter::from_raw(4);
        let ledger = ShredLedger::new();

        let store_a = StoreId::from_digest([0xA1; 32]);
        let store_b = StoreId::from_digest([0xB2; 32]);
        let domain_a = CryptoDomain::new(store_a, FenceEpoch::genesis(store_a));

        // Sanity: wrap+unwrap under the TRUE domain succeeds (not a blanket lockout).
        let wrapped = wrap_shred_salt(&cap, &salt, seg, domain_a).expect("wrap under A");
        assert!(
            unwrap_shred_salt(&cap, &wrapped, &ledger).is_ok(),
            "true-domain unwrap must succeed"
        );

        // Axis 1 — cross-EPOCH (same store, same KEK, forged later fence epoch).
        let domain_a_ep5 = CryptoDomain::new(store_a, FenceEpoch::from_raw(store_a, 5));
        let forged_epoch =
            WrappedShredSalt::from_persisted(wrapped.ciphertext().to_vec(), seg, domain_a_ep5);
        assert!(
            unwrap_shred_salt(&cap, &forged_epoch, &ledger).is_err(),
            "CROSS-EPOCH REINTERPRETATION: a wrapped salt sealed at epoch genesis must NOT \
             unwrap under a forged later epoch on the same KEK"
        );

        // Axis 2 — cross-STORE (different store id, same KEK).
        let domain_b = CryptoDomain::new(store_b, FenceEpoch::genesis(store_b));
        let forged_store =
            WrappedShredSalt::from_persisted(wrapped.ciphertext().to_vec(), seg, domain_b);
        assert!(
            unwrap_shred_salt(&cap, &forged_store, &ledger).is_err(),
            "CROSS-STORE REINTERPRETATION: a wrapped salt sealed for store A must NOT unwrap \
             under a forged store B on the same KEK"
        );
    }

    #[test]
    fn compress_then_encrypt_round_trips_siv_and_is_not_identity() {
        let kek = Kek::from_bytes([0x33; 32]);
        let cap = KekUnwrapCap::from_kek(kek);
        let salt = ShredSalt::from_bytes([0x44; 32]);
        let domain = test_domain();
        let dek = derive_dek(&cap, domain, SegmentCounter::ZERO, &salt);
        let aad = test_aad();
        let plaintext = b"hello compress-then-encrypt pipeline";
        let compressed = compress(plaintext);
        assert_ne!(
            compressed.as_bytes(),
            plaintext,
            "compress must not be a silent identity no-op"
        );
        let ct =
            compress_then_encrypt(plaintext, &dek, [9u8; 12], AeadArm::Siv, &aad).expect("encrypt");
        assert_eq!(ct.arm(), AeadArm::Siv);
        assert!(!ct.body().is_empty());
        let opened = decrypt(&ct, &dek, &aad).expect("decrypt");
        let round = decompress(&opened).expect("decompress");
        assert_eq!(round, plaintext);
    }

    #[test]
    fn gcm_arm_uses_chacha20poly1305() {
        let kek = Kek::from_bytes([0x55; 32]);
        let cap = KekUnwrapCap::from_kek(kek);
        let salt = ShredSalt::from_bytes([0x66; 32]);
        let domain = test_domain();
        let dek = derive_dek(&cap, domain, SegmentCounter::ZERO, &salt);
        let aad = test_aad();
        let ct = compress_then_encrypt(b"gcm-arm", &dek, [1u8; 12], AeadArm::Gcm, &aad)
            .expect("encrypt");
        assert_eq!(ct.arm(), AeadArm::Gcm);
        let opened = decrypt(&ct, &dek, &aad).expect("decrypt");
        assert_eq!(decompress(&opened).expect("decompress"), b"gcm-arm");
    }
}
