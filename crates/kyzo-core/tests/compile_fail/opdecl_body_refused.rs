//! Compile-fail: OpDecl cannot carry a callable body (Unconstructible).
use kyzo_model::OpDecl;

fn main() {
    let _ = OpDecl {
        name: "OP_ADD",
        min_arity: 0,
        vararg: true,
        deterministic: true,
        body: 0u8,
    };
}
