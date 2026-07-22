/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Session door that builds and queries the HNSW vector ANN projection.

#[cfg(test)]
mod tests {
    use miette::{Result, miette};
    use std::collections::BTreeMap;

    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::store::Storage;
    use crate::store::fjall::new_fjall_storage;
    use crate::store::sim::SimStorage;
    use kyzo_model::value::DataValue;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn open_engine<S: Storage>(store: S) -> Result<Engine<S>> {
        Ok(Engine::compose(store, Catalog::new())?)
    }

    /// THE SEARCH PIPELINE END TO END: `::hnsw create` builds and backfills
    /// the index, the mutation hook indexes a later insert, and the
    /// `~doc:emb{…}` atom drives `hnsw_knn` through parse → resolve →
    /// compile → RA → eval, appending the distance column nearest-first.
    fn hnsw_create_insert_search<S: Storage>(db: Engine<S>) {
        db.run_script(
            "?[id, v] <- [[1, vec([1.0, 0.0, 0.0, 0.0])], [2, vec([0.0, 1.0, 0.0, 0.0])]] \
             :create doc {id => v: <F32; 4>}",
            no_params(),
        )
        .map_err(|e| miette!("create+insert: {e}")).expect("test helper");
        db.run_script(
            "::hnsw create doc:emb {fields: [v], dim: 4, m: 16, ef_construction: 32, \
              distance: L2}",
            no_params(),
        )
        .map_err(|e| miette!("hnsw create: {e}")).expect("test helper");
        // Inserted AFTER the index exists: the write-path hook must index it.
        db.run_script(
            "?[id, v] <- [[3, vec([0.9, 0.1, 0.0, 0.0])]] :put doc {id => v}",
            no_params(),
        )
        .map_err(|e| miette!("post-create insert: {e}")).expect("test helper");

        let out = db
            .run_script(
                "?[dist, id] := ~doc:emb{id | query: vec([1.0, 0.0, 0.0, 0.0]), k: 3, \
                  bind_distance: dist} :sort dist",
                no_params(),
            )
            .map_err(|e| miette!("hnsw search: {e}")).expect("test helper");
        // A Datalog answer is a set; :sort puts it in distance order.
        // Nearest first by squared L2: id 1 at 0, id 3 at 0.02, id 2 at 2.
        let ids: Vec<i64> = out
            .rows()
            .iter()
            .map(|r| r[1].get_int().ok_or_else(|| miette!("id")))
            .collect::<Result<_>>().expect("test helper");
        assert_eq!(ids, vec![1, 3, 2], "nearest-first order");
        let d0 = out.rows()[0][0]
            .get_float()
            .ok_or_else(|| miette!("dist")).expect("test helper");
        let d1 = out.rows()[1][0]
            .get_float()
            .ok_or_else(|| miette!("dist")).expect("test helper");
        assert!(d0.abs() < 1e-6, "exact match at distance 0, got {d0}");
        assert!((d1 - 0.02).abs() < 1e-6, "squared L2, got {d1}");
    }

    #[test]
    fn hnsw_create_insert_search_mem() -> Result<()> {
        hnsw_create_insert_search(open_engine(SimStorage::new(7))?);
        Ok(())
    }

    #[test]
    fn hnsw_create_insert_search_fjall() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
        hnsw_create_insert_search(open_engine(new_fjall_storage(dir.path())?)?);
        Ok(())
    }
}
