/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * Original work: KyzoDB adds a geospatial access path (capability #44, absorbed
 * into story #3) that has no CozoDB antecedent. The scalar `haversine` in
 * `data/functions.rs` (a CozoDB inheritance) is the exact re-scoring primitive;
 * everything here — the curve, the quantization format, the range
 * decomposition, the expanding-ring kNN — is new, and mirrors the KyzoDB
 * index-as-stored-relation pattern of `runtime/{fts_index,hnsw}.rs`:
 *
 * - The engine is PURE FUNCTIONS over the kernel's [`ReadTx`]/[`WriteTx`]
 *   species ([`spatial_put`], [`spatial_del`], [`spatial_range_query`],
 *   [`spatial_knn`]); the RA operator tier drives search per parent tuple, the
 *   mutation tier drives put/del. The RA/catalog/lifecycle wiring is a
 *   companion patch, staged to rebase onto the settled `query/ra.rs` exactly as
 *   `ra-search-nodes.patch.md` stages HNSW/FTS/LSH.
 * - The curve encoding lives INSIDE the memcmp law (`data/memcmp.rs`: bytewise
 *   order == semantic value order). A point's curve index is 32-bit-per-
 *   dimension Z-order (Morton), stored as its 8 BIG-ENDIAN bytes in a `Bytes`
 *   key column, so memcmp order == big-endian u64 order == Morton curve order.
 *   See [`morton_encode`] and the module docs.
 * - NaN / out-of-range coordinates are unrepresentable past admission: every
 *   coordinate enters through [`GeoPoint::admit`] and every query box through
 *   [`BoundingBox::admit`] (the `IndexVec::admit` "parse, don't validate"
 *   discipline). A missed point is a wrong answer, so the curve scan
 *   OVER-approximates and an exact predicate filters; it never under-scans.
 * - Corruption is the shared typed [`IndexRowCorrupt`], never a panic: every
 *   stored index row is decoded fallibly with the row's key context.
 * - Determinism: canonical result order everywhere (range → ascending
 *   `(curve, src_key)`; kNN → ascending `(angular_distance, src_key)`); no
 *   hash-map iteration order escapes into a result.
 */

//! The geospatial index engine: a space-filling-curve access path over the
//! kernel's transaction species. Spatial proximity becomes an ordinary ordered
//! scan — a bounding-box or nearest-neighbour query is a bounded set of range
//! scans over the curve plus an exact great-circle re-check.
//!
//! A spatial index IS a stored relation ([`spatial_index_metadata`]): one row
//! per indexed base row, keyed `[curve, src_key…]` where `curve` is the 8
//! big-endian bytes of the point's 64-bit Z-order (Morton) index, with the
//! exact `(lat, lon)` carried in the value columns for the precise re-check.
//! Because the key sorts along the curve, a spatially-local query is a locally-
//! contiguous scan.
//!
//! ## The curve and its format (on-disk)
//!
//! Coordinates (`lat ∈ [-90,90]`, `lon ∈ [-180,180]`, finite) are quantized to
//! [`CURVE_BITS`] = 32 bits per dimension and bit-interleaved (lat → even bits,
//! lon → odd bits) into a single u64 Morton code — sub-centimetre resolution.
//! The code is stored big-endian so byte order equals curve order (THE law).
//! `CURVE_BITS` is a **format decision**, pinned by the literal-code fixtures in
//! the tests; changing it is a data migration.
//!
//! ## Queries (curve over-approximates, exact predicate filters)
//!
//! - **Bounding box** ([`spatial_range_query`]): the box's quantized image is
//!   decomposed into a bounded set of contiguous Morton ranges by recursive
//!   quadtree / LITMAX-BIGMIN splitting ([`decompose_box`]); each range is one
//!   ordered scan; each scanned row is re-checked against the exact `lat/lon`
//!   box. A cell is discarded only when provably disjoint, so the scan never
//!   misses an in-box point.
//! - **k-NN** ([`spatial_knn`]): an expanding box centred on the query, grown
//!   until the k-th nearest is proven closer than anything outside the box, with
//!   exact haversine re-scoring. Near the antimeridian or a pole the box snaps
//!   to full longitude (over-scan only), keeping k-NN exact everywhere.
//!
//! ## Boundary policy (deliberate)
//!
//! Range/bbox queries take **non-wrapping** boxes; one that would cross ±180° is
//! refused typed ([`AntimeridianBoxRefused`]) — a wrapping search is the union
//! of two non-wrapping boxes. A lat/lon box near a pole means exactly the
//! rectangle (no wrap over the pole). k-NN, which must be exact, over-scans a
//! full-longitude band at the seams. See [`BoundingBox`] and [`spatial_knn`].
//!
//! ## Distance unit
//!
//! Distances are the **angular** great-circle distance in radians, identical to
//! `data/functions.rs::op_haversine_deg_input`; a caller wanting metres
//! multiplies by their chosen Earth radius. The engine takes no stance on the
//! figure of the Earth.
//!
//! ## Seams (companion wiring — not this file)
//!
//! - **RA operator tier** (`query/ra.rs`): drives the search functions per
//!   parent tuple, appends the base row (+ angular distance for k-NN).
//! - **Mutation tier**: [`spatial_put`] after every base put, [`spatial_del`]
//!   before every delete (del-before-put on update — a moved point changes its
//!   curve cell), same transaction.
//! - **Lifecycle tier**: `::spatial create/drop` — create the index relation
//!   from [`spatial_index_metadata`], record `lat_field`/`lon_field`, backfill
//!   via [`spatial_put`], attach the manifest keeping `indices` sorted by name.
//!
//! ## Projection kind (story #305)
//!
//! [`Spatial`] is this engine's `K` parameterization of the shared
//! [`crate::engines::projection`] build→seal→query machine. Build→seal→query
//! goes through that machine; there is no bespoke per-engine seal or
//! freshness protocol. Relation-backed [`spatial_put`] /
//! [`spatial_range_query`] / [`spatial_knn`] remain the kernel curve algorithms.

use std::collections::BinaryHeap;

use miette::{Diagnostic, Result, bail, miette};
use ordered_float::OrderedFloat;
use rustc_hash::FxHashSet;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::value::{DataValue, ScanBound, Tuple};
use crate::engines::{IndexCorruptReason, IndexRowCorrupt};
use crate::engines::projection::ProjectionKind;
use crate::runtime::relation::RelationHandle;
use crate::storage::{ReadTx, WriteTx};

// ---------------------------------------------------------------------------
// Projection kind — `K` of the shared build→seal→query machine (#305).
// ---------------------------------------------------------------------------

/// Geospatial index as a projection kind: one `K` of
/// [`ProjectionBuilder`](crate::engines::projection::ProjectionBuilder) /
/// [`Sealed`](crate::engines::projection::Sealed).
///
/// Relation-backed curve maintenance and search ([`spatial_put`],
/// [`spatial_range_query`], [`spatial_knn`]) are the kernel algorithms — not
/// a second build/seal/freshness protocol.
///
/// Constructed at seal sites once generation freshness is seated (T5 /
/// projections-views); the type is live under the machine's tests today.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct Spatial;

/// Query surface for a sealed [`Spatial`] projection: range or k-NN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum SpatialQuery {
    /// Bounding-box range — no `k` truncation in the search law itself.
    Range,
    /// Expanding-ring k-NN with result-set bound `k`.
    Knn { k: usize },
}

impl ProjectionKind for Spatial {
    type Query = SpatialQuery;
    /// For range: `0` (unbounded by k). For knn: the `k` bound.
    type Candidates = usize;

    fn search(&self, query: &Self::Query) -> Self::Candidates {
        match *query {
            SpatialQuery::Range => 0,
            SpatialQuery::Knn { k } => k,
        }
    }
}

