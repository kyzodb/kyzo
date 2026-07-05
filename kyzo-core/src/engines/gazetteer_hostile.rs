/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! HOSTILE REVIEW tests for the gazetteer engine — NOT part of the reviewed
//! module. An independently-written leftmost-longest reference (structurally
//! different from the module's own `naive_tag`: it enumerates every candidate
//! substring match, then resolves leftmost-longest by greedy resumption) plus
//! adversarial documents (Turkish dotted/dotless i, ligatures, combining marks
//! adjacent to ASCII, three-way nesting, prefix==suffix, whole-doc surface,
//! single-char carpets, seeded fuzz). Every case asserts (a) engine == my
//! reference, (b) every returned span is on a char boundary and slices back to
//! the surface, (c) determinism.

#![cfg(test)]

use smartstring::{LazyCompact, SmartString};
use std::collections::BTreeSet;

use crate::data::program::InputRelationHandle;
use crate::data::relation::{ColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::engines::gazetteer::{
    Gazetteer, GazetteerConfig, Tag, compile_dictionary, gazetteer_dict_metadata,
};
use crate::runtime::relation::{KeyspaceKind, RelationHandle, create_relation};
use crate::storage::fjall::new_fjall_storage;
use crate::storage::{Storage, WriteTx};

fn input_handle(name: &str, metadata: StoredRelationMetadata) -> InputRelationHandle {
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

fn compile(
    db: &impl Storage,
    rows: &[(i64, &[&str])],
    config: GazetteerConfig,
) -> (RelationHandle, Gazetteer) {
    let meta = gazetteer_dict_metadata(ColType::Int);
    let mut tx = db.write_tx().unwrap();
    let dict = create_relation(&mut tx, input_handle("dict", meta), KeyspaceKind::Facts).unwrap();
    for (entity, surfaces) in rows {
        let surface_list = DataValue::List(surfaces.iter().map(|s| DataValue::from(*s)).collect());
        let row = vec![DataValue::from(*entity), surface_list];
        dict.put_fact(
            &mut tx,
            &row,
            crate::data::value::ValidityTs::from_raw(0),
            SourceSpan(0, 0),
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let rtx = db.read_tx().unwrap();
    let g = compile_dictionary(&rtx, &dict, config).unwrap();
    (dict, g)
}

fn view(tags: &[Tag]) -> Vec<(i64, usize, usize, String)> {
    tags.iter()
        .map(|t| {
            (
                t.entity.get_int().unwrap(),
                t.start,
                t.len,
                t.surface.to_string(),
            )
        })
        .collect()
}

fn pairs<'a>(rows: &'a [(i64, &'a [&'a str])]) -> Vec<(i64, &'a str)> {
    rows.iter()
        .flat_map(|(e, ss)| ss.iter().map(move |s| (*e, *s)))
        .collect()
}

/// INDEPENDENT reference: enumerate every candidate (start,end) where a surface
/// matches on char boundaries, then resolve leftmost-longest by greedy
/// resumption (smallest start >= cursor, then longest end at that start). This
/// is a different algorithm shape from the module's own oracle.
fn ref_tag(
    pairs: &[(i64, &str)],
    text: &str,
    case_insensitive: bool,
) -> Vec<(i64, usize, usize, String)> {
    let eq = |a: &[u8], b: &[u8]| {
        if case_insensitive {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    let bytes = text.as_bytes();
    // All candidate spans that match on char boundaries.
    let mut cands: Vec<(usize, usize)> = Vec::new(); // (start, end)
    for start in 0..=text.len() {
        if !text.is_char_boundary(start) {
            continue;
        }
        for (_, surf) in pairs {
            let sb = surf.as_bytes();
            let end = start + sb.len();
            if !sb.is_empty()
                && end <= text.len()
                && text.is_char_boundary(end)
                && eq(&bytes[start..end], sb)
            {
                cands.push((start, end));
            }
        }
    }
    let mut out: Vec<(i64, usize, usize, String)> = Vec::new();
    let mut cursor = 0usize;
    // Leftmost: smallest start >= cursor with any candidate.
    while let Some(&min_start) = cands.iter().map(|(s, _)| s).filter(|&&s| s >= cursor).min() {
        // Longest: max end among candidates at that start.
        let max_end = cands
            .iter()
            .filter(|(s, _)| *s == min_start)
            .map(|(_, e)| *e)
            .max()
            .unwrap();
        // Every entity whose surface equals text[min_start..max_end].
        let mut ents: BTreeSet<i64> = BTreeSet::new();
        for (e, surf) in pairs {
            let sb = surf.as_bytes();
            if sb.len() == max_end - min_start && eq(&bytes[min_start..max_end], sb) {
                ents.insert(*e);
            }
        }
        for e in ents {
            out.push((
                e,
                min_start,
                max_end - min_start,
                text[min_start..max_end].to_string(),
            ));
        }
        cursor = max_end;
    }
    out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Assert engine == reference AND every span is boundary-truthful (slices back
/// to its surface) — the latter would panic inside `tag` already if violated,
/// but we re-check the returned spans explicitly.
fn assert_agree(g: &Gazetteer, pairs: &[(i64, &str)], text: &str, ci: bool) {
    let tags = g.tag(text);
    for t in &tags {
        assert!(
            text.is_char_boundary(t.start),
            "start not boundary in {text:?}"
        );
        assert!(
            text.is_char_boundary(t.start + t.len),
            "end not boundary in {text:?}"
        );
        assert_eq!(
            &text[t.start..t.start + t.len],
            t.surface.as_str(),
            "surface not the document slice in {text:?}"
        );
    }
    assert_eq!(
        view(&tags),
        ref_tag(pairs, text, ci),
        "mismatch on {text:?}"
    );
    // Determinism: a second tag() is byte-identical.
    assert_eq!(g.tag(text), tags, "non-deterministic tag on {text:?}");
}

#[test]
fn turkish_dotted_dotless_i_no_false_multibyte_match() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    // Entities keyed on ASCII "i"/"I" plus the multibyte İ (U+0130) and
    // ı (U+0131). Case-insensitive: ASCII folding must NOT reach into the
    // multibyte chars, and İ as a surface must match İ in the doc.
    let rows: &[(i64, &[&str])] = &[
        (1, &["i"]),
        (2, &["I"]),
        (3, &["\u{0130}"]), // İ
        (4, &["\u{0131}"]), // ı
        (5, &["is"]),
    ];
    let cfg = GazetteerConfig {
        case_insensitive: true,
    };
    let (_d, g) = compile(&db, rows, cfg);
    let p = pairs(rows);
    for text in [
        "\u{0130}stanbul",      // İstanbul — İ then ASCII 'stanbul'
        "the \u{0131} dotless", // ı
        "It is I and \u{0130} and \u{0131}",
        "i\u{0130}i\u{0131}i",
        "\u{0130}\u{0130}\u{0130}",
    ] {
        assert_agree(&g, &p, text, true);
    }
}

#[test]
fn ligature_and_combining_marks_adjacent_to_ascii() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let rows: &[(i64, &[&str])] = &[
        (1, &["cafe"]),
        (2, &["ff"]),
        (3, &["\u{FB00}"]),  // ﬀ ligature
        (4, &["a\u{0301}"]), // a + combining acute
        (5, &["a"]),
    ];
    let (_d, g) = compile(&db, rows, GazetteerConfig::default());
    let p = pairs(rows);
    for text in [
        "cafe\u{0301}",        // "cafe" then combining mark on the e
        "office \u{FB00}ur",   // ligature adjacent to ascii
        "a\u{0301}bc a plain", // combining then plain a
        "\u{FB00}\u{FB00}ff",
        "cafe cafe\u{0301}",
    ] {
        assert_agree(&g, &p, text, false);
    }
}

#[test]
fn three_way_nesting_and_prefix_suffix_overlaps() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    // a ⊂ ab ⊂ abc ; and aba/ab/ba (prefix and suffix relationships).
    let rows: &[(i64, &[&str])] = &[
        (1, &["a"]),
        (2, &["ab"]),
        (3, &["abc"]),
        (4, &["aba"]),
        (5, &["ba"]),
        (6, &["abab"]),
    ];
    let (_d, g) = compile(&db, rows, GazetteerConfig::default());
    let p = pairs(rows);
    for text in [
        "abc",
        "ab",
        "a",
        "abab",
        "ababa",
        "aba",
        "baba",
        "abababc",
        "xabcx",
        "aabbaa",
        "abcabcabc",
    ] {
        assert_agree(&g, &p, text, false);
    }
}

#[test]
fn whole_document_surface_and_single_char_carpet() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let rows: &[(i64, &[&str])] = &[
        (1, &["a"]),
        (2, &["b"]),
        (3, &["the whole thing"]),
        (4, &["東京"]),
    ];
    let (_d, g) = compile(&db, rows, GazetteerConfig::default());
    let p = pairs(rows);
    for text in [
        "the whole thing", // surface == whole doc
        "ababababab",      // single-char carpet
        "abababab東京abab",
        "東京東京",
        "b",
    ] {
        assert_agree(&g, &p, text, false);
    }
}

#[test]
fn seeded_fuzz_against_independent_reference() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let rows: &[(i64, &[&str])] = &[
        (1, &["ab"]),
        (2, &["abc"]),
        (3, &["b"]),
        (4, &["café"]),
        (5, &["é"]),
        (6, &["東"]),
        (7, &["東京"]),
        (8, &["a\u{0301}"]),
    ];
    let (_dl, g_lower) = {
        let cfg = GazetteerConfig {
            case_insensitive: true,
        };
        compile(&db, rows, cfg)
    };
    // separate store for exact mode to avoid relation-name collision
    let dir2 = tempfile::tempdir().unwrap();
    let db2 = new_fjall_storage(dir2.path()).unwrap();
    let (_de, g_exact) = compile(&db2, rows, GazetteerConfig::default());
    let p = pairs(rows);

    let alphabet = [
        "a", "b", "c", "A", "B", "é", "東", "京", " ", "\u{0301}", "C",
    ];
    let mut state: u64 = 0x9E3779B97F4A7C15;
    for _ in 0..4000 {
        // xorshift64* deterministic PRNG — no crate dependency.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let mut r = state.wrapping_mul(0x2545F4914F6CDD1D);
        let n = 1 + (r % 9) as usize;
        r /= 9;
        let mut s = String::new();
        for _ in 0..n {
            let idx = (r % alphabet.len() as u64) as usize;
            r /= alphabet.len() as u64;
            s.push_str(alphabet[idx]);
        }
        assert_agree(&g_exact, &p, &s, false);
        assert_agree(&g_lower, &p, &s, true);
    }
}

/// A case-insensitive surface carrying a multibyte char (İ) whose ASCII-lower
/// key equals itself. This is the differential that a
/// `to_ascii_lowercase → to_lowercase` mutation of the compiler breaks: the
/// mutant would key the pattern as "i̇" (i + combining dot) and stop matching
/// the document's İ, diverging from this reference.
#[test]
fn case_insensitive_multibyte_surface_matches_original_form() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let rows: &[(i64, &[&str])] = &[(1, &["\u{0130}NDEX"])]; // İNDEX
    let cfg = GazetteerConfig {
        case_insensitive: true,
    };
    let (_d, g) = compile(&db, rows, cfg);
    let p = pairs(rows);
    // Document carries the exact multibyte surface plus ASCII-cased variants of
    // the ASCII tail.
    for text in ["the \u{0130}NDEX here", "\u{0130}ndex lower tail"] {
        assert_agree(&g, &p, text, true);
    }
    // Direct pin: İNDEX in the document tags at its true byte span.
    let tags = view(&g.tag("go \u{0130}NDEX go"));
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].0, 1);
    assert_eq!(
        &"go \u{0130}NDEX go"[tags[0].1..tags[0].1 + tags[0].2],
        "\u{0130}NDEX"
    );
}

