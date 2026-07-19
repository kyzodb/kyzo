//! Session door that builds and queries the HNSW vector ANN projection.

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::store::fjall::new_fjall_storage;
    use crate::store::sim::SimStorage;
    use crate::store::Storage;
    use kyzo_model::value::DataValue;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn open_engine<S: Storage>(store: S) -> Engine<S> {
        Engine::compose(store, Catalog::new()).expect("compose engine")
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
        .expect("create+insert");
        db.run_script(
            "::hnsw create doc:emb {fields: [v], dim: 4, m: 16, ef_construction: 32, \
              distance: L2}",
            no_params(),
        )
        .expect("hnsw create");
        // Inserted AFTER the index exists: the write-path hook must index it.
        db.run_script(
            "?[id, v] <- [[3, vec([0.9, 0.1, 0.0, 0.0])]] :put doc {id => v}",
            no_params(),
        )
        .expect("post-create insert");

        let out = db
            .run_script(
                "?[dist, id] := ~doc:emb{id | query: vec([1.0, 0.0, 0.0, 0.0]), k: 3, \
                  bind_distance: dist} :sort dist",
                no_params(),
            )
            .expect("hnsw search");
        // A Datalog answer is a set; :sort puts it in distance order.
        // Nearest first by squared L2: id 1 at 0, id 3 at 0.02, id 2 at 2.
        let ids: Vec<i64> = out
            .rows()
            .iter()
            .map(|r| r[1].get_int().expect("id"))
            .collect();
        assert_eq!(ids, vec![1, 3, 2], "nearest-first order");
        let d0 = out.rows()[0][0].get_float().expect("dist");
        let d1 = out.rows()[1][0].get_float().expect("dist");
        assert!(d0.abs() < 1e-6, "exact match at distance 0, got {d0}");
        assert!((d1 - 0.02).abs() < 1e-6, "squared L2, got {d1}");
    }

    #[test]
    fn hnsw_create_insert_search_mem() {
        hnsw_create_insert_search(open_engine(SimStorage::new(7)));
    }

    #[test]
    fn hnsw_create_insert_search_fjall() {
        let dir = tempfile::tempdir().expect("tempdir");
        hnsw_create_insert_search(open_engine(new_fjall_storage(dir.path()).unwrap()));
    }
}
