//! Compile-fail: metric scores cannot sit on a TagOrdered path (§14).
//! TagOrdered is Unconstructible — there is no such type to name.

fn main() {
    let _score: kyzo::TagOrdered = unimplemented!();
}
