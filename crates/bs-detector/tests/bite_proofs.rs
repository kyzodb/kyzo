/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Bite proofs: one detonation per registered check. checks.toml binds each
//! check to a fn defined HERE, and the registry refuses to load if the fn
//! is missing — so deleting or renaming a proof un-registers its check and
//! the gate goes red. Helper fns return `Option` so the only panics in this
//! file live inside `#[test]` scope.

use bs_detector::boundary::{Boundary, SourceFile, UnparsedFile};
use bs_detector::engines::{Hit, graph, meta, shape};

fn parsed(rel: &str, src: &str) -> Option<SourceFile> {
    let ast = syn::parse_file(src).ok()?;
    Some(SourceFile {
        rel_path: rel.to_string(),
        text: src.to_string(),
        ast,
    })
}

fn shape_hits(name: &str, src: &str) -> Option<Vec<Hit>> {
    let m = shape::matcher_by_name(name)?;
    let f = parsed("crates/probe/src/probe.rs", src)?;
    Some(shape::run_matcher(m, &f))
}

/// `None` when the matcher is unregistered or the fixture does not parse —
/// the asserts compare against `Some(n)`, so a broken probe can never
/// masquerade as a hit count.
fn detonates(name: &str, src: &str) -> Option<usize> {
    Some(shape_hits(name, src)?.len())
}

// --- panics where typed refusal is owed -------------------------------------

#[test]
fn bite_unwrap() {
    assert_eq!(detonates("unwrap", "fn f(x: Option<u8>) -> u8 { x.unwrap() }"), Some(1));
    // The zone-law boundary: #[test] scaffolding has no callers, so the
    // loud detonator is exempt there — and ONLY there.
    assert_eq!(detonates("unwrap", "#[test]\nfn t() { Some(1u8).unwrap(); }"), Some(0));
}

#[test]
fn bite_expect() {
    assert_eq!(detonates("expect", "fn f(x: Option<u8>) -> u8 { x.expect(\"y\") }"), Some(1));
    assert_eq!(detonates("expect", "#[test]\nfn t() { Some(1u8).expect(\"y\"); }"), Some(0));
}

#[test]
fn bite_unwrap_or() {
    assert_eq!(detonates("unwrap_or", "fn f(x: Option<u8>) -> u8 { x.unwrap_or(0) }"), Some(1));
    // Swallowing is banned in tests too: a fabricated fallback hides a
    // failure instead of failing loudly.
    assert_eq!(detonates("unwrap_or", "#[test]\nfn t() { let _v = Some(1u8).unwrap_or(0); assert_eq!(_v, 1); }"), Some(1));
}

#[test]
fn bite_unwrap_or_else() {
    assert_eq!(detonates("unwrap_or_else", "fn f(x: Option<u8>) -> u8 { x.unwrap_or_else(|| 0) }"), Some(1));
}

#[test]
fn bite_unwrap_or_default() {
    assert_eq!(detonates("unwrap_or_default", "fn f(x: Option<u8>) -> u8 { x.unwrap_or_default() }"), Some(1));
}

#[test]
fn bite_unchecked_unwrap() {
    assert_eq!(detonates("unwrap_unchecked", "fn f(x: Option<u8>) -> u8 { unsafe { x.unwrap_unchecked() } }"), Some(1));
}

#[test]
fn bite_panic_bang() {
    assert_eq!(detonates("panic_bang", "fn f() { panic!(\"boom\"); }"), Some(1));
    assert_eq!(detonates("panic_bang", "#[test]\nfn t() { panic!(\"loud test failure\"); }"), Some(0));
}

#[test]
fn bite_unreachable_bang() {
    assert_eq!(detonates("unreachable_bang", "fn f(x: bool) -> u8 { if x { 1 } else { unreachable!() } }"), Some(1));
    assert_eq!(detonates("unreachable_bang", "#[test]\nfn t() { if false { unreachable!() } }"), Some(0));
}

#[test]
fn bite_todo_bang() {
    assert_eq!(detonates("todo_bang", "fn f() { todo!() }"), Some(1));
    assert_eq!(detonates("todo_bang", "fn f() { unimplemented!() }"), Some(1));
}

#[test]
fn bite_debug_assert() {
    assert_eq!(detonates("debug_assert", "fn f(x: u8) { debug_assert!(x > 0); }"), Some(1));
}

