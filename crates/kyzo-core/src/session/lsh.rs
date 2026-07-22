/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Session door that builds and queries the LSH dedup projection.

#[cfg(test)]
mod tests {
    use miette::{Result, miette};
    use std::collections::BTreeMap;

    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::store::Storage;
    use crate::store::sim::SimStorage;
    use kyzo_model::value::DataValue;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn open_engine<S: Storage>(store: S) -> Result<Engine<S>> {
        Ok(Engine::compose(store, Catalog::new())?)
    }

    /// LSH end to end: near-duplicate candidates come back; `::index drop`
    /// removes the index and the search atom then refuses typed.
    #[test]
    fn lsh_create_search_drop_mem() -> Result<()>  {
        let db = open_engine(SimStorage::new(7));
        db.run_script(
            "?[id, body] <- [[1, 'a b c d e f g h i j'], [2, 'a b c d e f g h i z'], [3, 'q r s t u v w x y zz']] \
             :create doc {id => body: String}",
            no_params(),
        )
        .map_err(|e| miette!("create+insert: {e}"))?;
        db.run_script(
            "::lsh create doc:sim {extractor: body, tokenizer: Simple, n_gram: 3, \
              n_perm: 64, target_threshold: 0.5}",
            no_params(),
        )
        .map_err(|e| miette!("lsh create: {e}"))?;
        let out = db
            .run_script(
                "?[id] := ~doc:sim{id | query: 'a b c d e f g h i j', k: 5}, id != 1",
                no_params(),
            )
            .map_err(|e| miette!("lsh search: {e}"))?;
        let ids: Vec<i64> = out
            .rows()
            .iter()
            .map(|r| r[0].get_int().ok_or_else(|| miette!("id"))?)
            .collect();
        assert!(
            ids.contains(&2),
            "near-duplicate must be a candidate: {ids:?}"
        );
        assert!(!ids.contains(&3), "far row must not band-collide: {ids:?}");

        db.run_script("::index drop doc:sim", no_params())
            .map_err(|e| miette!("index drop: {e}"))?;
        let err = db
            .run_script(
                "?[id] := ~doc:sim{id | query: 'a b c d e f g h i j', k: 5}",
                no_params(),
            )
            .expect_err("search on a dropped index must refuse");
        assert!(
            err.to_string().contains("no index named"),
            "typed refusal, got: {err}"
        );
        Ok(())
    }
}
