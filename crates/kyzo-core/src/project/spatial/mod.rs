//! Geospatial index projections.
//!
//! Search door: [`RelationIndexSearch`](crate::project::projection::RelationIndexSearch)
//! on [`spatial::Spatial`]. Session `IndexKind::Spatial` create/mutation arm is
//! [OPEN] — the algorithm seats here are complete; the allow covers surfaces
//! waiting on that host arm (not mid-wiring placeholders).
#![allow(dead_code)] // [OPEN] session IndexKind::Spatial host arm

#[allow(clippy::module_inception)] // zone folder + impl module share the seat name by design
pub(crate) mod spatial;