// ---------------------------------------------------------------------------
// Format constants (on-disk).
// ---------------------------------------------------------------------------

/// Bits of precision per coordinate dimension. **This is a format decision.**
/// Two 32-bit quantized coordinates interleave into one 64-bit Morton code — a
/// single machine word, eight big-endian bytes, one `Bytes` key column — at
/// sub-centimetre resolution (lon: `360/2³² ≈ 9.3 mm` at the equator; lat:
/// `~4.6 mm`). Pinned by the literal-code fixtures; changing it re-encodes every
/// stored curve index and is a data migration (bump `FormatVersion::CURRENT`).
/// If it ever becomes configurable it must move into [`SpatialIndexManifest`].
const CURVE_BITS: u32 = 32;

/// `2^CURVE_BITS`, as the divisor of the quantization. Kept as `f64`: the
/// quantization fraction is computed in floating point.
const SCALE: f64 = 4_294_967_296.0; // 2^32

const LAT_MIN: f64 = -90.0;
const LAT_MAX: f64 = 90.0;
const LON_MIN: f64 = -180.0;
const LON_MAX: f64 = 180.0;

/// Hard cap on the number of scan ranges a box decomposes into. Beyond it the
/// decomposition coarsens (emits whole cells — a wider over-approximation, never
/// an under-approximation), so a pathological box costs more re-check but never
/// a wrong answer. Recursion depth is separately bounded by `2·CURVE_BITS`.
const SPLIT_BUDGET: usize = 1024;

const DEG_TO_RAD: f64 = std::f64::consts::PI / 180.0;

// ---------------------------------------------------------------------------
// Typed errors (admission + query boundary policy). Corruption reuses the
// shared `IndexRowCorrupt`.
// ---------------------------------------------------------------------------

/// A coordinate outside the admissible range was submitted. Refused typed at
/// admission: an out-of-range coordinate cannot be quantized onto the curve.
#[derive(Debug, Error, Diagnostic)]
#[error("coordinate out of range: {what} = {value} (lat ∈ [-90,90], lon ∈ [-180,180])")]
#[diagnostic(code(index::spatial::coord_out_of_range))]
pub(crate) struct GeoCoordOutOfRange {
    pub(crate) what: &'static str,
    pub(crate) value: f64,
}

/// A non-finite coordinate (NaN or infinity) was submitted. Refused typed: NaN
/// is unrepresentable on the curve and would corrupt ordering.
#[derive(Debug, Error, Diagnostic)]
#[error("coordinate is not finite: {what}")]
#[diagnostic(code(index::spatial::non_finite_coord))]
pub(crate) struct NonFiniteCoord {
    pub(crate) what: &'static str,
}

/// A bounding-box query that would wrap across the antimeridian (±180°) was
/// submitted (its `lon_lo > lon_hi`). Refused typed in v1: a wrapping search is
/// the union of two non-wrapping boxes.
#[derive(Debug, Error, Diagnostic)]
#[error("bounding box crosses the antimeridian (lon_lo {lon_lo} > lon_hi {lon_hi})")]
#[diagnostic(code(index::spatial::antimeridian_box))]
#[diagnostic(help(
    "express a wrapping search as two non-wrapping boxes, [lon_lo, 180] and \
     [-180, lon_hi]"
))]
pub(crate) struct AntimeridianBoxRefused {
    pub(crate) lon_lo: f64,
    pub(crate) lon_hi: f64,
}

/// The spatial extractor found a non-numeric value where a coordinate column
/// was declared. A definition error surfaced at index time.
#[derive(Debug, Error, Diagnostic)]
#[error("spatial index coordinate column {field} is not a number: {got}")]
#[diagnostic(code(index::spatial::coord_type))]
pub(crate) struct CoordNotNumber {
    pub(crate) field: usize,
    pub(crate) got: String,
}

// ---------------------------------------------------------------------------
// GeoPoint: a coordinate proven fit for the curve.
// ---------------------------------------------------------------------------

/// A coordinate ADMITTED to the index: finite, `lat ∈ [-90,90]`,
/// `lon ∈ [-180,180]`. Fields are private; [`admit`](Self::admit) is the sole
/// constructor — NaN / out-of-range cannot be forged by struct literal or field
/// write outside this module (the `IndexVec::admit` pattern).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GeoPoint {
    lat: f64,
    lon: f64,
}

impl GeoPoint {
    /// Admit a coordinate, or refuse it typed: [`NonFiniteCoord`] or
    /// [`GeoCoordOutOfRange`]. The only way to construct a [`GeoPoint`].
    pub(crate) fn admit(lat: f64, lon: f64) -> Result<Self> {
        if !lat.is_finite() {
            bail!(NonFiniteCoord { what: "lat" });
        }
        if !lon.is_finite() {
            bail!(NonFiniteCoord { what: "lon" });
        }
        if !(LAT_MIN..=LAT_MAX).contains(&lat) {
            bail!(GeoCoordOutOfRange {
                what: "lat",
                value: lat
            });
        }
        if !(LON_MIN..=LON_MAX).contains(&lon) {
            bail!(GeoCoordOutOfRange {
                what: "lon",
                value: lon
            });
        }
        Ok(GeoPoint { lat, lon })
    }

    /// Admitted latitude.
    pub(crate) fn lat(&self) -> f64 {
        self.lat
    }

    /// Admitted longitude.
    pub(crate) fn lon(&self) -> f64 {
        self.lon
    }

    /// The point's quantized cell (`q_lat`, `q_lon`), each in `[0, 2³²)`.
    fn quantize(&self) -> (u32, u32) {
        (
            quantize(self.lat, LAT_MIN, LAT_MAX),
            quantize(self.lon, LON_MIN, LON_MAX),
        )
    }

    /// The point's 64-bit Z-order (Morton) curve index.
    fn curve_index(&self) -> u64 {
        let (qx, qy) = self.quantize();
        morton_encode(qx, qy)
    }

    /// The curve index as the 8 big-endian bytes stored in the key column.
    fn curve_key(&self) -> DataValue {
        DataValue::Bytes(self.curve_index().to_be_bytes().to_vec())
    }
}

/// Quantize `v ∈ [lo, hi]` to `[0, 2³²)`, monotonically non-decreasing. The
/// clamp affects only `v == hi` (the top bucket) and guards float fuzz; by
/// admission `v ∈ [lo, hi]`, so the fraction is in `[0, 1]`.
fn quantize(v: f64, lo: f64, hi: f64) -> u32 {
    let frac = (v - lo) / (hi - lo);
    let scaled = (frac * SCALE).floor();
    if scaled <= 0.0 {
        0
    } else if scaled >= SCALE {
        (SCALE as u64 - 1) as u32
    } else {
        scaled as u32
    }
}

// ---------------------------------------------------------------------------
// Morton (Z-order) codec. lat → even bit positions, lon → odd. Explicit magic
// masks; big-endian on the wire (see the module docs). Decode is for tests /
// verification — the hot path never needs it (exact lat/lon are stored).
// ---------------------------------------------------------------------------

/// Spread the 32 bits of `x` so that bit `i` lands at bit `2i` (even positions).
fn spread32(x: u32) -> u64 {
    let mut x = x as u64;
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x << 2)) & 0x3333_3333_3333_3333;
    x = (x | (x << 1)) & 0x5555_5555_5555_5555;
    x
}

/// Inverse of [`spread32`]: gather the even-position bits back into a u32.
fn compact32(x: u64) -> u32 {
    let mut x = x & 0x5555_5555_5555_5555;
    x = (x | (x >> 1)) & 0x3333_3333_3333_3333;
    x = (x | (x >> 2)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x >> 4)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x >> 8)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x >> 16)) & 0x0000_0000_FFFF_FFFF;
    x as u32
}

