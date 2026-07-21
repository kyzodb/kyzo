/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Stored relation metadata and whole-schema compatibility proofs.

use miette::{Diagnostic, Result, bail};
use thiserror::Error;

use super::column::{ColType, ColumnDef, NullableColType};

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
pub struct StoredRelationMetadata {
    pub keys: Vec<ColumnDef>,
    pub non_keys: Vec<ColumnDef>,
}

/// Write shape that decides which stored columns an input must satisfy.
///
/// Constructed by the mutation / catalog tier when proving an input schema
/// against stored metadata via [`CompatibleInputSchema::prove`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum RelationWriteShape {
    /// Full put: every stored key and non-key must be provided (or defaulted).
    Put,
    /// Removal or update: only stored keys must be provided (or defaulted).
    RemoveOrUpdate,
}

/// Branded proof that an input schema is compatible with a stored schema
/// for one [`RelationWriteShape`]. The type *is* the certificate — mint it
/// only through [`CompatibleInputSchema::prove`], which constructs whole
/// or refuses whole.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CompatibleInputSchema {
    _private: (),
}

impl CompatibleInputSchema {
    /// Prove that `input` may write against `stored` under `shape`.
    ///
    /// All column obligations are discharged inside this constructor: either
    /// every obligation holds and a proof is returned, or the whole schema
    /// is refused. Callers never approve columns one at a time.
    pub fn prove(
        stored: &StoredRelationMetadata,
        input: &StoredRelationMetadata,
        shape: RelationWriteShape,
    ) -> Result<Self> {
        for col in input.keys.iter().chain(input.non_keys.iter()) {
            stored.require_compatible_column(col)?;
        }
        for col in &stored.keys {
            input.require_provides(col)?;
        }
        if matches!(shape, RelationWriteShape::Put) {
            for col in &stored.non_keys {
                input.require_provides(col)?;
            }
        }
        Ok(Self { _private: () })
    }
}

impl StoredRelationMetadata {
    /// Private whole-proof helper: this schema provides `col` (by name) or
    /// `col` carries a default. Not a public approval surface.
    fn require_provides(&self, col: &ColumnDef) -> Result<()> {
        for target in self.keys.iter().chain(self.non_keys.iter()) {
            if target.name == col.name {
                return Ok(());
            }
        }
        if col.default_gen.is_none() {
            #[derive(Debug, Error, Diagnostic)]
            #[error("required column {0} not provided by input")]
            #[diagnostic(code(eval::required_col_not_provided))]
            struct ColumnNotProvided(String);

            bail!(ColumnNotProvided(col.name.to_string()))
        }
        Ok(())
    }

    /// Private whole-proof helper: `col` names a column here with compatible
    /// typing (`Any?` remains a wildcard). Not a public approval surface.
    fn require_compatible_column(&self, col: &ColumnDef) -> Result<()> {
        for target in self.keys.iter().chain(self.non_keys.iter()) {
            if target.name == col.name {
                #[derive(Debug, Error, Diagnostic)]
                #[error("requested column {0} has typing {1}, but the requested typing is {2}")]
                #[diagnostic(code(eval::col_type_mismatch))]
                struct IncompatibleTyping(String, NullableColType, NullableColType);
                if (!col.typing.is_nullable() || *col.typing.coltype() != ColType::Any)
                    && target.typing != col.typing
                {
                    bail!(IncompatibleTyping(
                        col.name.to_string(),
                        target.typing.clone(),
                        col.typing.clone()
                    ))
                }

                return Ok(());
            }
        }

        #[derive(Debug, Error, Diagnostic)]
        #[error("required column {0} not found")]
        #[diagnostic(code(eval::required_col_not_found))]
        struct ColumnNotFound(String);

        bail!(ColumnNotFound(col.name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::column::{ColType, ColumnDef, NullableColType};
    use smartstring::SmartString;

    fn col(name: &str, ty: ColType) -> ColumnDef {
        ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType::required(ty),
            default_gen: None,
        }
    }

    #[test]
    fn put_requires_keys_and_non_keys_remove_only_keys() {
        let stored = StoredRelationMetadata {
            keys: vec![col("k", ColType::Int)],
            non_keys: vec![col("v", ColType::String)],
        };
        let full = StoredRelationMetadata {
            keys: vec![col("k", ColType::Int)],
            non_keys: vec![col("v", ColType::String)],
        };
        assert!(CompatibleInputSchema::prove(&stored, &full, RelationWriteShape::Put).is_ok());
        let keys_only = StoredRelationMetadata {
            keys: vec![col("k", ColType::Int)],
            non_keys: vec![],
        };
        assert!(
            CompatibleInputSchema::prove(&stored, &keys_only, RelationWriteShape::Put).is_err(),
            "Put without non-key must refuse"
        );
        assert!(
            CompatibleInputSchema::prove(&stored, &keys_only, RelationWriteShape::RemoveOrUpdate)
                .is_ok(),
            "RemoveOrUpdate needs only keys"
        );
    }

    #[test]
    fn type_mismatch_refuses_compatible_column() {
        let stored = StoredRelationMetadata {
            keys: vec![col("k", ColType::Int)],
            non_keys: vec![],
        };
        let wrong = StoredRelationMetadata {
            keys: vec![col("k", ColType::String)],
            non_keys: vec![],
        };
        assert!(CompatibleInputSchema::prove(&stored, &wrong, RelationWriteShape::Put).is_err());
    }
}
