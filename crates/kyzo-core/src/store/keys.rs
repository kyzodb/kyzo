/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Closed Secret key class (decisions.md §61).
//!
//! Owns: the closed [`Secret`] class — DEKs, KEKs, passwords, bearer tokens,
//! raw private keys, and credential-marked material.
//!
//! Bans: open-ended class growth without a Spec edit; secrets in any
//! indexed / memcmp key (admission → `Refuse(SecretInIndexedKey)`).
//!
//! Order law forces plaintext keys; secrets in keys are a permanent memcmp
//! tax and leak. That tax is published here so it cannot be rediscovered as
//! folklore.

/// Closed class of engine-illegal indexed-key material (§61).
///
/// Every variant is refused by admission when placed in an indexed / memcmp
/// key. Extending this enum requires a Spec edit — open-ended growth is banned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Secret {
    /// Data-encryption key material.
    Dek,
    /// Key-encryption key material.
    Kek,
    /// Password / passphrase material.
    Password,
    /// Bearer token material.
    BearerToken,
    /// Raw private key bytes.
    RawPrivateKey,
    /// Any credential material explicitly marked Secret.
    CredentialMarked,
}

impl Secret {
    /// Published memcmp-tax note: secrets in indexed keys are permanent leak.
    pub const MEMCMP_TAX_NOTE: &'static str = "INVARIANT(Secret): order law forces plaintext \
         indexed keys; Secret material in any memcmp key is a permanent tax and leak — \
         admission refuses SecretInIndexedKey (§61).";

    /// Stable tag for refuse / ledger surfaces.
    pub fn tag(self) -> &'static str {
        match self {
            Secret::Dek => "dek",
            Secret::Kek => "kek",
            Secret::Password => "password",
            Secret::BearerToken => "bearer_token",
            Secret::RawPrivateKey => "raw_private_key",
            Secret::CredentialMarked => "credential_marked",
        }
    }
}

/// Typed refuse when Secret material is proposed for an indexed key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum SecretKeyRefuse {
    #[error("Secret material illegal in indexed/memcmp key ({})", .0.tag())]
    #[diagnostic(code(store::keys::secret_in_indexed_key))]
    SecretInIndexedKey(Secret),
}

/// Classify whether a proposed key-plane marking is Secret-class.
///
/// Admission calls this; a `Some(Secret)` → [`SecretKeyRefuse::SecretInIndexedKey`].
pub fn refuse_if_secret(marking: Option<Secret>) -> Result<(), SecretKeyRefuse> {
    match marking {
        None => Ok(()),
        Some(s) => Err(SecretKeyRefuse::SecretInIndexedKey(s)),
    }
}

#[cfg(test)]
mod pins {
    use super::*;

    #[test]
    fn closed_class_has_six_variants() {
        let all = [
            Secret::Dek,
            Secret::Kek,
            Secret::Password,
            Secret::BearerToken,
            Secret::RawPrivateKey,
            Secret::CredentialMarked,
        ];
        assert_eq!(all.len(), 6);
        assert!(Secret::MEMCMP_TAX_NOTE.contains("SecretInIndexedKey"));
        assert!(matches!(
            refuse_if_secret(Some(Secret::Dek)),
            Err(SecretKeyRefuse::SecretInIndexedKey(Secret::Dek))
        ));
        assert!(refuse_if_secret(None).is_ok());
    }
}
