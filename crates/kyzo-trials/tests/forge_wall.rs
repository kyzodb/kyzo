/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Seat 8 forge wall (#268 T5 / #347): Store cannot mint KyzoRecord.
//!
//! Trybuild compile-fail proves store/encode/WAL/SST-shaped paths cannot
//! construct a KyzoRecord (private constructors at admission only). Grep
//! proves no put path embeds a forged record and no blob-form type sits on
//! the store admission surface.

#[test]
fn forge_wall_store_cannot_construct_kyzo_record() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/forge_wall_store_mint_kyzo_record.rs");
}

#[test]
fn forge_wall_wal_sst_cannot_mint_kyzo_record() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/forge_wall_wal_sst_mint_kyzo_record.rs");
}

#[test]
fn forge_wall_encode_cannot_mint_kyzo_record() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/forge_wall_encode_mint_kyzo_record.rs");
}

#[test]
fn forge_wall_grep_no_put_embeds_forged_record() {
    // Production store + admission surfaces only — strip cfg(test) so the
    // forbidden-needle table cannot match itself.
    let store_mod = strip_tests(include_str!("../../kyzo-core/src/store/mod.rs"));
    let store_tx = strip_tests(include_str!("../../kyzo-core/src/store/tx.rs"));
    let store_wal = strip_tests(include_str!("../../kyzo-core/src/store/wal.rs"));
    let store_fjall = strip_tests(include_str!("../../kyzo-core/src/store/fjall.rs"));
    let store_time = strip_tests(include_str!("../../kyzo-core/src/store/time.rs"));
    let store_backup = strip_tests(include_str!("../../kyzo-core/src/store/backup.rs"));
    let store_forge = strip_tests(include_str!("../../kyzo-core/src/store/forge_wall.rs"));
    let admit = strip_tests(include_str!("../../kyzo-core/src/session/admit.rs"));

    let sources = [
        store_mod.as_str(),
        store_tx.as_str(),
        store_wal.as_str(),
        store_fjall.as_str(),
        store_time.as_str(),
        store_backup.as_str(),
        store_forge.as_str(),
        admit.as_str(),
    ];

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

    // Store paths must never construct or name a forge mint.
    for src in [
        store_mod.as_str(),
        store_tx.as_str(),
        store_wal.as_str(),
        store_fjall.as_str(),
        store_time.as_str(),
        store_backup.as_str(),
        store_forge.as_str(),
    ] {
        for needle in &forbidden {
            assert!(
                !src.contains(needle),
                "forbidden forge mint `{needle}` must not appear on store surfaces"
            );
        }
        assert!(
            !src.contains("admit_record("),
            "store must not call admit_record — admission owns the mint"
        );
    }

    // Sole legal construct site: admit_record body in session/admit.rs.
    let admit_construct_count = admit.matches("KyzoRecord { core:").count();
    assert_eq!(
        admit_construct_count, 1,
        "exactly one KyzoRecord {{ core: ... }} mint in admit.rs (admit_record)"
    );
    assert!(
        admit.contains("pub(crate) fn admit_record("),
        "admit_record must remain the sole crate-visible mint door"
    );

    // Put path: bytes only — no KyzoRecord-typed put on WriteTx.
    assert!(
        store_tx.contains("fn put(&mut self, key: &[u8], val: &[u8])"),
        "WriteTx::put must stay byte currency"
    );
    for src in sources {
        assert!(
            !src.contains("fn put(&mut self, key: &[u8], val: KyzoRecord"),
            "put must not accept KyzoRecord"
        );
        assert!(
            !src.contains("fn put(&mut self, key: &[u8], val: &KyzoRecord"),
            "put must not accept &KyzoRecord"
        );
    }
}

#[test]
fn forge_wall_grep_no_blob_form_on_store_admission() {
    let surfaces = [
        strip_tests(include_str!("../../kyzo-core/src/store/mod.rs")),
        strip_tests(include_str!("../../kyzo-core/src/store/open.rs")),
        strip_tests(include_str!("../../kyzo-core/src/store/tx.rs")),
        strip_tests(include_str!("../../kyzo-core/src/store/wal.rs")),
        strip_tests(include_str!("../../kyzo-core/src/store/forge_wall.rs")),
        strip_tests(include_str!("../../kyzo-core/src/store/contract.rs")),
        strip_tests(include_str!("../../kyzo-core/src/session/admit.rs")),
    ];

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

    for src in &surfaces {
        for needle in &forbidden {
            assert!(
                !src.contains(needle),
                "blob-form `{needle}` must be absent from store admission surface"
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