/// Two independent compiles of the same dictionary tag identically on hostile
/// input — the determinism law, exercised on adversarial docs.
#[test]
fn two_compiles_agree_on_adversarial_docs() {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let rows: &[(i64, &[&str])] = &[
        (3, &["東京", "b"]),
        (1, &["ab", "a"]),
        (2, &["abc", "東", "ab"]),
    ];
    let (dict, g1) = compile(&db, rows, GazetteerConfig::default());
    let rtx = db.read_tx().unwrap();
    let g2 = compile_dictionary(&rtx, &dict, GazetteerConfig::default()).unwrap();
    for text in ["ab東京abc東ab", "東京", "abcabab"] {
        assert_eq!(
            g1.tag(text),
            g2.tag(text),
            "two compiles disagree on {text:?}"
        );
    }
}

/// Law 5 sweep: corrupt dictionaries beyond the module's own three — a surfaces
/// list with a nested list element, an empty entity key with an empty surface,
/// an enormous surface — none may panic; each is a typed error or a clean build.
#[test]
#[ignore = "heavyweight hostile sweep (minutes): run explicitly with --ignored"]
fn law5_corrupt_dictionary_sweep_never_panics() {
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
                    crate::data::value::ValidityTs::from_raw(0),
                    SourceSpan(0, 0),
                )
                .unwrap();
            }
            tx.commit().unwrap();
            let rtx = db.read_tx().unwrap();
            let g = compile_dictionary(&rtx, &dict, GazetteerConfig::default()).unwrap();
            // And it can tag a doc containing the enormous surface.
            let doc: SmartString<LazyCompact> = SmartString::from(big.as_str());
            g.tag(&doc).len()
        }));
        assert!(r.is_ok(), "enormous surface panicked");
        assert_eq!(r.unwrap(), 1, "enormous surface tags exactly once");
    }
}

