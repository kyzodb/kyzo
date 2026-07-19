//! Session door that builds and queries the FTS projection.

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::store::Storage;
    use crate::store::sim::SimStorage;
    use kyzo_model::value::DataValue;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn open_engine<S: Storage>(store: S) -> Engine<S> {
        Engine::compose(store, Catalog::new()).expect("compose engine")
    }

    /// FTS end to end: `::fts create` + a search atom with a bound score.
    #[test]
    fn fts_create_search_mem() {
        let db = open_engine(SimStorage::new(7));
        db.run_script(
            "?[id, body] <- [[1, 'the quick brown fox'], [2, 'lazy dogs sleep']] \
             :create doc {id => body: String}",
            no_params(),
        )
        .expect("create+insert");
        db.run_script(
            "::fts create doc:txt {extractor: body, tokenizer: Simple}",
            no_params(),
        )
        .expect("fts create");
        let out = db
            .run_script(
                "?[id, s] := ~doc:txt{id | query: 'fox', k: 5, bind_score: s}",
                no_params(),
            )
            .expect("fts search");
        assert_eq!(out.rows().len(), 1);
        assert_eq!(out.rows()[0][0].get_int(), Some(1));
        assert!(out.rows()[0][1].get_float().expect("score") > 0.0);
        // The searching row must survive a doc deletion (hook coverage).
        db.run_script("?[id] <- [[1]] :rm doc {id}", no_params())
            .expect("delete");
        let out = db
            .run_script("?[id] := ~doc:txt{id | query: 'fox', k: 5}", no_params())
            .expect("fts search after delete");
        assert_eq!(out.rows().len(), 0, "deleted doc left the index");
    }

    /// A single search atom whose hit count exceeds one output batch
    /// (1,200 matching docs > BATCH_ROWS = 1,024): the search executor's
    /// cross-batch resumption must deliver every hit exactly once — the
    /// same suspension state machine the materialized join pins.
    #[test]
    fn search_hits_resume_across_output_batch_boundary() {
        let db = open_engine(SimStorage::new(7));
        let mut script = String::from("?[id, body] <- [");
        for i in 0..1200 {
            script.push_str(&format!("[{i}, 'common fox term {i}'],"));
        }
        script.push_str("] :create doc {id => body: String}");
        db.run_script(&script, no_params()).expect("seed");
        db.run_script(
            "::fts create doc:txt {extractor: body, tokenizer: Simple}",
            no_params(),
        )
        .expect("fts create");
        let out = db
            .run_script("?[id] := ~doc:txt{id | query: 'fox', k: 1500}", no_params())
            .expect("boundary search");
        assert_eq!(
            out.rows().len(),
            1200,
            "every hit exactly once across the boundary"
        );
    }
}
