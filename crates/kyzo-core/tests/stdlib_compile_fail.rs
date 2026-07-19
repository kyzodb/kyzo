//! Trybuild harness: stdlib seals (OpDecl / BoundOp / TagOrdered).

#[test]
fn opdecl_body_unconstructible() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/opdecl_body_refused.rs");
}

#[test]
fn bound_op_mint_unconstructible() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/bound_op_mint_refused.rs");
}

#[test]
fn tag_ordered_score_unconstructible() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/tag_ordered_score_refused.rs");
}
