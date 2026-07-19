//! Compile-fail: BoundOp is not constructible at the public door.
//! Minting is sealed inside bind_op; external struct literals are refused.

fn main() {
    let _ = kyzo::BoundOp {
        decl: kyzo_model::program::op::OP_ADD,
        body: (|_| unimplemented!()) as fn(&[kyzo_model::DataValue]) -> miette::Result<kyzo_model::DataValue>,
    };
}