/// Interleave two quantized coordinates into a 64-bit Morton code.
fn morton_encode(qx: u32, qy: u32) -> u64 {
    spread32(qx) | (spread32(qy) << 1)
}

/// De-interleave a Morton code back into `(q_lat, q_lon)`.
fn morton_decode(code: u64) -> (u32, u32) {
    (compact32(code), compact32(code >> 1))
}

// ---------------------------------------------------------------------------
// The manifest and the index relation's schema.
// ---------------------------------------------------------------------------

/// The persisted description of one spatial index. Serialized (msgpack, struct
/// maps) as the payload of the base relation's `IndexKind::Spatial` catalog
/// entry — its wire form is an on-disk format, pinned by the round-trip test
/// below; changing it is a migration decision. `CURVE_BITS` is a module format
/// constant, not carried here (see its docs).
#[derive(Debug, Clone, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct SpatialIndexManifest {
    pub(crate) base_relation: SmartString<LazyCompact>,
    pub(crate) index_name: SmartString<LazyCompact>,
    /// Position, in the base relation's full tuple (keys then non-keys), of the
    /// latitude column.
    pub(crate) lat_field: usize,
    /// Position of the longitude column.
    pub(crate) lon_field: usize,
}

/// Mint the index relation's column metadata for a spatial index over `base`.
///
/// Keys: `curve` (the 8 big-endian Morton bytes), then `src_*` (the base
/// relation's key columns). Non-keys: `lat`, `lon` (the exact coordinates for
/// the precise re-check — deliberate denormalization so the filter needs no
/// base fetch per over-scanned candidate).
pub(crate) fn spatial_index_metadata(base: &StoredRelationMetadata) -> StoredRelationMetadata {
    let mut keys = vec![ColumnDef {
        name: SmartString::from("curve"),
        typing: NullableColType {
            coltype: ColType::Bytes,
            nullable: false,
        },
        default_gen: None,
    }];
    for k in base.keys.iter() {
        keys.push(ColumnDef {
            name: format!("src_{}", k.name).into(),
            typing: k.typing.clone(),
            default_gen: None,
        });
    }
    let coord = || NullableColType {
        coltype: ColType::Float,
        nullable: false,
    };
    let non_keys = vec![
        ColumnDef {
            name: SmartString::from("lat"),
            typing: coord(),
            default_gen: None,
        },
        ColumnDef {
            name: SmartString::from("lon"),
            typing: coord(),
            default_gen: None,
        },
    ];
    StoredRelationMetadata { keys, non_keys }
}

// ---------------------------------------------------------------------------
// Index maintenance.
// ---------------------------------------------------------------------------

/// Read the point named by the manifest out of one base-relation tuple, admitted.
fn extract_point(tuple: &[DataValue], manifest: &SpatialIndexManifest) -> Result<GeoPoint> {
    let coord = |field: usize| -> Result<f64> {
        let v = tuple.get(field).ok_or_else(|| {
            miette!(CoordNotNumber {
                field,
                got: "<beyond row arity>".to_string(),
            })
        })?;
        v.get_float().ok_or_else(|| {
            miette!(CoordNotNumber {
                field,
                got: format!("{v:?}"),
            })
        })
    };
    let lat = coord(manifest.lat_field)?;
    let lon = coord(manifest.lon_field)?;
    GeoPoint::admit(lat, lon)
}

/// The spatial posting key `[curve, src_key…]` for one base row.
fn posting_key(point: &GeoPoint, base_key_len: usize, tuple: &[DataValue]) -> Tuple {
    let mut key = Tuple::with_capacity(1 + base_key_len);
    key.push(point.curve_key());
    key.extend(tuple[..base_key_len].iter().cloned());
    key
}

/// Index one base-relation row: admit its coordinate, compute its curve cell,
/// and write the posting `[curve, src_key…] -> (lat, lon)`.
///
/// Contract: the mutation tier calls this after every put on the base relation,
/// in the same transaction, having first removed the row's previous posting via
/// [`spatial_del`] when the coordinate changed (a moved point lands on a
/// different curve key, so the stale posting must be deleted, not overwritten).
pub(crate) fn spatial_put<T: WriteTx>(
    tx: &mut T,
    tuple: &[DataValue],
    manifest: &SpatialIndexManifest,
    base: &RelationHandle,
    idx: &RelationHandle,
) -> Result<()> {
    let base_key_len = base.metadata.keys.len();
    if tuple.len() < base_key_len {
        bail!(IndexRowCorrupt::new(
            &base.name,
            tuple,
            IndexCorruptReason::RowShorterThanKey,
        ));
    }
    let point = extract_point(tuple, manifest)?;
    let key = posting_key(&point, base_key_len, tuple);
    let val = vec![
        DataValue::from(point.lat()),
        DataValue::from(point.lon()),
    ];
    let key_bytes = idx.encode_key_for_store(key.as_slice(), SourceSpan::default())?;
    let val_bytes = idx.encode_val_only_for_store(&val, SourceSpan::default())?;
    tx.put(&key_bytes, &val_bytes)
}

/// Un-index one base-relation row: delete the posting it contributed. The
/// coordinate must be the one the row was indexed with, so the curve key matches
/// what [`spatial_put`] wrote.
pub(crate) fn spatial_del<T: WriteTx>(
    tx: &mut T,
    tuple: &[DataValue],
    manifest: &SpatialIndexManifest,
    base: &RelationHandle,
    idx: &RelationHandle,
) -> Result<()> {
    let base_key_len = base.metadata.keys.len();
    if tuple.len() < base_key_len {
        bail!(IndexRowCorrupt::new(
            &base.name,
            tuple,
            IndexCorruptReason::RowShorterThanKey,
        ));
    }
    let point = extract_point(tuple, manifest)?;
    let key = posting_key(&point, base_key_len, tuple);
    let key_bytes = idx.encode_key_for_store(key.as_slice(), SourceSpan::default())?;
    tx.del(&key_bytes)
}

// ---------------------------------------------------------------------------
// Bounding box: a query region proven fit for a range scan.
// ---------------------------------------------------------------------------

/// A non-wrapping lat/lon query box, both corners admitted and ordered
/// (`lat_lo ≤ lat_hi`, `lon_lo ≤ lon_hi`). Fields are private; [`admit`](Self::admit)
/// is the sole constructor. A box crossing the antimeridian is refused at
/// admission ([`AntimeridianBoxRefused`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BoundingBox {
    lat_lo: f64,
    lat_hi: f64,
    lon_lo: f64,
    lon_hi: f64,
}

impl BoundingBox {
    /// Admit a query box, or refuse it typed. Both corners go through
    /// [`GeoPoint::admit`]; `lat_lo ≤ lat_hi` and `lon_lo ≤ lon_hi` are required
    /// (a `lon_lo > lon_hi` box would wrap the antimeridian and is refused).
    /// The only way to construct a [`BoundingBox`].
    pub(crate) fn admit(lat_lo: f64, lon_lo: f64, lat_hi: f64, lon_hi: f64) -> Result<Self> {
        GeoPoint::admit(lat_lo, lon_lo)?;
        GeoPoint::admit(lat_hi, lon_hi)?;
        if lat_lo > lat_hi {
            bail!(GeoCoordOutOfRange {
                what: "lat_lo > lat_hi",
                value: lat_lo,
            });
        }
        if lon_lo > lon_hi {
            bail!(AntimeridianBoxRefused { lon_lo, lon_hi });
        }
        Ok(BoundingBox {
            lat_lo,
            lat_hi,
            lon_lo,
            lon_hi,
        })
    }

