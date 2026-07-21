/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Fuzz seeds for the one-law encoding battery.
//!
//! The corpus in `kyzo-model/src/format/tests.rs` doubles as fuzz-seed
//! material for this lane — one corpus, no fork (03-storage-store.json ban).
//!
//! Gazetteer corrupt-dictionary sweep re-laned here from condemned
//! `engines/gazetteer_hostile` (story #350 T5 / 04-engines-project.json).

#![cfg(test)]

use kyzo_model::SourceSpan;
use kyzo_model::value::DataValue;
use kyzo_core::data::program::InputRelationHandle;
use kyzo_core::data::relation::{ColType, StoredRelationMetadata};
use kyzo_core::project::gazetteer::{GazetteerConfig, compile_dictionary, gazetteer_dict_metadata};
use kyzo_core::session::catalog::{KeyspaceKind, create_relation};
use kyzo_core::store::fjall::new_fjall_storage;
use kyzo_core::store::{Storage, WriteTx};
use smartstring::{LazyCompact, SmartString};

fn input_handle(name: &str, metadata: StoredRelationMetadata) -> InputRelationHandle {
    use kyzo_model::program::symbol::Symbol;
    let key_bindings = metadata
        .keys
        .iter()
        .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
        .collect();
    let dep_bindings = metadata
        .non_keys
        .iter()
        .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
        .collect();
    InputRelationHandle {
        name: Symbol::new(name, SourceSpan(0, 0)),
        metadata,
        key_bindings,
        dep_bindings,
        span: SourceSpan(0, 0),
    }
}

/// Law 5 sweep: corrupt dictionaries — nested-list surface, enormous (~2 MiB)
/// surface — none may panic; each is a typed error or a clean build.
/// Re-laned from an #[ignore]d unit test into the fuzz/trials lane.
#[test]
pub fn law5_corrupt_dictionary_sweep_never_panics() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    // (a) surfaces list containing a nested List (not a string).
    {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let meta = gazetteer_dict_metadata(ColType::Int);
        let mut tx = db.write_tx().unwrap();
        let dict =
            create_relation(&mut tx, input_handle("dict", meta), KeyspaceKind::Facts).unwrap();
        let bad = vec![
            DataValue::from(1i64),
            DataValue::List(vec![DataValue::List(vec![DataValue::from("x")])]),
        ];
        let key = dict.encode_key_for_store(&bad, SourceSpan(0, 0)).unwrap();
        let val = dict
            .encode_val_only_for_store(&bad, SourceSpan(0, 0))
            .unwrap();
        tx.put(&key, &val).unwrap();
        tx.commit().unwrap();
        let rtx = db.read_tx().unwrap();
        let r = catch_unwind(AssertUnwindSafe(|| {
            compile_dictionary(&rtx, &dict, GazetteerConfig::default())
        }));
        assert!(r.is_ok(), "nested-list surface panicked");
        assert!(
            r.unwrap().is_err(),
            "nested-list surface should be a typed error"
        );
    }

    // (b) enormous surface (~2 MiB of 'a') — must build or error, never panic.
    {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let big = "a".repeat(2 * 1024 * 1024);
        let rows: &[(i64, &[&str])] = &[(1, &[big.as_str()])];
        let r = catch_unwind(AssertUnwindSafe(|| {
            let meta = gazetteer_dict_metadata(ColType::Int);
            let mut tx = db.write_tx().unwrap();
            let dict =
                create_relation(&mut tx, input_handle("dict", meta), KeyspaceKind::Facts).unwrap();
            for (entity, surfaces) in rows {
                let sl = DataValue::List(surfaces.iter().map(|s| DataValue::from(*s)).collect());
                let row = vec![DataValue::from(*entity), sl];
                dict.put_fact(
                    &mut tx,
                    &row,
                    kyzo_model::value::ValidityTs::from_raw(0),
                    SourceSpan(0, 0),
                )
                .unwrap();
            }
            tx.commit().unwrap();
            let rtx = db.read_tx().unwrap();
            let g = compile_dictionary(&rtx, &dict, GazetteerConfig::default()).unwrap();
            let doc: SmartString<LazyCompact> = SmartString::from(big.as_str());
            g.tag(&doc).len()
        }));
        assert!(r.is_ok(), "enormous surface panicked");
        assert_eq!(r.unwrap(), 1, "enormous surface tags exactly once");
    }
}
