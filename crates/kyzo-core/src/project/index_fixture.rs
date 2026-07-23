/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Projection-index test fixture scaffold — ONE seat for base+index create/commit.
//!
//! Sparse/spatial/FTS (and hostile) suites used to paste the same
//! `write_tx` / `create_relation` / `commit` body. That was a second
//! authority by copy-paste (copy_detector). Callers seed rows through the
//! callback; they do not re-own the scaffold.

use crate::session::catalog::{KeyspaceKind, RelationHandle, create_relation};
use crate::store::{Storage, WriteTx};
use kyzo_model::program::InputRelationHandle;
use kyzo_model::schema::StoredRelationMetadata;
use miette::{Result, miette};

/// Open a write tx, create base (Facts) + index (AlgorithmState), run `seed`, commit.
pub(crate) fn seed_base_and_index<S, F>(
    db: &S,
    base_name: &str,
    idx_name: &str,
    meta: StoredRelationMetadata,
    idx_meta: StoredRelationMetadata,
    mut seed: F,
) -> Result<(RelationHandle, RelationHandle)>
where
    S: Storage,
    F: FnMut(&mut S::WriteTx, &RelationHandle, &RelationHandle) -> Result<()>,
{
    let mut tx = db.write_tx()?;
    let base = create_relation(
        &mut tx,
        InputRelationHandle::from_metadata(base_name, meta),
        KeyspaceKind::Facts,
    )?;
    let idx = create_relation(
        &mut tx,
        InputRelationHandle::from_metadata(idx_name, idx_meta),
        KeyspaceKind::AlgorithmState,
    )?;
    seed(&mut tx, &base, &idx)?;
    tx.commit().map_err(|e| miette!("{e}"))?;
    Ok((base, idx))
}