    /// Admitted lower latitude bound.
    pub(crate) fn lat_lo(&self) -> f64 {
        self.lat_lo
    }

    /// Admitted upper latitude bound.
    pub(crate) fn lat_hi(&self) -> f64 {
        self.lat_hi
    }

    /// Admitted lower longitude bound.
    pub(crate) fn lon_lo(&self) -> f64 {
        self.lon_lo
    }

    /// Admitted upper longitude bound.
    pub(crate) fn lon_hi(&self) -> f64 {
        self.lon_hi
    }

    /// Whether an exact point lies within this box (inclusive). The precise
    /// predicate that filters the curve's over-approximation.
    fn contains(&self, lat: f64, lon: f64) -> bool {
        self.lat_lo <= lat && lat <= self.lat_hi && self.lon_lo <= lon && lon <= self.lon_hi
    }

    /// The box's quantized integer image `(qxlo, qxhi, qylo, qyhi)`. Because
    /// [`quantize`] is monotone, this over-covers the coordinate box: every
    /// point in the box has its quantized cell inside this integer box.
    fn quantized(&self) -> QBox {
        QBox {
            xlo: quantize(self.lat_lo, LAT_MIN, LAT_MAX) as u64,
            xhi: quantize(self.lat_hi, LAT_MIN, LAT_MAX) as u64,
            ylo: quantize(self.lon_lo, LON_MIN, LON_MAX) as u64,
            yhi: quantize(self.lon_hi, LON_MIN, LON_MAX) as u64,
        }
    }
}

// ---------------------------------------------------------------------------
// Curve range decomposition (recursive quadtree / LITMAX-BIGMIN).
// ---------------------------------------------------------------------------

/// A query box in quantized integer coordinates (inclusive on all four sides).
#[derive(Debug, Clone, Copy)]
struct QBox {
    xlo: u64,
    xhi: u64,
    ylo: u64,
    yhi: u64,
}

/// The inclusive Morton-code range `[lo, hi]` covered by a quadtree cell at
/// `level` with corner `(x0, y0)` (a `2^level × 2^level` aligned block). A cell's
/// codes are contiguous because the low `2·level` bits range over every
/// combination. The whole-space cell (`level == 32`) is the full u64 range.
fn cell_range(level: u32, x0: u64, y0: u64) -> (u64, u64) {
    let span_bits = 2 * level;
    if span_bits >= 64 {
        (0, u64::MAX)
    } else {
        let base = morton_encode(x0 as u32, y0 as u32);
        (base, base + ((1u64 << span_bits) - 1))
    }
}

/// Decompose a quantized box into a bounded set of contiguous, merged Morton
/// ranges. A cell is discarded ONLY when provably disjoint from the box; a cell
/// fully inside is emitted whole; a straddling cell is split into its four
/// Morton-ordered children — until the [`SPLIT_BUDGET`] is hit, past which a
/// straddling cell is emitted whole (coarser over-approximation, never an
/// under-approximation).
fn decompose_box(qbox: &QBox) -> Vec<(u64, u64)> {
    let mut raw = Vec::new();
    decompose_cell(CURVE_BITS, 0, 0, qbox, &mut raw);
    // Merge touching / overlapping ranges into a minimal set. Emission is
    // Morton-ordered, so sorting then coalescing yields the canonical ranges.
    raw.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(raw.len());
    for (lo, hi) in raw {
        match merged.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => {
                if hi > last.1 {
                    last.1 = hi;
                }
            }
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

fn decompose_cell(level: u32, x0: u64, y0: u64, qbox: &QBox, out: &mut Vec<(u64, u64)>) {
    let size = 1u64 << level; // level ≤ 32 ⇒ ≤ 2^32, fits u64
    let x1 = x0 + size - 1;
    let y1 = y0 + size - 1;
    // Provably disjoint → discard (the only place a cell is dropped).
    if x1 < qbox.xlo || x0 > qbox.xhi || y1 < qbox.ylo || y0 > qbox.yhi {
        return;
    }
    // Fully inside → one contiguous range.
    if x0 >= qbox.xlo && x1 <= qbox.xhi && y0 >= qbox.ylo && y1 <= qbox.yhi {
        out.push(cell_range(level, x0, y0));
        return;
    }
    // A single cell, or the budget is exhausted → emit whole (over-approx).
    if level == 0 || out.len() >= SPLIT_BUDGET {
        out.push(cell_range(level, x0, y0));
        return;
    }
    // Straddling → recurse into the four children in Morton order:
    // (x0,y0), (x0+half,y0), (x0,y0+half), (x0+half,y0+half).
    let half = 1u64 << (level - 1);
    decompose_cell(level - 1, x0, y0, qbox, out);
    decompose_cell(level - 1, x0 + half, y0, qbox, out);
    decompose_cell(level - 1, x0, y0 + half, qbox, out);
    decompose_cell(level - 1, x0 + half, y0 + half, qbox, out);
}

// ---------------------------------------------------------------------------
// Row decode.
// ---------------------------------------------------------------------------

/// One decoded index posting: the base row's key columns and the exact point.
struct Posting {
    src_key: Tuple,
    lat: f64,
    lon: f64,
}

/// Decode a full spatial index tuple `[curve, src_key…, lat, lon]`. Anything
/// that does not fit is the typed [`IndexRowCorrupt`] with the row's context.
fn decode_posting(row: &[DataValue], base_key_len: usize, index_name: &str) -> Result<Posting> {
    let expected_len = base_key_len + 3; // curve + src_key… + lat + lon
    if row.len() != expected_len {
        bail!(IndexRowCorrupt::new(
            index_name,
            row,
            IndexCorruptReason::WrongColumnCount {
                found: row.len(),
                expected: expected_len,
            },
        ));
    }
    // The curve column must be exactly the 8 bytes it was stored as.
    match row[0].get_bytes() {
        Some(b) if b.len() == 8 => {}
        _ => bail!(IndexRowCorrupt::new(
            index_name,
            row,
            IndexCorruptReason::SpatialCurveNot8Bytes,
        )),
    }
    let lat = row[base_key_len + 1].get_float().ok_or_else(|| {
        miette!(IndexRowCorrupt::new(
            index_name,
            row,
            IndexCorruptReason::SpatialLatNotNumber,
        ))
    })?;
    let lon = row[base_key_len + 2].get_float().ok_or_else(|| {
        miette!(IndexRowCorrupt::new(
            index_name,
            row,
            IndexCorruptReason::SpatialLonNotNumber,
        ))
    })?;
    Ok(Posting {
        src_key: Tuple::from_vec(row[1..=base_key_len].to_vec()),
        lat,
        lon,
    })
}

/// Fetch the base row a posting points at; its absence is index corruption.
fn fetch_base(
    tx: &impl ReadTx,
    base: &RelationHandle,
    idx: &RelationHandle,
    src_key: &[DataValue],
) -> Result<Tuple> {
    base.get(tx, src_key)?.ok_or_else(|| {
        miette!(IndexRowCorrupt::new(
            &idx.name,
            src_key,
            IndexCorruptReason::BaseRowMissing,
        ))
    })
}

// ---------------------------------------------------------------------------
// Bounding-box range query.
// ---------------------------------------------------------------------------

/// Every posting whose exact point lies in `bbox`, decoded, in ascending
/// `(curve, src_key)` order — the natural order of scanning the merged Morton
/// ranges. Shared by [`spatial_range_query`] and the k-NN ring scan.
fn scan_box(
    tx: &impl ReadTx,
    base: &RelationHandle,
    idx: &RelationHandle,
    bbox: &BoundingBox,
) -> Result<Vec<Posting>> {
    let base_key_len = base.metadata.keys.len();
    let ranges = decompose_box(&bbox.quantized());
    let mut out = Vec::new();
    for (lo, hi) in ranges {
        let lower = [ScanBound::Value(DataValue::Bytes(
            lo.to_be_bytes().to_vec(),
        ))];
        let upper = [ScanBound::Value(DataValue::Bytes(
            hi.to_be_bytes().to_vec(),
        ))];
        for row in
            crate::engines::index_rows(&idx.name, idx.scan_bounded_prefix(tx, &[], &lower, &upper))
        {
            let row = row?;
            let posting = decode_posting(row.as_slice(), base_key_len, &idx.name)?;
            // The curve over-approximates; the exact predicate filters.
            if bbox.contains(posting.lat, posting.lon) {
                out.push(posting);
            }
        }
    }
    Ok(out)
}

/// Bounding-box search: the base rows whose point lies in `bbox`, in canonical
/// ascending `(curve, src_key)` order.
pub(crate) fn spatial_range_query(
    tx: &impl ReadTx,
    base: &RelationHandle,
    idx: &RelationHandle,
    bbox: &BoundingBox,
) -> Result<Vec<Tuple>> {
    scan_box(tx, base, idx, bbox)?
        .into_iter()
        .map(|p| fetch_base(tx, base, idx, p.src_key.as_slice()))
        .collect()
}

// ---------------------------------------------------------------------------
// k-nearest-neighbour by expanding ring.
// ---------------------------------------------------------------------------

/// The parameters of one k-NN query.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KnnParams {
    pub(crate) k: usize,
    /// Append the angular great-circle distance (radians) as a trailing `Float`.
    pub(crate) bind_distance: bool,
}

/// The angular great-circle distance (radians) between two points given in
/// degrees — identical to `data/functions.rs::op_haversine_deg_input`, the
/// engine's exact re-scoring primitive.
fn angular_distance(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, l1, p2, l2) = (
        lat1 * DEG_TO_RAD,
        lon1 * DEG_TO_RAD,
        lat2 * DEG_TO_RAD,
        lon2 * DEG_TO_RAD,
    );
    2.0 * f64::asin(f64::sqrt(
        f64::sin((p1 - p2) / 2.0).powi(2)
            + f64::cos(p1) * f64::cos(p2) * f64::sin((l1 - l2) / 2.0).powi(2),
    ))
}

