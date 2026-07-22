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
use kyzo_model::program::InputRelationHandle;
use kyzo_model::value::DataValue;
use kyzo_core::data::relation::{ColType, StoredRelationMetadata};
use kyzo_core::project::gazetteer::{GazetteerConfig, compile_dictionary, gazetteer_dict_metadata};
use kyzo_core::session::catalog::{KeyspaceKind, create_relation};
use kyzo_core::store::fjall::new_fjall_storage;
use kyzo_core::store::{Storage, WriteTx};
use smartstring::{LazyCompact, SmartString};


/// Fail the trial loudly — `assert!` is always live (not `debug_assert`).
fn must_ok<T, E: std::fmt::Display>(r: Result<T, E>, ctx: &str) -> T {
    match r {
        Ok(v) => v,
        Err(e) => loop {
            assert!(false, "{ctx}: {e}");
        },
    }
}

fn must_some<T>(o: Option<T>, ctx: &str) -> T {
    match o {
        Some(v) => v,
        None => loop {
            assert!(false, "{ctx}");
        },
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
        let dir = must_ok(tempfile::tempdir(), "tempdir");
        let db = must_ok(new_fjall_storage(dir.path()), "fjall storage");
        let meta = gazetteer_dict_metadata(ColType::Int);
        let mut tx = must_ok(db.write_tx(), "write_tx");
        let dict =
            must_ok(create_relation(&mut tx, InputRelationHandle::from_metadata("dict", meta), KeyspaceKind::Facts), "create_relation");
        let bad = vec![
            DataValue::from(1i64),
            DataValue::List(vec![DataValue::List(vec![DataValue::from("x")])]),
        ];
        let key = must_ok(dict.encode_key_for_store(&bad, SourceSpan(0, 0)), "encode_key");
        let val = must_ok(dict
            .encode_val_only_for_store(&bad, SourceSpan(0, 0)), "encode_val");
        must_ok(tx.put(&key, &val), "put");
        must_ok(tx.commit(), "commit");
        let rtx = must_ok(db.read_tx(), "read_tx");
        let r = catch_unwind(AssertUnwindSafe(|| {
            compile_dictionary(&rtx, &dict, GazetteerConfig::default())
        }));
        assert!(r.is_ok(), "nested-list surface panicked");
        assert!(
            must_ok(r, "trial").is_err(),
            "nested-list surface should be a typed error"
        );
    }

    // (b) enormous surface (~2 MiB of 'a') — must build or error, never panic.
    {
        let dir = must_ok(tempfile::tempdir(), "tempdir");
        let db = must_ok(new_fjall_storage(dir.path()), "fjall storage");
        let big = "a".repeat(2 * 1024 * 1024);
        let rows: &[(i64, &[&str])] = &[(1, &[big.as_str()])];
        let r = catch_unwind(AssertUnwindSafe(|| {
            let meta = gazetteer_dict_metadata(ColType::Int);
            let mut tx = must_ok(db.write_tx(), "write_tx");
            let dict =
                must_ok(create_relation(&mut tx, InputRelationHandle::from_metadata("dict", meta), KeyspaceKind::Facts), "create_relation");
            for (entity, surfaces) in rows {
                let sl = DataValue::List(surfaces.iter().map(|s| DataValue::from(*s)).collect());
                let row = vec![DataValue::from(*entity), sl];
                must_ok(dict.put_fact(
                    &mut tx,
                    &row,
                    kyzo_model::value::ValidityTs::of_micros(0),
                    SourceSpan(0, 0),
                ), "put_fact");
            }
            must_ok(tx.commit(), "commit");
            let rtx = must_ok(db.read_tx(), "read_tx");
            let g = must_ok(compile_dictionary(&rtx, &dict, GazetteerConfig::default()), "compile_dictionary");
            let doc: SmartString<LazyCompact> = SmartString::from(big.as_str());
            g.tag(&doc).len()
        }));
        assert!(r.is_ok(), "enormous surface panicked");
        assert_eq!(must_ok(r, "trial"), 1, "enormous surface tags exactly once");
    }
}