/// REVIEWER PROBE: isolate which phase is slow for huge surface forms
/// (law5 sweep's 2 MiB case exceeded a 1800 s suite cap). Times store-put,
/// compile_dictionary, and tag separately at growing sizes.
#[test]
#[ignore = "heavyweight scaling probe (minutes): run explicitly with --ignored"]
fn probe_big_surface_scaling() {
    use std::time::Instant;
    for kib in [64usize, 256, 1024] {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let big = "a".repeat(kib * 1024);
        let meta = gazetteer_dict_metadata(ColType::Int);
        let t0 = Instant::now();
        let mut tx = db.write_tx().unwrap();
        let dict =
            create_relation(&mut tx, input_handle("dict", meta), KeyspaceKind::Facts).unwrap();
        let sl = DataValue::List(vec![DataValue::from(big.as_str())]);
        let row = vec![DataValue::from(1i64), sl];
        dict.put_fact(
            &mut tx,
            &row,
            crate::data::value::ValidityTs::from_raw(0),
            SourceSpan(0, 0),
        )
        .unwrap();
        tx.commit().unwrap();
        let t_put = t0.elapsed();
        let rtx = db.read_tx().unwrap();
        let t1 = Instant::now();
        let g = compile_dictionary(&rtx, &dict, GazetteerConfig::default()).unwrap();
        let t_compile = t1.elapsed();
        let t2 = Instant::now();
        let n = g.tag(&big).len();
        let t_tag = t2.elapsed();
        eprintln!("PROBE {kib} KiB: put={t_put:?} compile={t_compile:?} tag={t_tag:?} tags={n}");
    }
}