/// The clamped/wrapped search box for a k-NN ring of half-extent `half` degrees
/// around `p`, together with whether it reached each seam. Near ±180° or a pole
/// the longitude snaps to full range (over-scan, keeping k-NN exact).
struct RingBox {
    bbox: BoundingBox,
    /// Longitude covers the full `[-180,180]` (no longitude exterior).
    full_lon: bool,
    /// Latitude reached the north pole (no exterior above).
    at_north: bool,
    /// Latitude reached the south pole (no exterior below).
    at_south: bool,
}

fn ring_box(p: &GeoPoint, half: f64) -> Result<RingBox> {
    let raw_lat_lo = p.lat() - half;
    let raw_lat_hi = p.lat() + half;
    let raw_lon_lo = p.lon() - half;
    let raw_lon_hi = p.lon() + half;

    let at_south = raw_lat_lo <= LAT_MIN;
    let at_north = raw_lat_hi >= LAT_MAX;
    // Crossing ±180°, or capping a pole (points "over" the pole sit at lon+180),
    // both require the full longitude band to stay exact.
    let full_lon = raw_lon_lo < LON_MIN || raw_lon_hi > LON_MAX || at_south || at_north;

    let lat_lo = raw_lat_lo.max(LAT_MIN);
    let lat_hi = raw_lat_hi.min(LAT_MAX);
    let (lon_lo, lon_hi) = if full_lon {
        (LON_MIN, LON_MAX)
    } else {
        (raw_lon_lo, raw_lon_hi)
    };
    Ok(RingBox {
        bbox: BoundingBox::admit(lat_lo, lon_lo, lat_hi, lon_hi)?,
        full_lon,
        at_north,
        at_south,
    })
}

/// A safe UNDER-estimate of the radius (radians) of the largest great-circle
/// ball centred at `p` that fits inside the ring box: the minimum great-circle
/// distance from `p` to any box edge that has exterior beyond it. Under-
/// estimating only makes the k-NN stop rule stricter (more expansion), never
/// premature — so it can never miss a nearer neighbour. `+∞` when a direction
/// has no exterior (full-longitude band, or a pole-capped latitude edge).
fn inner_radius(p: &GeoPoint, ring: &RingBox) -> f64 {
    let b = &ring.bbox;
    let mut r = f64::INFINITY;
    // Latitude edges are exact meridian arcs.
    if !ring.at_south {
        r = r.min((p.lat() - b.lat_lo()) * DEG_TO_RAD);
    }
    if !ring.at_north {
        r = r.min((b.lat_hi() - p.lat()) * DEG_TO_RAD);
    }
    // Longitude edges: cross-track distance to the meridian great circle is a
    // safe under-estimate of the distance to the meridian segment.
    if !ring.full_lon {
        let cos_lat = f64::cos(p.lat() * DEG_TO_RAD);
        let x_lo = f64::asin(f64::sin((p.lon() - b.lon_lo()) * DEG_TO_RAD) * cos_lat);
        let x_hi = f64::asin(f64::sin((b.lon_hi() - p.lon()) * DEG_TO_RAD) * cos_lat);
        r = r.min(x_lo).min(x_hi);
    }
    r
}

/// A k-NN candidate ordered so a max-heap evicts the farthest (largest distance,
/// then largest `src_key`) — the deterministic tie-break that keeps the smallest
/// `(distance, src_key)` when the heap overflows `k`.
#[derive(PartialEq, Eq)]
struct Candidate {
    dist: OrderedFloat<f64>,
    src_key: Tuple,
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.dist
            .cmp(&other.dist)
            .then_with(|| self.src_key.cmp(&other.src_key))
    }
}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The k-NN seed half-extent in degrees. Small enough to keep dense-data scans
/// tight; the geometric doubling reaches the whole globe in a handful of rounds
/// for sparse data.
const KNN_SEED_HALF_DEG: f64 = 1.0;