// --- swallowed errors and discarded values ----------------------------------

#[test]
fn bite_let_underscore() {
    assert_eq!(detonates("let_underscore", "fn g() -> u8 { 1 }\nfn f() { let _ = g(); }"), Some(1));
}

#[test]
fn bite_ok_drop() {
    assert_eq!(detonates("ok_drop", "fn f(x: Result<u8, u8>) { x.ok(); }"), Some(1));
}

#[test]
fn bite_err_costume() {
    let src = "fn f(r: Result<u8, u8>) -> u8 { match r { Ok(v) => v, Err(_) => 0 } }";
    let hits = match shape_hits("err_costume", src) {
        Some(h) => h,
        None => panic!("matcher registered and fixture parses"),
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].construct, "err_to_zero_costume");
}

#[test]
fn bite_poison_continue() {
    assert_eq!(
        detonates("poison_continue", "fn f(poisoned: std::sync::PoisonError<i32>) -> i32 { poisoned.into_inner() }"),
        Some(1)
    );
}

// --- numeric dishonesty -------------------------------------------------------

#[test]
fn bite_as_cast() {
    assert_eq!(detonates("as_cast", "fn f(x: u64) -> u8 { x as u8 }"), Some(1));
}

#[test]
fn bite_unchecked_arith() {
    assert_eq!(detonates("unchecked_arith", "fn f(a: u64, b: u64) -> u64 { a.wrapping_mul(b) }"), Some(1));
    assert_eq!(
        detonates(
            "unchecked_arith",
            "fn f(a: u64, b: u64) -> u64 {\n    // INVARIANT(SeedMix): wrap is the published mix contract.\n    a.wrapping_mul(b)\n}"
        ),
        Some(0),
        "an adjacent named INVARIANT proof stands"
    );
}

#[test]
fn bite_capacity_min_cap() {
    assert_eq!(
        detonates("capacity_min_cap", "fn f(n: usize) -> Vec<u8> { Vec::with_capacity(n.min(1024)) }"),
        Some(1)
    );
}

// --- silenced lints -------------------------------------------------------------

#[test]
fn bite_allow_dead_code() {
    assert_eq!(detonates("allow_dead_code", "#[allow(dead_code)]\nfn f() {}"), Some(1));
}

#[test]
fn bite_allow_unused() {
    assert_eq!(detonates("allow_unused", "#[allow(unused_variables)]\nfn f() { let x = 1; }"), Some(1));
}

#[test]
fn bite_allow_clippy() {
    assert_eq!(detonates("allow_clippy", "#[allow(clippy::all)]\nfn f() {}"), Some(1));
}

#[test]
fn bite_allow_missing_docs() {
    assert_eq!(detonates("allow_missing_docs", "#![allow(missing_docs)]\nfn f() {}"), Some(1));
}

#[test]
fn bite_allow_private() {
    assert_eq!(detonates("allow_private", "#[allow(private_interfaces)]\nfn f() {}"), Some(1));
}

#[test]
fn bite_allow_unsafe() {
    assert_eq!(detonates("allow_unsafe", "#![allow(unsafe_code)]\nfn f() {}"), Some(1));
}

// --- exhaustiveness and construction ---------------------------------------------

#[test]
fn bite_catchall_arm() {
    assert_eq!(detonates("catchall_arm", "fn f(x: u8) -> u8 { match x { 1 => 1, _ => 0 } }"), Some(1));
}

#[test]
fn bite_default_derive() {
    assert_eq!(detonates("default_derive", "#[derive(Default)]\nstruct S { x: u8 }"), Some(1));
}

#[test]
fn bite_default_impl() {
    assert_eq!(
        detonates("default_impl", "struct S;\nimpl Default for S { fn default() -> S { S } }"),
        Some(1)
    );
}

#[test]
fn bite_construction_door() {
    assert_eq!(detonates("construction_door", "fn from_raw(x: u8) -> u8 { x }"), Some(1));
    assert_eq!(detonates("construction_door", "struct S(u8);\nimpl S { fn new_unchecked(x: u8) -> S { S(x) } }"), Some(1));
    // BANNED #7's named example: infallible from_bytes admits anything.
    assert_eq!(
        detonates("construction_door", "struct K([u8; 4]);\nimpl K { fn from_bytes(b: [u8; 4]) -> K { K(b) } }"),
        Some(1)
    );
    // A from_bytes that can refuse is a validated admission door, not this shape.
    assert_eq!(
        detonates("construction_door", "struct K(u8);\nimpl K { fn from_bytes(b: &[u8]) -> Option<K> { b.first().map(|x| K(*x)) } }"),
        Some(0)
    );
}

