//! Compile-fail proofs for #303 T2 key-shape split.
#[test]
fn storage_key_rejects_tuple_key() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/storage_key_rejects_tuple_key.rs");
}
