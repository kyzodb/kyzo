//! Schema vocabulary: columns and stored relation metadata.
pub mod column;
pub mod relation;

pub use column::{ColType, ColumnDef, NullableColType, VecElementType};
pub use relation::{CompatibleInputSchema, RelationWriteShape, StoredRelationMetadata};