#[test]
fn bite_naked_array_sig() {
    assert_eq!(detonates("naked_array_sig", "fn seal_key(k: [u8; 32]) -> [u8; 32] { k }"), Some(1));
    // The wrap-door exemption is impl-scoped ONLY: a free fn named like a
    // door is still a naked seam.
    assert_eq!(detonates("naked_array_sig", "fn from_bytes(k: [u8; 32]) -> [u8; 32] { k }"), Some(1));
    assert_eq!(
        detonates("naked_array_sig", "struct D([u8; 32]);\nimpl D { fn from_derived(b: [u8; 32]) -> D { D(b) } }"),
        Some(0)
    );
}

// --- tests that can pass without proving anything -----------------------------------

#[test]
fn bite_test_err_early_return() {
    assert_eq!(
        detonates(
            "test_err_early_return",
            "fn t(r: Result<u8, u8>) { match r { Ok(v) => { assert!(v > 0); } Err(_) => return, } }"
        ),
        Some(1)
    );
}

#[test]
fn bite_ignore_test() {
    assert_eq!(detonates("ignore_test", "#[ignore]\nfn t() {}"), Some(1));
}

#[test]
fn bite_should_panic() {
    assert_eq!(detonates("should_panic", "#[should_panic]\nfn t() {}"), Some(1));
}

// --- process and scheduling dishonesty ------------------------------------------------

#[test]
fn bite_process_exit() {
    assert_eq!(detonates("process_exit", "fn f() { std::process::exit(1); }"), Some(1));
}

#[test]
fn bite_sleep_sync() {
    assert_eq!(
        detonates("sleep_sync", "fn f() { std::thread::sleep(std::time::Duration::from_millis(5)); }"),
        Some(1)
    );
}

// --- wire-format and determinism laws ---------------------------------------------------

#[test]
fn bite_serde_default_skip() {
    assert_eq!(detonates("serde_default_skip", "struct S { #[serde(default)] x: u8 }"), Some(1));
    assert_eq!(detonates("serde_default_skip", "struct S { #[serde(skip)] x: u8 }"), Some(1));
}

#[test]
fn bite_nondeterminism() {
    assert_eq!(detonates("nondeterminism", "fn f() -> u128 { std::time::Instant::now().elapsed().as_nanos() }"), Some(1));
}

#[test]
fn bite_peer_dial() {
    assert_eq!(
        detonates("peer_dial", "fn f() { let _c = std::net::TcpStream::connect(\"127.0.0.1:1\"); }"),
        Some(1)
    );
}

#[test]
fn bite_unsafe_token() {
    assert_eq!(detonates("unsafe_token", "fn f() { unsafe { std::hint::unreachable_unchecked() } }"), Some(1));
}

// --- graph: relational lies ---------------------------------------------------------------

#[test]
fn bite_derive_bypass() {
    let f = match parsed(
        "crates/x/src/a.rs",
        "#[derive(Default)] struct Interval(u8);\nimpl Interval { fn new(x: u8) -> Result<Interval, ()> { if x > 0 { Ok(Interval(x)) } else { Err(()) } } }",
    ) {
        Some(f) => f,
        None => panic!("fixture parses"),
    };
    assert_eq!(graph::derive_bypass(&[&f]).len(), 1);
}

#[test]
fn bite_copy_detector() {
    let body = "{ let mut acc = 0; for i in 0..100 { if i % 2 == 0 { acc += i * 3 + 7; } else { acc -= i / 2 + 11; } while acc > 500 { acc /= 2; } match acc { 0 => acc = 1, 1 => acc = 2, other => acc = other - 1, } } acc }";
    let a = parsed("crates/x/src/a.rs", &format!("fn alpha() -> i64 {body}"));
    let b = parsed("crates/y/src/b.rs", &format!("fn beta() -> i64 {body}"));
    match (a, b) {
        (Some(a), Some(b)) => assert_eq!(graph::copy_detector(&[&a, &b]).len(), 1),
        (_, _) => panic!("fixtures parse"),
    }
}

