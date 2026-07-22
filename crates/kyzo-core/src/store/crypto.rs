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

use sha2::{Digest as ShaDigest, Sha256};

use super::epoch::CryptoDomain;
use super::transcript::{
    CanonicalTranscript, encode_key_commitment, encode_wrapped_shred_salt_aad,
};

/// AEAD nonce (96-bit). Distinct from [`Digest`] / [`Mac`] / key material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nonce([u8; 12]);

impl Nonce {
    /// Wrap already-proven nonce bytes (mint / wrap-derive sites).
    pub fn from_bytes(bytes: [u8; 12]) -> Self {
        Self(bytes)
    }

    /// Borrow nonce bytes at the RustCrypto / wire edge only.
    pub fn as_bytes(&self) -> &[u8; 12] {
        &self.0
    }
}

/// Fixed 32-byte digest (SHA-256 / CMT-1 commitment). Distinct from [`Mac`] / keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Wrap already-proven digest bytes (hash finalize / decode sites).
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow digest bytes at transcript / compare edges only.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Authenticator MAC (AuditKey leaf). Distinct from [`Digest`] — wrong-kind unconstructible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Mac([u8; 32]);

impl Mac {
    /// Wrap already-proven MAC bytes (leaf-MAC finalize).
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow MAC bytes at compare / pack edges only.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Sealed signature bytes (ed25519 / FROST wire width 64). Distinct from digests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signature([u8; 64]);

impl Signature {
    /// Wrap already-proven signature bytes (sign / decode sites).
    pub fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    /// Borrow signature bytes at the RustCrypto verify edge only.
    pub fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }
}

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
    /// Standard AEAD (ChaCha20-Poly1305). Forbidden on SnapshotFork=yes arms.
    Gcm,
    /// Misuse-resistant AES-256-GCM-SIV — nonce repeat degrades to message-equality leak only.
    Siv,
}

/// Root unwrap capability presented at open (client / HSM trait).
///
/// Absent at open → [`super::failure::StoreRefuse::MissingRootKek`]. Presence
/// of this type is the crypto-door proof: wrap/unwrap/derive take `&KekUnwrapCap`,
/// so a missing root is unconstructible at those doors (not a crypto refuse).
/// Host-held wrapped DEKs are a separate at-rest layer; zero-access still
/// requires this root.
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

    /// Borrow KEK bytes at the RustCrypto edge only.
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

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
    fn from_derived(bytes: [u8; 32], crypto_domain: CryptoDomain) -> Self {
        Self {
            bytes,
            crypto_domain,
        }
    }

    /// Borrow DEK bytes at the RustCrypto edge only.
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// CryptoDomain this DEK was derived under (CMT-1 bind).
    pub fn crypto_domain(&self) -> CryptoDomain {
        self.crypto_domain
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
    /// Opaque ciphertext under the Store KEK (AES-256-GCM-SIV).
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
    pub fn leaf_mac(&self, transcript: &CanonicalTranscript) -> Mac {
        let mut h = Sha256::new();
        h.update(b"kyzo.audit.leaf.v1");
        h.update(self.0);
        h.update(transcript.as_bytes());
        let dig = h.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&dig);
        Mac::from_bytes(out)
    }
}

/// Typed refuse from crypto doors.
///
/// Missing root KEK is not a variant here — [`KekUnwrapCap`] makes absence
/// unconstructible at wrap/unwrap/derive; open refuses via
/// [`super::failure::StoreRefuse::MissingRootKek`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum CryptoRefuse {
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

/// Poly1305 / GCM-SIV authentication tag length (both arms).
const AEAD_TAG_LEN: usize = 16;
/// SHA-256 key-commitment length appended after ciphertext ‖ tag.
const KEY_COMMIT_LEN: usize = 32;

/// CMT-1 key-commitment over raw key bytes — RustCrypto / `.as_bytes()` edge only.
///
/// Callers must pass [`Dek::as_bytes`] or [`Kek::as_bytes`]; never a peer digest/MAC.
fn key_commitment_bytes(
    key: &[u8; 32],
    crypto_domain: CryptoDomain,
) -> Result<Digest, CryptoRefuse> {
    let transcript =
        encode_key_commitment(key, crypto_domain).map_err(|_| CryptoRefuse::AeadFailed)?;
    let mut h = Sha256::new();
    h.update(transcript.as_bytes());
    let dig = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&dig);
    Ok(Digest::from_bytes(out))
}

/// CMT-1 key-commitment for the DEK encrypt door — [`Kek`] cannot satisfy this.
///
/// Minted through the ONE [`encode_key_commitment`] CanonicalTranscript constructor
/// (seat 59). The AEAD tag already binds nonce+aad+message; C adds only key-binding.
fn key_commitment(key: &Dek, crypto_domain: CryptoDomain) -> Result<Digest, CryptoRefuse> {
    key_commitment_bytes(key.as_bytes(), crypto_domain)
}

