/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Seat 8 forge wall (#268 T5 / #347): Store cannot mint KyzoRecord.
//!
//! Seat 8 is proven by (1) Rust module/field privacy — `pub(crate) mod session`
//! → `pub(crate) mod admit`, private `KyzoRecord.core`, sibling `store` cannot
//! construct (enforced by `cargo check -p kyzo`); and (2) the grep-proof
//! harness below (`forge_wall_grep_*`). External trybuild cannot test that
//! `pub(crate)` internal wall without exposing `session`/`admit` at the crate
//! door, which would weaken seat 8 — so no external compile-fail suite here.
//! Grep proves no put path embeds a forged record and no blob-form type sits
//! on the store admission surface.
//!
//! [`read_surface`] walks `crates/kyzo-core/src/store/` and
//! `crates/kyzo-core/src/session/` on disk at test-run-time, rather than
//! naming files by hand: a hand-picked `include_str!` list silently stops
//! covering a file the moment a new one lands next to it (this harness
//! covered 10 of the surface's real files before this walk replaced the
//! list) — a directory walk covers every file that exists today AND every
//! file added tomorrow, with no second maintenance step.

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `.rs` file under `root`, recursively.
fn collect_rs_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// `(label, source stripped of #[cfg(test)] scope)` for every real `.rs`
/// file under `crates/kyzo-core/src/<rel_dir>/`, walked from disk (not a
/// hand-maintained list) and sorted for a deterministic scan order.
fn read_surface(rel_dir: &str) -> Vec<(String, String)> {
    let base = manifest_dir().join("../kyzo-core/src").join(rel_dir);
    let mut paths = Vec::new();
    collect_rs_files(&base, &mut paths);
    assert!(
        !paths.is_empty(),
        "no .rs files found under {} — the walk target moved or the surface is empty",
        base.display()
    );
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let label = p.to_string_lossy().replace('\\', "/");
            let text =
                std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("reading {label}: {e}"));
            (label, strip_tests(&text))
        })
        .collect()
}

#[test]
fn forge_wall_grep_no_put_embeds_forged_record() {
    // Production store + session surfaces, walked whole (test scope already
    // stripped per-file by `read_surface`) — the forbidden-needle table
    // cannot match itself since it lives in kyzo-trials, outside both walks.
    let store = read_surface("store");
    let session = read_surface("session");
    println!(
        "forge_wall: scanned {} store file(s), {} session file(s) ({} total)",
        store.len(),
        session.len(),
        store.len() + session.len()
    );

    let admit = session
        .iter()
        .find(|(label, _)| label.ends_with("session/admit.rs"))
        .unwrap_or_else(|| panic!("session/admit.rs must be present in the walked surface"));
    let admit_text = admit.1.as_str();

    // Split so this test body never contains a contiguous forbidden ident.
    let forged_construct: String = ["KyzoRecord ", "{"].concat();
    let forged_new: String = ["KyzoRecord::", "new"].concat();
    let forged_from_bytes: String = ["KyzoRecord::", "from_bytes"].concat();
    let forged_from_sst: String = ["KyzoRecord::", "from_sst"].concat();
    let forged_from_wal: String = ["KyzoRecord::", "from_wal"].concat();
    let put_forged: String = ["put_forged", "_record"].concat();
    let embed_forged: String = ["embed_forged", "_kyzo_record"].concat();

    let forbidden = [
        forged_construct.as_str(),
        forged_new.as_str(),
        forged_from_bytes.as_str(),
        forged_from_sst.as_str(),
        forged_from_wal.as_str(),
        put_forged.as_str(),
        embed_forged.as_str(),
    ];

    // Store paths must never construct or name a forge mint, nor reach into
    // admission internals directly — store routes through the one admission
    // door, never around it.
    for (label, src) in &store {
        for needle in &forbidden {
            assert!(
                !src.contains(needle),
                "forbidden forge mint `{needle}` must not appear on store surface {label}"
            );
        }
        assert!(
            !src.contains("admit_record("),
            "store must not call admit_record — admission owns the mint ({label})"
        );
    }

    // Every OTHER session file must also never construct or name a forge
    // mint — admit.rs is the sole legitimate mint site anywhere on this
    // surface, not merely relative to store. (Other session files legally
    // CALL admit_record — that is the one admission door working as
    // intended — so the admit_record-call ban stays store-only, above.)
    for (label, src) in &session {
        if label == &admit.0 {
            continue;
        }
        for needle in &forbidden {
            assert!(
                !src.contains(needle),
                "forbidden forge mint `{needle}` must not appear on session surface {label}"
            );
        }
    }

    // Sole legal construct site: admit_record body in session/admit.rs.
    let admit_construct_count = admit_text.matches("KyzoRecord { core:").count();
    assert_eq!(
        admit_construct_count, 1,
        "exactly one KyzoRecord {{ core: ... }} mint in admit.rs (admit_record)"
    );
    assert!(
        admit_text.contains("pub(crate) fn admit_record("),
        "admit_record must remain the sole crate-visible mint door"
    );

    // Put path: bytes only — no KyzoRecord-typed put on WriteTx.
    let tx = store
        .iter()
        .find(|(label, _)| label.ends_with("store/tx.rs"))
        .unwrap_or_else(|| panic!("store/tx.rs must be present in the walked surface"));
    assert!(
        tx.1.contains("fn put(&mut self, key: &[u8], val: &[u8])"),
        "WriteTx::put must stay byte currency"
    );
    for (label, src) in store.iter().chain(session.iter()) {
        assert!(
            !src.contains("fn put(&mut self, key: &[u8], val: KyzoRecord"),
            "{label}: put must not accept KyzoRecord"
        );
        assert!(
            !src.contains("fn put(&mut self, key: &[u8], val: &KyzoRecord"),
            "{label}: put must not accept &KyzoRecord"
        );
    }
}

#[test]
fn forge_wall_grep_no_blob_form_on_store_admission() {
    let store = read_surface("store");
    let session = read_surface("session");
    println!(
        "forge_wall (blob-form): scanned {} store file(s), {} session file(s) ({} total)",
        store.len(),
        session.len(),
        store.len() + session.len()
    );

    // Split needles so this file cannot match itself.
    let blob_form: String = ["struct Blob", "Form"].concat();
    let blob_record: String = ["struct Blob", "Record"].concat();
    let string_kind: String = ["struct String", "Kind"].concat();
    let raw_json: String = ["struct RawJson", "Payload"].concat();
    let kind_string_payload: String = ["kind: String,", " payload:"].concat();
    let kind_str_json: String = ["kind: String,", " json:"].concat();

    let forbidden = [
        blob_form.as_str(),
        blob_record.as_str(),
        string_kind.as_str(),
        raw_json.as_str(),
        kind_string_payload.as_str(),
        kind_str_json.as_str(),
    ];

    for (label, src) in store.iter().chain(session.iter()) {
        for needle in &forbidden {
            assert!(
                !src.contains(needle),
                "blob-form `{needle}` must be absent from store admission surface ({label})"
            );
        }
    }
}

fn strip_tests(src: &str) -> String {
    src.split("#[cfg(test)]")
        .next()
        .expect("production surface")
        .to_string()
}