#[test]
fn bite_agreement_registry() {
    let f = match parsed("crates/x/src/a.rs", "fn law_alive() {}") {
        Some(f) => f,
        None => panic!("fixture parses"),
    };
    let reg = "[[agreement]]\ntest_fn = \"law_alive\"\n[[agreement]]\ntest_fn = \"law_deleted\"\n";
    let hits = graph::agreement_registry(&[&f], reg);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].construct.contains("law_deleted"));
}

// --- meta: the detector policing itself ------------------------------------------------------

#[test]
fn bite_coverage() {
    let b = Boundary {
        files: vec![],
        unparsed: vec![],
        existing: vec!["crates/x/src/lib.rs".to_string()],
    };
    assert_eq!(meta::coverage(&b).len(), 1, "an unvisited existing file is a hole");
}

#[test]
fn bite_unparsed() {
    let b = Boundary {
        files: vec![],
        unparsed: vec![UnparsedFile {
            rel_path: "crates/x/src/broken.rs".to_string(),
            error: "expected `{`".to_string(),
        }],
        existing: vec!["crates/x/src/broken.rs".to_string()],
    };
    assert_eq!(meta::unparsed(&b).len(), 1, "an unparseable file is reported, never skipped");
}

#[test]
fn bite_forbid_roots() {
    let f = match parsed("crates/x/src/lib.rs", "pub fn f() {}") {
        Some(f) => f,
        None => panic!("fixture parses"),
    };
    let b = Boundary {
        existing: vec![f.rel_path.clone()],
        files: vec![f],
        unparsed: vec![],
    };
    assert_eq!(meta::forbid_roots(&b).len(), 1);
}

// --- the registry binds to THIS file ----------------------------------------------------------

#[test]
fn the_real_registry_loads_and_binds_to_this_file() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let waivers = match bs_detector::waiver::WaiverFile::load(&root.join("waivers.toml")) {
        Ok(w) => w,
        Err(e) => panic!("waivers.toml must be lawful: {e:#}"),
    };
    let reg = match bs_detector::registry::Registry::load(
        &root.join("checks.toml"),
        &waivers,
        include_str!("bite_proofs.rs"),
    ) {
        Ok(r) => r,
        Err(e) => panic!("checks.toml must be lawful: {e:#}"),
    };
    assert!(reg.checks.len() >= 43, "every engine's checks are registered");
}

// --- ported ratchets: the old xtask checks' historical bugs still bite ---------------------------

#[test]
fn bite_assert_bang() {
    // The RelationId shape: stored bytes bound-checked by assert on a
    // production decode path.
    assert_eq!(detonates("assert_bang", "fn decode(b: &[u8]) -> u64 { assert!(b.len() >= 8); 0 }"), Some(1));
    assert_eq!(detonates("assert_bang", "#[test]\nfn t() { assert_eq!(1, 1); }"), Some(0));
}

#[test]
fn bite_condemned_boundary() {
    assert_eq!(
        detonates("condemned_boundary", "struct T { put_triggers: Vec<String> }"),
        Some(1),
        "raw-source trigger field"
    );
    assert_eq!(
        detonates(
            "condemned_boundary",
            "fn f(filter: u8, extractor: u8) -> String { format!(\"if({filter}, {extractor})\") }"
        ),
        Some(1),
        "Display splice"
    );
    assert_eq!(
        detonates(
            "condemned_boundary",
            "struct S { extractor: String }\nfn f(e: u8) -> S { S { extractor: e.to_string() } }"
        ),
        Some(1),
        "to_string capture"
    );
}

#[test]
fn bite_hand_layout() {
    let src = "struct H; impl H { fn update(&mut self, _b: &[u8]) {} }\nfn seal(h: &mut H) { h.update(b\"kyzo:checkpoint:v1\"); }";
    let hits = match shape_hits("hand_layout", src) {
        Some(h) => h,
        None => panic!("matcher registered and fixture parses"),
    };
    assert_eq!(hits.len(), 1);
    // The one canonical constructor is exempt BY NAME — its sites are the
    // authority this law protects.
    let m = match shape::matcher_by_name("hand_layout") {
        Some(m) => m,
        None => panic!("registered"),
    };
    let f = match parsed("crates/kyzo-core/src/store/transcript.rs", src) {
        Some(f) => f,
        None => panic!("fixture parses"),
    };
    assert!(shape::run_matcher(m, &f).is_empty());
}