/// k-nearest search: the `k` base rows nearest `query` by great-circle distance,
/// nearest first, ties broken by `src_key`, each optionally extended by its
/// angular distance (radians). Exact — the curve only decides which rows to
/// re-score.
pub(crate) fn spatial_knn(
    tx: &impl ReadTx,
    base: &RelationHandle,
    idx: &RelationHandle,
    query: &GeoPoint,
    params: &KnnParams,
) -> Result<Vec<Tuple>> {
    if params.k == 0 {
        return Ok(vec![]);
    }
    let mut best: BinaryHeap<Candidate> = BinaryHeap::new();
    let mut seen: FxHashSet<Tuple> = FxHashSet::default();
    let mut half = KNN_SEED_HALF_DEG;

    loop {
        let ring = ring_box(query, half)?;
        for posting in scan_box(tx, base, idx, &ring.bbox)? {
            if !seen.insert(posting.src_key.clone()) {
                continue; // already scored in an inner ring
            }
            let dist = angular_distance(query.lat(), query.lon(), posting.lat, posting.lon);
            best.push(Candidate {
                dist: OrderedFloat(dist),
                src_key: posting.src_key,
            });
            if best.len() > params.k {
                best.pop(); // evict the farthest
            }
        }

        let whole_globe = ring.full_lon && ring.at_north && ring.at_south;
        // Done when k held and all of them are proven closer than anything
        // outside the box; or when the box already spans the globe.
        if whole_globe {
            break;
        }
        if best.len() == params.k {
            let kth = best.peek().map(|c| c.dist.0).unwrap_or(f64::INFINITY);
            if kth <= inner_radius(query, &ring) {
                break;
            }
        }
        half *= 2.0;
    }

    // Canonical ascending (distance, src_key).
    let mut scored: Vec<Candidate> = best.into_vec();
    scored.sort_unstable();
    scored
        .into_iter()
        .map(|c| {
            let mut row = fetch_base(tx, base, idx, c.src_key.as_slice())?;
            if params.bind_distance {
                row.push(DataValue::from(c.dist.0));
            }
            Ok(row)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests: the engine's executable law.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::program::InputRelationHandle;
    use crate::data::symb::Symbol;
    use crate::runtime::relation::KeyspaceKind;
    use crate::runtime::relation::create_relation;
    use crate::storage::Storage;
    use crate::storage::fjall::new_fjall_storage;

    // -- a tiny deterministic PRNG (splitmix64) so tests need no rand dep -----

    struct Rng(u64);
    impl Rng {
        fn next_u64(&mut self) -> u64 {
            // INVARIANT(splitmix64): modular mix per the splitmix64 contract; wrap is the PRNG.
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        /// A uniform f64 in `[0, 1)`.
        fn unit(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }
        fn lat(&mut self) -> f64 {
            self.unit() * 180.0 - 90.0
        }
        fn lon(&mut self) -> f64 {
            self.unit() * 360.0 - 180.0
        }
    }

    // -- schema / fixture helpers --------------------------------------------

    fn col(name: &str, coltype: ColType) -> ColumnDef {
        ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType {
                coltype,
                nullable: false,
            },
            default_gen: None,
        }
    }

    fn input_handle(name: &str, metadata: StoredRelationMetadata) -> InputRelationHandle {
        let key_bindings = metadata
            .keys
            .iter()
            .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
            .collect();
        let dep_bindings = metadata
            .non_keys
            .iter()
            .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
            .collect();
        InputRelationHandle {
            name: Symbol::new(name, SourceSpan(0, 0)),
            metadata,
            key_bindings,
            dep_bindings,
            span: SourceSpan(0, 0),
        }
    }

    /// Base relation `places { id => lat, lon }`.
    fn base_meta() -> StoredRelationMetadata {
        StoredRelationMetadata {
            keys: vec![col("id", ColType::Int)],
            non_keys: vec![col("lat", ColType::Float), col("lon", ColType::Float)],
        }
    }

    fn manifest() -> SpatialIndexManifest {
        SpatialIndexManifest {
            base_relation: SmartString::from("places"),
            index_name: SmartString::from("geo"),
            lat_field: 1,
            lon_field: 2,
        }
    }

    struct Fixture {
        base: RelationHandle,
        idx: RelationHandle,
        manifest: SpatialIndexManifest,
        points: Vec<(i64, f64, f64)>,
    }

    fn setup(db: &impl Storage, points: &[(i64, f64, f64)]) -> Fixture {
        let meta = base_meta();
        let manifest = manifest();
        let mut tx = db.write_tx().unwrap();
        let base = create_relation(
            &mut tx,
            input_handle("places", meta.clone()),
            KeyspaceKind::Facts,
        )
        .unwrap();
        let idx = create_relation(
            &mut tx,
            input_handle("places:geo", spatial_index_metadata(&meta)),
            KeyspaceKind::AlgorithmState,
        )
        .unwrap();
        for (id, lat, lon) in points {
            let row = vec![
                DataValue::from(*id),
                DataValue::from(*lat),
                DataValue::from(*lon),
            ];
            base.put_fact(
                &mut tx,
                &row,
                crate::data::value::ValidityTs::from_raw(0),
                SourceSpan(0, 0),
            )
            .unwrap();
            spatial_put(&mut tx, &row, &manifest, &base, &idx).unwrap();
        }
        tx.commit().unwrap();
        Fixture {
            base,
            idx,
            manifest,
            points: points.to_vec(),
        }
    }

    fn range_ids(db: &impl Storage, f: &Fixture, bbox: &BoundingBox) -> Vec<i64> {
        let rtx = db.read_tx().unwrap();
        spatial_range_query(&rtx, &f.base, &f.idx, bbox)
            .unwrap()
            .iter()
            .map(|t| t[0].get_int().unwrap())
            .collect()
    }

    fn knn_ids(db: &impl Storage, f: &Fixture, q: &GeoPoint, k: usize) -> Vec<(i64, f64)> {
        let rtx = db.read_tx().unwrap();
        spatial_knn(
            &rtx,
            &f.base,
            &f.idx,
            q,
            &KnnParams {
                k,
                bind_distance: true,
            },
        )
        .unwrap()
        .iter()
        .map(|t| {
            (
                t[0].get_int().unwrap(),
                t.last().unwrap().get_float().unwrap(),
            )
        })
        .collect()
    }

    // -- naive references ----------------------------------------------------

    /// Full-scan reference: ids of points inside the box, ascending id.
    fn naive_range(points: &[(i64, f64, f64)], b: &BoundingBox) -> Vec<i64> {
        let mut ids: Vec<i64> = points
            .iter()
            .filter(|(_, lat, lon)| b.contains(*lat, *lon))
            .map(|(id, _, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Full-scan reference: the k nearest by exact haversine, ties by id.
    fn naive_knn(points: &[(i64, f64, f64)], q: &GeoPoint, k: usize) -> Vec<i64> {
        let mut scored: Vec<(f64, i64)> = points
            .iter()
            .map(|(id, lat, lon)| (angular_distance(q.lat(), q.lon(), *lat, *lon), *id))
            .collect();
        scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
        scored.into_iter().take(k).map(|(_, id)| id).collect()
    }

    // == Curve codec: round-trip, ordering, pinned format ====================

    #[test]
    fn curve_roundtrip_and_ordering() {
        let mut rng = Rng(0xC0FFEE);
        let mut pairs: Vec<(u64, DataValue)> = Vec::new();
        for _ in 0..5000 {
            let p = GeoPoint::admit(rng.lat(), rng.lon()).unwrap();
            let (qx, qy) = p.quantize();
            // Round-trip: quantize → interleave → de-interleave recovers the cell.
            let code = morton_encode(qx, qy);
            assert_eq!(morton_decode(code), (qx, qy), "morton codec round-trips");
            pairs.push((code, p.curve_key()));
        }
        // Ordering property: memcmp order of the Bytes key == u64 curve order.
        // This is THE law — byte order equals curve order.
        let mut encoded: Vec<(Vec<u8>, u64)> = pairs
            .iter()
            .map(|(code, key)| {
                let mut buf = Vec::new();
                crate::data::value::append_canonical(&mut buf, key);
                (buf, *code)
            })
            .collect();
        let mut by_bytes = encoded.clone();
        by_bytes.sort();
        encoded.sort_by_key(|(_, code)| *code);
        assert_eq!(
            by_bytes.iter().map(|(_, c)| *c).collect::<Vec<_>>(),
            encoded.iter().map(|(_, c)| *c).collect::<Vec<_>>(),
            "memcmp byte order must equal Morton curve order"
        );
    }

    /// ATTACK R5 (hostile-review F1, killer adopted verbatim): pin the
    /// quantization ROUNDING MODE. `floor -> round` in `quantize` is
    /// monotone and self-consistent, so every behavioral test stays green —
    /// but it is an on-disk format drift (an index written under floor,
    /// read under round, silently returns wrong rows). These fixtures have
    /// scaled fractional parts >= 0.5 on at least one axis each (lat for
    /// SF, lon for London, both for Sydney), so any rounding-mode drift
    /// moves the cell and fails loudly.
    #[test]
    fn rev_pinned_quantization_rounding_mode() {
        let cases: [(f64, f64, (u32, u32), u64); 3] = [
            (
                37.7749,
                -122.4194,
                (0xB5B9_6BDE, 0x28F2_3A74),
                0x4D91_EF49_1ECD_7B74,
            ),
            (
                51.5074,
                -0.1278,
                (0xC941_45A4, 0x7FE8_BC16),
                0x7AEB_B881_9AB1_4638,
            ),
            (
                -33.8688,
                151.2093,
                (0x4FD4_BF09, 0xEB86_D021),
                0xB8DF_D138_E755_0843,
            ),
        ];
        for (lat, lon, cell, code) in cases {
            let p = GeoPoint::admit(lat, lon).unwrap();
            assert_eq!(p.quantize(), cell, "quantized cell for ({lat},{lon})");
            assert_eq!(p.curve_index(), code, "curve code for ({lat},{lon})");
        }
    }

    #[test]
    fn pinned_curve_codes() {
        // Format fixtures: literal Morton codes for fixed coordinates. If the
        // quantization precision, the interleave, or the endianness drifts,
        // these fail loudly — the encoding is an on-disk format.
        let cases = [
            // (lat, lon, expected u64 code)
            (LAT_MIN, LON_MIN, 0u64),     // origin corner → all-zero cell
            (LAT_MAX, LON_MAX, u64::MAX), // opposite corner → all-ones cell
        ];
        for (lat, lon, want) in cases {
            let p = GeoPoint::admit(lat, lon).unwrap();
            assert_eq!(p.curve_index(), want, "curve code for ({lat},{lon})");
        }
        // A mid-domain point, pinned exactly. lat=0 is the midpoint of
        // [-90,90], so q_lat = 0.5·2³² = 0x8000_0000 (the bucket boundary);
        // likewise q_lon. Both quantized coordinates have only bit 31 set, which
        // spreads to bits 62 (lat) and 63 (lon) → 0xC000…. Pinned so any drift
        // in the quantization or interleave is caught.
        let equator = GeoPoint::admit(0.0, 0.0).unwrap();
        let (qx, qy) = equator.quantize();
        assert_eq!(
            (qx, qy),
            (0x8000_0000, 0x8000_0000),
            "equator/prime-meridian cell"
        );
        assert_eq!(
            equator.curve_index(),
            0xC000_0000_0000_0000,
            "equator curve code"
        );
        // ASYMMETRIC fixtures — quantized lat ≠ quantized lon — pin WHICH axis
        // owns the even bits (lat → even, lon → odd). The symmetric fixtures
        // above cannot distinguish a coherent lat/lon axis swap (encode and
        // decode both swapped: internally round-trip-consistent, but a
        // different on-disk curve). These two can.
        let lat_only = GeoPoint::admit(0.0, LON_MIN).unwrap();
        assert_eq!(lat_only.quantize(), (0x8000_0000, 0), "(0,-180) cell");
        assert_eq!(
            lat_only.curve_index(),
            0x4000_0000_0000_0000,
            "lat bit 31 spreads to even bit 62; a swapped curve would put it at 63"
        );
        let lon_only = GeoPoint::admit(LAT_MIN, 0.0).unwrap();
        assert_eq!(lon_only.quantize(), (0, 0x8000_0000), "(-90,0) cell");
        assert_eq!(
            lon_only.curve_index(),
            0x8000_0000_0000_0000,
            "lon bit 31 spreads to odd bit 63; a swapped curve would put it at 62"
        );
    }

    #[test]
    fn negative_zero_encodes_as_zero() {
        // IEEE -0.0 == 0.0, but they are distinct bit patterns; the curve must
        // give them ONE cell and ONE key, or the same location could carry two
        // different index postings.
        let neg = GeoPoint::admit(-0.0, -0.0).unwrap();
        let pos = GeoPoint::admit(0.0, 0.0).unwrap();
        assert_eq!(neg.quantize(), pos.quantize(), "-0.0 and 0.0: same cell");
        assert_eq!(
            neg.curve_index(),
            pos.curve_index(),
            "-0.0 and 0.0: same curve code"
        );
        assert_eq!(
            neg.curve_key(),
            pos.curve_key(),
            "-0.0 and 0.0: byte-identical key"
        );
    }

    #[test]
    fn admit_rejects_nan_and_out_of_range() {
        // NaN is a TYPED refusal (NonFiniteCoord), never a silent sort: assert
        // the concrete error type, not merely is_err().
        let nan_err = GeoPoint::admit(f64::NAN, 0.0).unwrap_err();
        assert!(
            nan_err.downcast_ref::<NonFiniteCoord>().is_some(),
            "NaN lat must refuse typed as NonFiniteCoord, got: {nan_err}"
        );
        let nan_lon_err = GeoPoint::admit(0.0, f64::NAN).unwrap_err();
        assert!(nan_lon_err.downcast_ref::<NonFiniteCoord>().is_some());
        let inf_err = GeoPoint::admit(0.0, f64::INFINITY).unwrap_err();
        assert!(inf_err.downcast_ref::<NonFiniteCoord>().is_some());
        let range_err = GeoPoint::admit(90.1, 0.0).unwrap_err();
        assert!(range_err.downcast_ref::<GeoCoordOutOfRange>().is_some());
        let lon_range_err = GeoPoint::admit(0.0, -180.001).unwrap_err();
        assert!(lon_range_err.downcast_ref::<GeoCoordOutOfRange>().is_some());
        assert!(GeoPoint::admit(90.0, 180.0).is_ok());
        // A wrapping box is refused typed.
        let err = BoundingBox::admit(0.0, 170.0, 10.0, -170.0).unwrap_err();
        assert!(err.downcast_ref::<AntimeridianBoxRefused>().is_some());
    }

    // == Range query vs naive full scan ======================================

    #[test]
    fn range_matches_naive_fullscan() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut rng = Rng(0x5EED_0001);
        let points: Vec<(i64, f64, f64)> = (0..2000).map(|i| (i, rng.lat(), rng.lon())).collect();
        let f = setup(&db, &points);

        // Many random non-wrapping boxes, including tiny and huge.
        let mut qr = Rng(0xB0B0);
        for _ in 0..300 {
            let (a, b) = (qr.lat(), qr.lat());
            let (c, d) = (qr.lon(), qr.lon());
            let bbox = BoundingBox::admit(a.min(b), c.min(d), a.max(b), c.max(d)).unwrap();
            let got = range_ids(&db, &f, &bbox);
            let mut got_sorted = got.clone();
            got_sorted.sort_unstable();
            assert_eq!(got_sorted, naive_range(&points, &bbox), "box {bbox:?}");
            // Canonical order: the query already returns ascending (curve, id);
            // for single-key relations that is a deterministic total order.
            let rerun = range_ids(&db, &f, &bbox);
            assert_eq!(got, rerun, "range query is deterministic");
        }
    }

    #[test]
    fn points_on_cell_boundaries() {
        // Points exactly on quantization cell edges and box edges must not be
        // lost (no under-approximation). Use coordinates that land on bucket
        // boundaries.
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let points = vec![
            (1, 0.0, 0.0),
            (2, 45.0, 90.0),
            (3, -45.0, -90.0),
            (4, 90.0, 180.0),
            (5, -90.0, -180.0),
        ];
        let f = setup(&db, &points);
        // A box whose edges pass exactly through several points.
        let bbox = BoundingBox::admit(-45.0, -90.0, 45.0, 90.0).unwrap();
        let mut got = range_ids(&db, &f, &bbox);
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3], "inclusive edges keep boundary points");
    }

    /// A degenerate point-query box (min corner == max corner == a stored
    /// point's exact coordinates) collapses the decomposition to the single
    /// quantized cell — so the cell disjoint/contained test is decisive, with
    /// no budget coarsening to mask it. The sharpest boundary adversary: an
    /// off-by-one in the disjoint check (discarding a cell at the box's upper
    /// edge) drops the point here.
    #[test]
    fn point_query_returns_exact_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let points = vec![(1, 10.0, 20.0), (2, -33.3, 44.4), (3, 12.5, -60.25)];
        let f = setup(&db, &points);
        for (id, lat, lon) in &points {
            // The box IS the point: lat_lo == lat_hi == lat, likewise lon.
            let bbox = BoundingBox::admit(*lat, *lon, *lat, *lon).unwrap();
            assert_eq!(
                range_ids(&db, &f, &bbox),
                vec![*id],
                "point query for ({lat},{lon}) returns exactly its row"
            );
        }
    }

    #[test]
    fn duplicate_points_are_distinct_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        // Three rows at the identical coordinate.
        let points = vec![(10, 12.34, 56.78), (20, 12.34, 56.78), (30, 12.34, 56.78)];
        let f = setup(&db, &points);
        let bbox = BoundingBox::admit(12.0, 56.0, 13.0, 57.0).unwrap();
        let mut got = range_ids(&db, &f, &bbox);
        got.sort_unstable();
        assert_eq!(
            got,
            vec![10, 20, 30],
            "duplicate coordinates are distinct rows"
        );
    }

    #[test]
    fn empty_index_queries() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let f = setup(&db, &[]);
        let bbox = BoundingBox::admit(-10.0, -10.0, 10.0, 10.0).unwrap();
        assert!(range_ids(&db, &f, &bbox).is_empty());
        let q = GeoPoint::admit(0.0, 0.0).unwrap();
        assert!(
            knn_ids(&db, &f, &q, 5).is_empty(),
            "kNN on empty index terminates"
        );
    }

    // == k-NN vs exact haversine sort ========================================

    #[test]
    fn knn_matches_exact_haversine_sort() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut rng = Rng(0x2A2A);
        let points: Vec<(i64, f64, f64)> = (0..1500).map(|i| (i, rng.lat(), rng.lon())).collect();
        let f = setup(&db, &points);

        let mut qr = Rng(0x9119);
        for _ in 0..120 {
            let q = GeoPoint::admit(qr.lat(), qr.lon()).unwrap();
            for k in [1usize, 3, 10] {
                let got: Vec<i64> = knn_ids(&db, &f, &q, k)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                assert_eq!(got, naive_knn(&points, &q, k), "kNN q={q:?} k={k}");
            }
        }
    }

    #[test]
    fn knn_distance_is_exact_and_ascending() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let points = vec![(1, 0.0, 0.0), (2, 0.0, 1.0), (3, 0.0, 2.0), (4, 1.0, 1.0)];
        let f = setup(&db, &points);
        let q = GeoPoint::admit(0.0, 0.0).unwrap();
        let got = knn_ids(&db, &f, &q, 4);
        // Distances ascending; the first is the query point itself (distance 0).
        assert_eq!(got[0].0, 1);
        assert!(got[0].1.abs() < 1e-12, "self distance is 0");
        for w in got.windows(2) {
            assert!(w[0].1 <= w[1].1, "distances ascend");
        }
        // Exact distance to point 2 (0,0)->(0,1 deg) is 1 degree in radians.
        assert!(
            (got[1].1 - 1.0 * DEG_TO_RAD).abs() < 1e-9,
            "exact haversine"
        );
    }

    #[test]
    fn knn_wraps_antimeridian_and_pole() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        // A neighbour just across the antimeridian, and one over the pole.
        let points = vec![
            (1, 0.0, 179.9),   // query is at lon 179.99
            (2, 0.0, -179.9),  // just across ±180 — geographically ~0.2° away
            (3, 0.0, 100.0),   // far
            (4, 89.9, 10.0),   // near north pole
            (5, 89.9, -170.0), // "over" the pole from #4
        ];
        let f = setup(&db, &points);

        let q1 = GeoPoint::admit(0.0, 179.99).unwrap();
        let near = knn_ids(&db, &f, &q1, 2)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        assert_eq!(
            near,
            naive_knn(&points, &q1, 2),
            "antimeridian neighbour found"
        );
        assert!(
            near.contains(&2),
            "the point across ±180 is among the 2 nearest"
        );

        let q2 = GeoPoint::admit(89.95, 10.0).unwrap();
        let polar = knn_ids(&db, &f, &q2, 2)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        assert_eq!(
            polar,
            naive_knn(&points, &q2, 2),
            "over-the-pole neighbour found"
        );
    }

    // == Determinism =========================================================

    #[test]
    fn determinism_run_twice() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut rng = Rng(0xD37);
        let points: Vec<(i64, f64, f64)> = (0..800).map(|i| (i, rng.lat(), rng.lon())).collect();
        let f = setup(&db, &points);
        let q = GeoPoint::admit(12.0, 34.0).unwrap();
        let a = knn_ids(&db, &f, &q, 10);
        let b = knn_ids(&db, &f, &q, 10);
        assert_eq!(a, b, "kNN identical across runs");
        let bbox = BoundingBox::admit(-30.0, -30.0, 30.0, 30.0).unwrap();
        assert_eq!(
            range_ids(&db, &f, &bbox),
            range_ids(&db, &f, &bbox),
            "range identical"
        );
    }

    // == Corruption is typed, never a panic ==================================

    #[test]
    fn corrupt_posting_is_typed_error_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let f = setup(&db, &[(1, 10.0, 20.0)]);

        // Overwrite the index row's value with garbage msgpack.
        let mut tx = db.write_tx().unwrap();
        let kvs: Vec<(fjall::Slice, fjall::Slice)> = {
            let lower = crate::data::value::encode_key_with_suffix(f.idx.id, &[], &[]);
            let upper = (f.idx.id.raw() + 1).to_be_bytes();
            tx.range_scan(lower.as_bytes(), &upper)
                .collect::<Result<Vec<_>>>()
                .unwrap()
        };
        assert!(!kvs.is_empty());
        for (k, _) in &kvs {
            let mut garbage = vec![0u8; 8];
            garbage.push(0xc1); // reserved, never-valid msgpack byte
            tx.put(k, &garbage).unwrap();
        }
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let bbox = BoundingBox::admit(9.0, 19.0, 11.0, 21.0).unwrap();
        let err = spatial_range_query(&rtx, &f.base, &f.idx, &bbox)
            .expect_err("corrupt posting must error, not panic");
        assert!(
            err.downcast_ref::<crate::engines::IndexRowCorrupt>()
                .is_some(),
            "corrupt index bytes must surface as the typed IndexRowCorrupt: {err:?}"
        );
    }

    #[test]
    fn del_withdraws_posting() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let f = setup(&db, &[(1, 10.0, 20.0), (2, 10.1, 20.1)]);
        let bbox = BoundingBox::admit(9.0, 19.0, 11.0, 21.0).unwrap();
        assert_eq!(range_ids(&db, &f, &bbox).len(), 2);

        let mut tx = db.write_tx().unwrap();
        let row = vec![
            DataValue::from(1i64),
            DataValue::from(10.0),
            DataValue::from(20.0),
        ];
        spatial_del(&mut tx, &row, &f.manifest, &f.base, &f.idx).unwrap();
        tx.commit().unwrap();

        let got = range_ids(&db, &f, &bbox);
        assert_eq!(got, vec![2], "only the surviving point remains");
    }

    // == Manifest wire form round-trips ======================================

    #[test]
    fn manifest_roundtrips() {
        let m = manifest();
        let bytes = rmp_serde::to_vec_named(&m).unwrap();
        let back: SpatialIndexManifest = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(m, back, "manifest survives its wire form");
    }
}
