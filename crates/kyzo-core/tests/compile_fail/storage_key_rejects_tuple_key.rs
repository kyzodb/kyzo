//! Compile-fail: a bare TupleKey is not a StorageKey — cross-use refuses.
use kyzo::{StorageKey, TupleKey};

fn needs_storage(_k: StorageKey) {}

fn main() {
    let bare = TupleKey::from_values(&[]);
    needs_storage(bare);
}
