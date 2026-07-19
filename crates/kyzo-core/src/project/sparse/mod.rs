//! Sparse vector index projections.

#[allow(clippy::module_inception)] // zone folder + impl module share the seat name by design
pub(crate) mod sparse;
#[cfg(test)]
pub(crate) mod sparse_hostile;