/// CMT-1 key-commitment for the KEK wrap door — [`Dek`] cannot satisfy this.
fn key_commitment_kek(key: &Kek, crypto_domain: CryptoDomain) -> Result<Digest, CryptoRefuse> {
    key_commitment_bytes(key.as_bytes(), crypto_domain)
}

/// Constant-time equality over typed digests.
fn ct_eq_digest(a: &Digest, b: &Digest) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Mint the ONE WrappedShredSalt wrap-AAD CanonicalTranscript (seat 59).
fn shred_salt_wrap_transcript(
    crypto_domain: CryptoDomain,
    segment: SegmentCounter,
) -> Result<CanonicalTranscript, CryptoRefuse> {
    encode_wrapped_shred_salt_aad(crypto_domain, segment.get())
        .map_err(|_| CryptoRefuse::AeadFailed)
}

/// Deterministic 96-bit nonce for KEK wrap (SIV makes repeat safe).
///
/// Digests the ONE wrap-AAD [`CanonicalTranscript`] — no second serialization path.
/// Misuse-resistant SIV keeps deterministic nonce repeat message-equality-only.
fn wrap_nonce(transcript: &CanonicalTranscript) -> Nonce {
    let mut h = Sha256::new();
    h.update(transcript.as_bytes());
    let dig = h.finalize();
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&dig[..12]);
    Nonce::from_bytes(nonce)
}

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

