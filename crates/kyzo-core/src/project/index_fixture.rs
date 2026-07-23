/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Projection-index test fixture scaffold — ONE seat for base+index create/commit,
//! fact+index seed loops, corrupt-posting poison, and pinned msgpack manifests.

use crate::session::catalog::{KeyspaceKind, RelationHandle, create_relation};
use crate::store::tx::Slice;
use crate::store::{Storage, WriteTx};
use kyzo_model::program::InputRelationHandle;
use kyzo_model::schema::StoredRelationMetadata;
use kyzo_model::value::{DataValue, SourceSpan};
use miette::{Result, miette};
use serde::Serialize;
use serde::de::DeserializeOwned;

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

pub(crate) fn seed_fact_index_rows<T, R, F>(
    tx: &mut T,
    base: &RelationHandle,
    idx: &RelationHandle,
    rows: R,
    mut index_put: F,
) -> Result<()>
where
    T: WriteTx,
    R: IntoIterator<Item = Vec<DataValue>>,
    F: FnMut(&mut T, &[DataValue], &RelationHandle, &RelationHandle) -> Result<()>,
{
    for row in rows {
        base.put_fact(
            tx,
            &row,
            kyzo_model::value::ValidityTs::of_micros(0),
            SourceSpan(0, 0),
        )?;
        index_put(tx, &row, base, idx)?;
    }
    Ok(())
}

pub(crate) fn poison_index_values_with_reserved_msgpack<T: WriteTx>(
    tx: &mut T,
    idx: &RelationHandle,
) -> Result<()> {
    let lower = kyzo_model::value::encode_key_with_suffix(idx.id, &[], &[]);
    let upper = (idx.id.raw() + 1).to_be_bytes();
    let kvs: Vec<(Slice, Slice)> = tx
        .range_scan(lower.as_bytes(), &upper)
        .collect::<Result<Vec<_>>>()?;
    assert!(!kvs.is_empty(), "poison seat requires at least one index row");
    for (k, _) in &kvs {
        let mut garbage = vec![0u8; 8];
        garbage.push(0xc1);
        tx.put(k, &garbage)?;
    }
    Ok(())
}

pub(crate) fn assert_msgpack_manifest_wire_pinned<M>(m: &M, pinned_hex: &str) -> Result<()>
where
    M: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let mut bytes = vec![];
    m.serialize(&mut rmp_serde::Serializer::new(&mut bytes).with_struct_map())
        .map_err(|e| miette!("{e}"))?;
    let decoded: M = rmp_serde::from_slice(&bytes).map_err(|e| miette!("{e}"))?;
    assert_eq!(&decoded, m, "wire round trip");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, pinned_hex, "the manifest wire format changed; this is an on-disk format migration, not a refactor");
    assert!(rmp_serde::from_slice::<M>(&bytes[..bytes.len() / 2]).is_err(), "truncated wire must refuse");
    Ok(())
}