/// Base AEAD seal over raw key bytes — after [`Dek::as_bytes`] / [`Kek::as_bytes`] only.
fn seal_aead_arm_bytes(
    arm: AeadArm,
    key: &[u8; 32],
    nonce: &Nonce,
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoRefuse> {
    match arm {
        AeadArm::Siv => aes_gcm_siv_seal(key, nonce.as_bytes(), aad, plaintext),
        AeadArm::Gcm => chacha20poly1305_seal(key, nonce.as_bytes(), aad, plaintext),
    }
}

/// Base AEAD open over raw key bytes — after [`Dek::as_bytes`] / [`Kek::as_bytes`] only.
fn open_aead_arm_bytes(
    arm: AeadArm,
    key: &[u8; 32],
    nonce: &Nonce,
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoRefuse> {
    match arm {
        AeadArm::Siv => aes_gcm_siv_open(key, nonce.as_bytes(), aad, ciphertext),
        AeadArm::Gcm => chacha20poly1305_open(key, nonce.as_bytes(), aad, ciphertext),
    }
}

/// Base AEAD open under a KEK — wrap path only. [`Dek`] cannot enter.
///
/// Production open goes through [`open_arm_kek`] → [`open_arm_committed`].
/// This typed non-committed door exists for the guardian collision fixture
/// that must exercise the base Gcm arm before CMT-1 is applied.
#[cfg(test)]
fn open_aead_arm_kek(
    arm: AeadArm,
    key: &Kek,
    nonce: &Nonce,
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoRefuse> {
    open_aead_arm_bytes(arm, key.as_bytes(), nonce, aad, ciphertext)
}

/// ONE committing-AEAD seal body — RustCrypto / `.as_bytes()` edge only.
///
/// Sealed bytes = ciphertext ‖ tag ‖ C, with
/// `C = H_canonical(KEY_COMMIT domain-label, key-id, CryptoDomain)`.
/// Callers must pass [`Dek::as_bytes`] or [`Kek::as_bytes`]; typed doors stay split.
fn seal_arm_committed(
    arm: AeadArm,
    key: &[u8; 32],
    nonce: &Nonce,
    aad: &[u8],
    plaintext: &[u8],
    crypto_domain: CryptoDomain,
) -> Result<Vec<u8>, CryptoRefuse> {
    let mut sealed = seal_aead_arm_bytes(arm, key, nonce, aad, plaintext)?;
    if sealed.len() < AEAD_TAG_LEN {
        return Err(CryptoRefuse::AeadFailed);
    }
    let c = key_commitment_bytes(key, crypto_domain)?;
    sealed.extend_from_slice(c.as_bytes());
    Ok(sealed)
}

/// Committing-AEAD seal under a DEK: thin typed door over [`seal_arm_committed`].
///
/// KeyCommitment posture is on for all AEAD sites (seat 27 pattern).
/// A [`Kek`] is type-stopped here — DEK↔KEK swap is unconstructable.
fn seal_arm(
    arm: AeadArm,
    key: &Dek,
    nonce: &Nonce,
    aad: &[u8],
    plaintext: &[u8],
    crypto_domain: CryptoDomain,
) -> Result<Vec<u8>, CryptoRefuse> {
    seal_arm_committed(arm, key.as_bytes(), nonce, aad, plaintext, crypto_domain)
}

/// Committing-AEAD seal under a KEK — shred-salt wrap door. [`Dek`] cannot enter.
fn seal_arm_kek(
    arm: AeadArm,
    key: &Kek,
    nonce: &Nonce,
    aad: &[u8],
    plaintext: &[u8],
    crypto_domain: CryptoDomain,
) -> Result<Vec<u8>, CryptoRefuse> {
    seal_arm_committed(arm, key.as_bytes(), nonce, aad, plaintext, crypto_domain)
}

/// ONE committing-AEAD open body — RustCrypto / `.as_bytes()` edge only.
///
/// Base arm open, then constant-time CMT-1 key-commitment check.
/// On commitment mismatch returns [`CryptoRefuse::KeyCommitmentMismatch`] and
/// does not release the AEAD plaintext.
/// Callers must pass [`Dek::as_bytes`] or [`Kek::as_bytes`]; typed doors stay split.
fn open_arm_committed(
    arm: AeadArm,
    key: &[u8; 32],
    nonce: &Nonce,
    aad: &[u8],
    sealed: &[u8],
    crypto_domain: CryptoDomain,
) -> Result<Vec<u8>, CryptoRefuse> {
    if sealed.len() < AEAD_TAG_LEN + KEY_COMMIT_LEN {
        return Err(CryptoRefuse::AeadFailed);
    }
    let split = sealed.len() - KEY_COMMIT_LEN;
    let (aead_body, presented_c) = sealed.split_at(split);
    let plaintext = open_aead_arm_bytes(arm, key, nonce, aad, aead_body)?;
    let expected = key_commitment_bytes(key, crypto_domain)?;
    let mut presented = [0u8; KEY_COMMIT_LEN];
    presented.copy_from_slice(presented_c);
    if !ct_eq_digest(&expected, &Digest::from_bytes(presented)) {
        // never release plaintext on commitment mismatch
        drop(plaintext);
        return Err(CryptoRefuse::KeyCommitmentMismatch);
    }
    Ok(plaintext)
}

/// Committing-AEAD open under a DEK: thin typed door over [`open_arm_committed`].
///
/// A [`Kek`] is type-stopped here.
fn open_arm(
    arm: AeadArm,
    key: &Dek,
    nonce: &Nonce,
    aad: &[u8],
    sealed: &[u8],
    crypto_domain: CryptoDomain,
) -> Result<Vec<u8>, CryptoRefuse> {
    open_arm_committed(arm, key.as_bytes(), nonce, aad, sealed, crypto_domain)
}

/// Committing-AEAD open under a KEK — shred-salt unwrap door. [`Dek`] cannot enter.
fn open_arm_kek(
    arm: AeadArm,
    key: &Kek,
    nonce: &Nonce,
    aad: &[u8],
    sealed: &[u8],
    crypto_domain: CryptoDomain,
) -> Result<Vec<u8>, CryptoRefuse> {
    open_arm_committed(arm, key.as_bytes(), nonce, aad, sealed, crypto_domain)
}

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
    let aad = shred_salt_wrap_transcript(crypto_domain, segment)?;
    let nonce = wrap_nonce(&aad);
    // KeyCommitment posture on for all AEAD sites — wrap uses SIV + CMT-1 under KEK.
    let body = seal_arm_kek(
        AeadArm::Siv,
        cap.kek(),
        &nonce,
        aad.as_bytes(),
        salt.as_bytes(),
        crypto_domain,
    )?;
    Ok(WrappedShredSalt {
        ciphertext: body,
        segment,
        crypto_domain,
    })
}

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
    let aad = shred_salt_wrap_transcript(wrapped.crypto_domain, wrapped.segment)?;
    let nonce = wrap_nonce(&aad);
    let pt = match open_arm_kek(
        AeadArm::Siv,
        cap.kek(),
        &nonce,
        aad.as_bytes(),
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
    nonce: Nonce,
    body: Vec<u8>,
}

impl Ciphertext {
    /// AEAD arm used.
    pub fn arm(&self) -> AeadArm {
        self.arm
    }

    /// Nonce sealed into the ciphertext.
    pub fn nonce(&self) -> &Nonce {
        &self.nonce
    }

    /// Ciphertext body (AEAD ciphertext ‖ tag ‖ key-commitment).
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// Compress plaintext with pure-Rust LZ4 (`lz4_flex`). Must precede AEAD (§67).
pub fn compress(plaintext: &[u8]) -> CompressedBytes {
    CompressedBytes(lz4_flex::compress_prepend_size(plaintext))
}

/// Decompress LZ4 size-prepended bytes from [`compress`].
pub fn decompress(compressed: &CompressedBytes) -> Result<Vec<u8>, CryptoRefuse> {
    lz4_flex::decompress_size_prepended(compressed.as_bytes())
        .map_err(|_| CryptoRefuse::DecompressFailed)
}

/// Encrypt compressed bytes under a DEK + nonce + arm.
///
/// Only accepts [`CompressedBytes`] — compression precedes AEAD by construction.
/// [`AeadArm::Siv`] → AES-256-GCM-SIV; [`AeadArm::Gcm`] → ChaCha20-Poly1305.
pub fn encrypt(
    compressed: CompressedBytes,
    dek: &Dek,
    nonce: Nonce,
    arm: AeadArm,
    aad: &CanonicalTranscript,
) -> Result<Ciphertext, CryptoRefuse> {
    let body = seal_arm(
        arm,
        dek,
        &nonce,
        aad.as_bytes(),
        compressed.as_bytes(),
        dek.crypto_domain(),
    )?;
    Ok(Ciphertext { arm, nonce, body })
}

/// Open AEAD ciphertext back to [`CompressedBytes`].
pub fn decrypt(
    ciphertext: &Ciphertext,
    dek: &Dek,
    aad: &CanonicalTranscript,
) -> Result<CompressedBytes, CryptoRefuse> {
    let pt = open_arm(
        ciphertext.arm,
        dek,
        &ciphertext.nonce,
        aad.as_bytes(),
        &ciphertext.body,
        dek.crypto_domain(),
    )?;
    Ok(CompressedBytes(pt))
}

/// Compression-then-encryption pipeline — the only Store path (§67).
pub fn compress_then_encrypt(
    plaintext: &[u8],
    dek: &Dek,
    nonce: Nonce,
    arm: AeadArm,
    aad: &CanonicalTranscript,
) -> Result<Ciphertext, CryptoRefuse> {
    encrypt(compress(plaintext), dek, nonce, arm, aad)
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

/// Durable shred tombstone naming one shredded (CryptoDomain, SegmentCounter).
///
/// Survives so post-shred restore of an old pack that still carries the wrapped
/// ciphertext converges to [`CryptoRefuse::Shredded`], not silent unreadability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShredTombstone {
    segment: SegmentCounter,
    crypto_domain: CryptoDomain,
}

impl ShredTombstone {
    /// Segment this tombstone revokes.
    pub fn segment(self) -> SegmentCounter {
        self.segment
    }

    /// Crypto domain this tombstone revokes under.
    pub fn crypto_domain(self) -> CryptoDomain {
        self.crypto_domain
    }

    /// Whether this tombstone covers a wrapped salt handle.
    pub fn covers(self, wrapped: &WrappedShredSalt) -> bool {
        self.segment == wrapped.segment && self.crypto_domain == wrapped.crypto_domain
    }
}

/// Ledger of shredded segments consulted on unwrap / leave-is-free restore.
#[derive(Debug, Clone)]
pub struct ShredLedger {
    /// (store_id, fence_epoch, segment) keys revoked by shred.
    keys: HashSet<([u8; 32], u64, u64)>,
}

impl ShredLedger {
    /// Empty ledger — no segments shredded.
    pub fn new() -> Self {
        Self {
            keys: HashSet::new(),
        }
    }

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
    use miette::{IntoDiagnostic, Result, miette};
    use super::*;
    use crate::store::contract::FormatVersion;
    use crate::store::epoch::FenceEpoch;
    use crate::store::open::StoreId;
    use crate::store::transcript::{
        CanonicalTranscriptBuilder, FieldId, SealedArtifactKind, WRAPPED_SHRED_SALT_AAD_GOLDEN_VEC,
        encode_wrapped_shred_salt_aad, parse_golden_hex,
    };

    /// GUARDIAN GATE (#376 T2) — CMT-1 key-commitment over the Gcm collision.
    /// Empirically constructed (invisible-salamanders / Partitioning Oracle Attacks): ONE
    /// (ciphertext ‖ tag) that the base ChaCha20-Poly1305 AEAD accepts under TWO distinct
    /// keys, under a fixed AAD byte string the collision was solved against. The collision
    /// block was solved over GF(2^130-5) so both keys' Poly1305 tags coincide. With CMT-1
    /// via CanonicalTranscript, C binds only the opening key (+ CryptoDomain): present
    /// (ct‖tag‖C_K1) → opens under K1 and refuses under K2 with
    /// [`CryptoRefuse::KeyCommitmentMismatch`] (K2 recomputes C_K2 ≠ C_K1). Exercises
    /// two-key rejection through production [`open_arm`], not a short-input refuse.
    /// AAD bytes below are collision-fixture constants — not the production wrap path.
    #[test]
    fn gcm_arm_is_not_key_committing() -> Result<()> {
        let k1 = Kek::from_bytes([0x11u8; 32]);
        let k2 = Kek::from_bytes([0x22u8; 32]);
        let nonce = Nonce::from_bytes([0x24u8; 12]);
        let domain = test_domain();
        // Collision-fixture AAD (fixed historical bytes the Poly1305 collision targets).
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
            open_aead_arm_kek(AeadArm::Gcm, &k1, &nonce, &aad, &aead_body).is_ok()
                && open_aead_arm_kek(AeadArm::Gcm, &k2, &nonce, &aad, &aead_body).is_ok(),
            "fixture must still be a two-key Poly1305 collision under the base Gcm arm"
        );
        // Commit under K1 via CanonicalTranscript CMT-1; present (ct‖tag‖C_K1).
        let c_k1 = key_commitment_kek(&k1, domain)?;
        let mut msg = aead_body;
        msg.extend_from_slice(c_k1.as_bytes());
        let o1 = open_arm_kek(AeadArm::Gcm, &k1, &nonce, &aad, &msg, domain);
        let o2 = open_arm_kek(AeadArm::Gcm, &k2, &nonce, &aad, &msg, domain);
        assert!(
            o1.is_ok(),
            "committed ciphertext must open under the committing key K1"
        );
        assert!(
            matches!(o2, Err(CryptoRefuse::KeyCommitmentMismatch)),
            "production open_arm_kek must refuse the K1-committed collision under K2 with \
             KeyCommitmentMismatch (got {o2:?})"
        );
    
        Ok(())
    }

    fn test_domain() -> CryptoDomain {
        let store = StoreId::from_digest([0xAB; 32]);
        CryptoDomain::new(store, FenceEpoch::genesis(store))
    }

    fn test_aad() -> Result<CanonicalTranscript> {
        let mut b = CanonicalTranscriptBuilder::new(FormatVersion::CURRENT)?;
        b.append_u64(
            FieldId::ARTIFACT_KIND,
            SealedArtifactKind::AuditKeyLeaf.tag(),
        )?;
        Ok(b.seal())
    }

    #[test]
    fn wrap_unwrap_round_trip_and_derive() -> Result<()> {
        let kek = Kek::from_bytes([0x11; 32]);
        let cap = KekUnwrapCap::from_kek(kek);
        let salt = ShredSalt::from_bytes([0x22; 32]);
        let seg = SegmentCounter::from_raw(7);
        let domain = test_domain();
        let wrapped = wrap_shred_salt(&cap, &salt, seg, domain)?;
        let ledger = ShredLedger::new();
        let opened = unwrap_shred_salt(&cap, &wrapped, &ledger)?;
        let dek = derive_dek(&cap, domain, seg, &opened);
        assert_eq!(dek.crypto_domain(), domain);
        let (receipt, tombstone) = shred(wrapped);
        assert_eq!(receipt.segment(), seg);
        assert!(tombstone.covers(&WrappedShredSalt::from_persisted(vec![0], seg, domain)));
    
        Ok(())
    }

    #[test]
    fn post_shred_unwrap_refuses_shredded() -> Result<()> {
        let kek = Kek::from_bytes([0x11; 32]);
        let cap = KekUnwrapCap::from_kek(kek);
        let salt = ShredSalt::from_bytes([0x22; 32]);
        let seg = SegmentCounter::from_raw(3);
        let domain = test_domain();
        let wrapped = wrap_shred_salt(&cap, &salt, seg, domain)?;
        // Old pack still holds a copy of the wrapped ciphertext.
        let stale_copy = wrapped.clone();
        let (_receipt, tombstone) = shred(wrapped);
        let mut ledger = ShredLedger::new();
        ledger.record(tombstone);
        assert!(matches!(
            unwrap_shred_salt(&cap, &stale_copy, &ledger),
            Err(CryptoRefuse::Shredded)
        ));
    
        Ok(())
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
    fn cross_domain_wrapped_salt_reinterpretation_refuses() -> Result<()> {
        let cap = KekUnwrapCap::from_kek(Kek::from_bytes([0x77; 32])); // SAME KEK throughout
        let salt = ShredSalt::from_bytes([0x99; 32]);
        let seg = SegmentCounter::from_raw(4);
        let ledger = ShredLedger::new();

        let store_a = StoreId::from_digest([0xA1; 32]);
        let store_b = StoreId::from_digest([0xB2; 32]);
        let domain_a = CryptoDomain::new(store_a, FenceEpoch::genesis(store_a));

        // Sanity: wrap+unwrap under the TRUE domain succeeds (not a blanket lockout).
        let wrapped = wrap_shred_salt(&cap, &salt, seg, domain_a)?;
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
    
        Ok(())
    }

    #[test]
    fn compress_then_encrypt_round_trips_siv_and_is_not_identity() -> Result<()> {
        let kek = Kek::from_bytes([0x33; 32]);
        let cap = KekUnwrapCap::from_kek(kek);
        let salt = ShredSalt::from_bytes([0x44; 32]);
        let domain = test_domain();
        let dek = derive_dek(&cap, domain, SegmentCounter::ZERO, &salt);
        let aad = test_aad()?;
        let plaintext = b"hello compress-then-encrypt pipeline";
        let compressed = compress(plaintext);
        assert_ne!(
            compressed.as_bytes(),
            plaintext,
            "compress must not be a silent identity no-op"
        );
        let ct = compress_then_encrypt(
            plaintext,
            &dek,
            Nonce::from_bytes([9u8; 12]),
            AeadArm::Siv,
            &aad,
        )?;
        assert_eq!(ct.arm(), AeadArm::Siv);
        assert!(!ct.body().is_empty());
        let opened = decrypt(&ct, &dek, &aad)?;
        let round = decompress(&opened)?;
        assert_eq!(round, plaintext);
    
        Ok(())
    }

    #[test]
    fn gcm_arm_uses_chacha20poly1305() -> Result<()> {
        let kek = Kek::from_bytes([0x55; 32]);
        let cap = KekUnwrapCap::from_kek(kek);
        let salt = ShredSalt::from_bytes([0x66; 32]);
        let domain = test_domain();
        let dek = derive_dek(&cap, domain, SegmentCounter::ZERO, &salt);
        let aad = test_aad()?;
        let ct = compress_then_encrypt(
            b"gcm-arm",
            &dek,
            Nonce::from_bytes([1u8; 12]),
            AeadArm::Gcm,
            &aad,
        )?;
        assert_eq!(ct.arm(), AeadArm::Gcm);
        let opened = decrypt(&ct, &dek, &aad)?;
        assert_eq!(decompress(&opened)?, b"gcm-arm");
    
        Ok(())
    }

    /// RED-first (#376 T17): Mac / Digest / Nonce / Signature / Dek / Kek are
    /// distinct newtypes — a MAC cannot be passed where a Digest is required,
    /// encrypt doors take `&Dek`, wrap doors take `&Kek` / `KekUnwrapCap`, and
    /// neither role satisfies the other's door (no shared AeadKeyBytes trait).
    #[test]
    fn wrong_kind_fixed_arrays_are_unconstructible_as_peer_kinds() -> Result<()> {
        let domain = test_domain();
        let aad = test_aad()?;
        let audit = AuditKey::from_bytes([0xABu8; 32]);
        let mac: Mac = audit.leaf_mac(&aad);
        assert_eq!(std::mem::size_of_val(&mac), 32);
        // Digest from CMT-1 KEK door is not a Mac; Mac has no From/Into Digest bridge.
        let kek = Kek::from_bytes([0x11; 32]);
        let digest: Digest = key_commitment_kek(&kek, domain)?;
        assert_eq!(std::mem::size_of_val(&digest), 32);
        assert_ne!(
            std::any::type_name::<Mac>(),
            std::any::type_name::<Digest>(),
            "Mac and Digest must remain distinct types"
        );
        assert_ne!(
            std::any::type_name::<Nonce>(),
            std::any::type_name::<Digest>(),
            "Nonce and Digest must remain distinct types"
        );
        assert_ne!(
            std::any::type_name::<Signature>(),
            std::any::type_name::<Digest>(),
            "Signature and Digest must remain distinct types"
        );
        assert_ne!(
            std::any::type_name::<Dek>(),
            std::any::type_name::<Kek>(),
            "Dek and Kek must remain distinct types"
        );
        // Wrap-derived nonce is typed Nonce, not a free [u8;12].
        let wrap_aad = encode_wrapped_shred_salt_aad(domain, SegmentCounter::ZERO.get())?;
        let n: Nonce = wrap_nonce(&wrap_aad);
        // encrypt door takes &Dek + Nonce — naked [u8;12] / &Kek are type errors.
        let salt = ShredSalt::from_bytes([0x22; 32]);
        let cap = KekUnwrapCap::from_kek(Kek::from_bytes([0x33; 32]));
        let dek = derive_dek(&cap, domain, SegmentCounter::ZERO, &salt);
        let ct = compress_then_encrypt(b"typed-nonce", &dek, n, AeadArm::Siv, &aad)?;
        assert_eq!(ct.nonce(), &n);
    
        Ok(())
    }

    /// Grep gate obligation (#376 T17): named crypto/auth doors must not take or
    /// return naked `[u8;32]` / `[u8;12]` / `[u8;64]`, and DEK/KEK doors must be
    /// role-split (no shared `AeadKeyBytes` / `impl AeadKeyBytes` door).
    ///
    /// Parent escalate: durable guardian copy belongs in `crates/xtask` (off
    /// allowlist) scanning the full store surface — this pin locks the T17
    /// doors until that lands.
    #[test]
    fn t17_no_naked_fixed_arrays_in_auth_fn_signatures() -> Result<()> {
        let crypto = include_str!("crypto.rs");
        let grants = include_str!("grants.rs");
        let replica = include_str!("replica.rs");
        let crypto_prod = match crypto.split("#[cfg(test)]").next() {
            Some(prod) => prod,
            None => crypto,
        };

        // RustCrypto edge may take naked arrays; committing-AEAD role doors must not.
        for name in [
            "fn seal_arm(",
            "fn open_arm(",
            "fn key_commitment(",
            "fn seal_arm_kek(",
            "fn open_arm_kek(",
            "fn key_commitment_kek(",
            "fn wrap_nonce(",
        ] {
            let Some(line) = crypto_prod
                .lines()
                .find(|l| l.trim_start().starts_with(name) || l.contains(name))
            else {
                assert!(false, "missing door {name}");
                return Ok(());
            };
            for needle in ["[u8; 32]", "[u8; 12]", "[u8; 64]"] {
                assert!(
                    !line.contains(needle),
                    "crypto.rs: {name} still exposes naked {needle}: {line}"
                );
            }
            assert!(
                !line.contains("AeadKeyBytes") && !line.contains("impl AeadKeyBytes"),
                "crypto.rs: {name} must not use AeadKeyBytes: {line}"
            );
        }
        assert!(
            !crypto_prod.contains("trait AeadKeyBytes")
                && !crypto_prod.contains("impl AeadKeyBytes")
                && !crypto_prod.contains("&impl AeadKeyBytes"),
            "AeadKeyBytes shared door must be gone — DEK and KEK need distinct typed doors"
        );
        assert!(
            crypto_prod.contains("fn key_commitment(key: &Dek, crypto_domain: CryptoDomain)")
                && crypto_prod.contains("fn seal_arm(")
                && crypto_prod.contains("key: &Dek,"),
            "encrypt-path commitment/seal doors must take &Dek"
        );
        assert!(
            crypto_prod.contains("fn key_commitment_kek(key: &Kek, crypto_domain: CryptoDomain)")
                && crypto_prod.contains("fn seal_arm_kek(")
                && crypto_prod.contains("fn open_arm_kek("),
            "wrap-path commitment/seal/open doors must take &Kek"
        );
        // ONE committed body each; typed doors stay thin wrappers (copy_detector).
        assert!(
            crypto_prod.contains("fn open_arm_committed(")
                && crypto_prod.contains("fn seal_arm_committed(")
                && crypto_prod.contains("key: &[u8; 32],"),
            "shared open_arm_committed / seal_arm_committed must own commit logic at the bytes edge"
        );
        assert!(
            crypto_prod.contains("open_arm_committed(arm, key.as_bytes(),")
                && crypto_prod.contains("seal_arm_committed(arm, key.as_bytes(),"),
            "open_arm/open_arm_kek and seal_arm/seal_arm_kek must delegate to the shared bodies"
        );
        // Wrapper bodies must not re-implement the CMT-1 check / append loop.
        for (door, needle) in [
            ("fn open_arm(", "KEY_COMMIT_LEN"),
            ("fn open_arm_kek(", "KEY_COMMIT_LEN"),
            ("fn seal_arm(", "key_commitment("),
            ("fn seal_arm_kek(", "key_commitment_kek("),
        ] {
            let start = crypto_prod
                .find(door).ok_or_else(|| miette!("missing door"))?;
            let body = &crypto_prod[start..];
            let end = match body.find("
fn ").or_else(|| body.find("
pub fn ")) {
                Some(i) => i,
                None => body.len(),
            }
            .min(400);
            let wrapper = &body[..end];
            assert!(
                !wrapper.contains(needle),
                "{door} must not re-implement commit logic ({needle}): {wrapper}"
            );
        }
        assert!(
            crypto_prod.contains("pub fn encrypt(")
                && crypto_prod.contains("dek: &Dek,")
                && crypto_prod.contains("nonce: Nonce"),
            "encrypt must take &Dek + Nonce (not AeadKeyBytes)"
        );
        assert!(
            crypto_prod.contains("pub fn wrap_shred_salt(")
                && crypto_prod.contains("cap: &KekUnwrapCap,")
                && crypto_prod.contains("seal_arm_kek(")
                && crypto_prod.contains("open_arm_kek("),
            "wrap/unwrap must route through KekUnwrapCap + *_kek doors"
        );
        assert!(
            crypto_prod.contains("fn wrap_nonce(transcript: &CanonicalTranscript) -> Nonce"),
            "wrap_nonce must digest CanonicalTranscript and return Nonce"
        );
        assert!(
            crypto_prod.contains("encode_wrapped_shred_salt_aad(")
                && crypto_prod.contains("fn shred_salt_wrap_transcript(")
                && !crypto_prod.contains("fn wrap_aad("),
            "wrap AAD must route through encode_wrapped_shred_salt_aad; hand-rolled wrap_aad gone"
        );
        assert!(
            !crypto_prod.contains("kyzo.wrap.shred_salt.nonce.v1"),
            "wrap_nonce must not hand-frame a second domain-label serialization"
        );
        assert!(
            crypto_prod.contains("pub fn leaf_mac(&self, transcript: &CanonicalTranscript) -> Mac"),
            "leaf_mac must return Mac"
        );

        let grants_prod = match grants.split("#[cfg(test)]").next() {
            Some(prod) => prod,
            None => grants,
        };
        assert!(
            grants_prod.contains("fn hash_transcript(transcript: &CanonicalTranscript) -> Digest"),
            "hash_transcript must return Digest"
        );
        assert!(
            grants_prod.contains("fn consent_key_id_digest(verifying_key: &[u8; 32]) -> Digest"),
            "consent_key_id_digest must return Digest"
        );
        assert!(
            grants_prod.contains("payload_digest: &Digest")
                && grants_prod.contains("signature: &Signature"),
            "consent/entitlement verify doors must take Digest + Signature"
        );

        let replica_prod = match replica.split("#[cfg(test)]").next() {
            Some(prod) => prod,
            None => replica,
        };
        assert!(
            replica_prod.contains("pub(crate) fn sign(&self, body: &Digest) -> Result<Signature, ReplicaRefuse>"),
            "AuthorizingKey::sign must take Digest and return Signature"
        );
        assert!(
            replica_prod.contains(
                "pub(crate) fn verify_signature(&self, body: &Digest, signature: &Signature) -> bool"
            ),
            "AuthorizingKey::verify_signature must take Digest + Signature"
        );
        assert!(
            replica_prod.contains(") -> Digest")
                && replica_prod.contains("fn signing_body_digest("),
            "signing_body_digest must return Digest"
        );
    
        Ok(())
    }

    /// RED-first (#376 T18 / seat 59): wrap AAD is the ONE CanonicalTranscript path;
    /// production encode matches the independent golden; hand-rolled WSS1 framing is gone
    /// from production; wrap/unwrap still round-trips; main encrypt AAD remains
    /// `&CanonicalTranscript` only.
    #[test]
    fn t18_wrap_aad_is_sole_canonical_transcript_path() -> Result<()> {
        let store = StoreId::from_digest([0x11; 32]);
        let domain = CryptoDomain::new(store, FenceEpoch::genesis(store));
        let production = encode_wrapped_shred_salt_aad(domain, 0)?;
        let golden = parse_golden_hex(WRAPPED_SHRED_SALT_AAD_GOLDEN_VEC)?;
        assert_eq!(
            production.as_bytes(),
            golden.as_slice(),
            "production encode_wrapped_shred_salt_aad must match independent golden"
        );

        let crypto = include_str!("crypto.rs");
        let crypto_prod = match crypto.split("#[cfg(test)]").next() {
            Some(prod) => prod,
            None => crypto,
        };
        assert!(
            !crypto_prod.contains("fn wrap_aad("),
            "hand-rolled wrap_aad must be deleted from production"
        );
        // Split so this gate body never contains a contiguous production WSS1 concat needle.
        let wss1_concat = ["extend_from_slice(b\"", "WSS1\")"].concat();
        assert!(
            !crypto_prod.contains(wss1_concat.as_str()),
            "production must not hand-concat WSS1 wrap AAD bytes"
        );
        assert!(
            crypto_prod.contains("aad: &CanonicalTranscript")
                && crypto_prod.contains("pub fn encrypt(")
                && crypto_prod.contains("aad.as_bytes()"),
            "main encrypt AAD must remain sole CanonicalTranscript path"
        );

        let kek = Kek::from_bytes([0x11; 32]);
        let cap = KekUnwrapCap::from_kek(kek);
        let salt = ShredSalt::from_bytes([0x22; 32]);
        let seg = SegmentCounter::from_raw(7);
        let wrap_domain = test_domain();
        let wrapped = wrap_shred_salt(&cap, &salt, seg, wrap_domain)?;
        let ledger = ShredLedger::new();
        let opened = unwrap_shred_salt(&cap, &wrapped, &ledger)?;
        let dek = derive_dek(&cap, wrap_domain, seg, &opened);
        assert_eq!(dek.crypto_domain(), wrap_domain);

        // Nonce is deterministic bytes-from-transcript-digest (SIV misuse-resistant).
        let aad = shred_salt_wrap_transcript(wrap_domain, seg)?;
        let n1 = wrap_nonce(&aad);
        let n2 = wrap_nonce(&aad);
        assert_eq!(n1, n2);
    
        Ok(())
    }
}
