/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Geometry`: a 2D point whose domain is the **discrete fixed-point cell
//! grid** (`u32 × u32`), not floating coordinates.
//!
//! ## Identity and order (stated before bytes)
//!
//! - **Identity** is the cell pair `(lat, lon)` — two geometries are equal
//!   iff both cells are equal. There is no float, so there is no
//!   `floor()`-quantize step and no cell-boundary rounding ambiguity: a
//!   cell is exactly one cell.
//! - **Order** is the **Hilbert** space-filling-curve index of those cells
//!   (not Morton/Z-order). Hilbert is the ruled curve (#202): tighter
//!   provable locality than Z-order — a `2^k × 2^k` aligned block maps to
//!   one contiguous index range. Storage order IS curve order.
//!   Expression-level spatial predicates (range / k-NN) become ordinary
//!   memcmp range scans over the canonical key.
//!
//! ## Canonical payload (format v1)
//!
//! Eight big-endian bytes of the 64-bit Hilbert index. memcmp of the
//! payload equals `u64` curve order; the tag byte prefixes the kind in
//! cross-type order. Decode is total over any 8-byte body (every `u64`
//! is a lawful curve key and maps to a unique cell pair).
//!
//! [OPEN] cross-story dep — #288 under epic #353: engine-side
//! `project/spatial` must retire floor()-quantize / Z-order in favor of this
//! seat's Hilbert fixed-point encoding. This value-plane kind is complete;
//! the engine projection retirement is not inventable here.

/// One axis cell on the 32-bit fixed-point grid. Private field; mint only
/// through [`Geometry`] constructors that already hold the proof.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct CellCoord(u32);

const _: () = assert!(std::mem::size_of::<CellCoord>() == std::mem::size_of::<u32>());
const _: () = assert!(std::mem::align_of::<CellCoord>() == std::mem::align_of::<u32>());

impl CellCoord {
    /// Brand a raw cell index. Every `u32` is a lawful cell.
    pub fn new(raw: u32) -> CellCoord {
        CellCoord(raw)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl PartialOrd for CellCoord {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CellCoord {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

/// A geometry point: proven `(lat, lon)` cells on the fixed-point grid.
///
/// Order law: Hilbert curve of the cells — equal to the canonical
/// payload's byte order. Never holds `f64`; never floors.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Geometry {
    lat: CellCoord,
    lon: CellCoord,
}

impl Geometry {
    /// Admit door: brand both cell indices. Total — every `u32×u32` pair
    /// is a lawful geometry.
    pub fn from_cells(lat: u32, lon: u32) -> Geometry {
        Geometry {
            lat: CellCoord::new(lat),
            lon: CellCoord::new(lon),
        }
    }

    /// Post-decode mint: the curve key already proved both cells.
    pub(crate) fn from_curve_key(code: u64) -> Geometry {
        let (lat, lon) = hilbert_decode(code);
        Geometry::from_cells(lat, lon)
    }

    pub fn lat(self) -> CellCoord {
        self.lat
    }

    pub fn lon(self) -> CellCoord {
        self.lon
    }

    /// The 64-bit Hilbert curve index — the semantic order key.
    pub fn curve_index(self) -> u64 {
        hilbert_encode(self.lat.get(), self.lon.get())
    }

    /// The eight big-endian curve-key bytes written as the canonical
    /// payload (memcmp of these equals [`Self::curve_index`] order).
    pub fn curve_key_bytes(self) -> [u8; 8] {
        self.curve_index().to_be_bytes()
    }
}

impl PartialOrd for Geometry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Geometry {
    /// Curve order: Hilbert index comparison — the same total order the
    /// canonical eight payload bytes embed.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.curve_index().cmp(&other.curve_index())
    }
}

/// Hilbert rotate/flip for a square of side `n` (power of two).
/// `n = None` means side `2^32` (encode's full grid): `n-1 - v` is `!v`.
fn hilbert_rot(n: Option<u32>, x: &mut u32, y: &mut u32, rx: u32, ry: u32) {
    if ry == 0 {
        if rx == 1 {
            match n {
                None => {
                    *x = !*x;
                    *y = !*y;
                }
                Some(side) => {
                    // INVARIANT(HilbertRotReflect): side is a power of two ≥ 2;
                    // wrap implements (side-1)-x reflection on the u32 grid.
                    *x = side.wrapping_sub(1).wrapping_sub(*x);
                    *y = side.wrapping_sub(1).wrapping_sub(*y);
                }
            }
        }
        core::mem::swap(x, y);
    }
}

/// Map `(lat, lon)` cells to the 64-bit Hilbert index (Wikipedia xy2d,
/// 32-bit axes → full `u64` range). `lat` is the curve's x, `lon` its y.
fn hilbert_encode(mut lat: u32, mut lon: u32) -> u64 {
    let mut d = 0u64;
    let mut s = 1u32 << 31;
    loop {
        let rx = u32::from((lat & s) != 0);
        let ry = u32::from((lon & s) != 0);
        d += u64::from(s) * u64::from(s) * u64::from((3 * rx) ^ ry);
        // Full-grid rotate: side 2^32.
        hilbert_rot(None, &mut lat, &mut lon, rx, ry);
        if s == 1 {
            break;
        }
        s >>= 1;
    }
    d
}

/// Inverse of [`hilbert_encode`]: Hilbert index → `(lat, lon)` cells
/// (Wikipedia d2xy).
fn hilbert_decode(mut d: u64) -> (u32, u32) {
    let mut lat = 0u32;
    let mut lon = 0u32;
    let mut s = 1u32;
    loop {
        let rx = match (d >> 1) & 1 {
            0 => 0u32,
            _ => 1u32,
        };
        let ry = match (d ^ u64::from(rx)) & 1 {
            0 => 0u32,
            _ => 1u32,
        };
        hilbert_rot(Some(s), &mut lat, &mut lon, rx, ry);
        // INVARIANT(HilbertDecodeStep): s is a power of two on the 32-bit
        // Hilbert walk; wrap places the bit into lat/lon without overflow checks.
        lat = lat.wrapping_add(s.wrapping_mul(rx));
        lon = lon.wrapping_add(s.wrapping_mul(ry));
        d >>= 2;
        if s == 1 << 31 {
            break;
        }
        s <<= 1;
    }
    (lat, lon)
}

#[cfg(test)]
mod tests {
    use miette::{IntoDiagnostic, Result};

    use super::*;
    use crate::value::DataValue;
    use crate::value::canonical::{decode, encode_owned};
    use crate::value::tag::Tag;
    use std::cmp::Ordering;

    /// Round-trip through the canonical codec: cells survive encode/decode.
    #[test]
    fn encode_decode_round_trip() -> Result<()> {
        let cases = [
            Geometry::from_cells(0, 0),
            Geometry::from_cells(1, 0),
            Geometry::from_cells(0, 1),
            Geometry::from_cells(u32::MAX, u32::MAX),
            Geometry::from_cells(0xAAAA_AAAA, 0x5555_5555),
            Geometry::from_cells(1_000_000, 2_000_000),
        ];
        for g in cases {
            let v = DataValue::Geometry(g);
            let enc = encode_owned(&v);
            assert_eq!(enc.as_bytes()[0], Tag::Geometry.byte());
            assert_eq!(enc.len(), 9, "tag + 8 curve bytes");
            let back = decode(enc.as_bytes()).into_diagnostic()?;
            assert_eq!(back, v, "round-trip changed {g:?}");
            let DataValue::Geometry(g2) = back else {
                panic!("decoded non-geometry");
            };
            assert_eq!(g2.lat().get(), g.lat().get());
            assert_eq!(g2.lon().get(), g.lon().get());
            assert_eq!(g2.curve_index(), g.curve_index());
        }
        Ok(())
    }

    /// THE one-law property for geometry: Ord == Hilbert-index order ==
    /// canonical byte order (memcmp key = curve key).
    #[test]
    fn byte_order_equals_hilbert_order() {
        let mut corpus = vec![
            Geometry::from_cells(0, 0),
            Geometry::from_cells(1, 0),
            Geometry::from_cells(0, 1),
            Geometry::from_cells(1, 1),
            Geometry::from_cells(2, 0),
            Geometry::from_cells(0, 2),
            Geometry::from_cells(u32::MAX, 0),
            Geometry::from_cells(0, u32::MAX),
            Geometry::from_cells(u32::MAX, u32::MAX),
        ];
        // Deterministic fill of the mid grid.
        let mut s = 0xC0FFEE_u64;
        for _ in 0..40 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            // INVARIANT(HilbertCorpusMix): xorshift mix for a deterministic
            // test corpus; wrap is the intended u32 scatter, not a size proof.
            let lat = match u32::try_from(s & 0xFFFF_FFFF) {
                Ok(v) => v,
                Err(_) => 0,
            }
            .wrapping_mul(0x9E37_79B9);
            let lon = match u32::try_from(s >> 32) {
                Ok(v) => v,
                Err(_) => 0,
            }
            // INVARIANT(HilbertCorpusMix): xorshift mix for a deterministic
            // test corpus; wrap is the intended u32 scatter, not a size proof.
            .wrapping_mul(0x85EB_CA6B);
            corpus.push(Geometry::from_cells(lat, lon));
        }
        let values: Vec<DataValue> = corpus.iter().copied().map(DataValue::Geometry).collect();
        let encoded: Vec<_> = values.iter().map(encode_owned).collect();
        for i in 0..values.len() {
            for j in 0..values.len() {
                let curve = corpus[i].curve_index().cmp(&corpus[j].curve_index());
                let structural = values[i].cmp(&values[j]);
                let byte = encoded[i].as_bytes().cmp(encoded[j].as_bytes());
                let geo_ord = corpus[i].cmp(&corpus[j]);
                assert_eq!(
                    curve, geo_ord,
                    "Geometry::Ord != Hilbert: {:?} vs {:?}",
                    corpus[i], corpus[j]
                );
                assert_eq!(
                    structural, curve,
                    "DataValue::Ord != Hilbert: {:?} vs {:?}",
                    corpus[i], corpus[j]
                );
                assert_eq!(
                    byte, curve,
                    "canonical bytes != Hilbert: {:?} vs {:?}",
                    corpus[i], corpus[j]
                );
            }
        }
    }

    /// Locality: Hilbert's tighter bound — an aligned 2×2 block occupies
    /// four contiguous indices; unit-step neighbors stay nearer on the
    /// key line than a distant cell.
    #[test]
    fn nearby_cells_yield_nearby_keys() {
        // 32-bit Hilbert: origin 2×2 is one contiguous index range.
        let block = [
            Geometry::from_cells(0, 0),
            Geometry::from_cells(1, 0),
            Geometry::from_cells(1, 1),
            Geometry::from_cells(0, 1),
        ];
        let mut idxs: Vec<u64> = block.iter().map(|g| g.curve_index()).collect();
        idxs.sort_unstable();
        assert_eq!(
            idxs,
            vec![0, 1, 2, 3],
            "2×2 origin block must be contiguous"
        );

        let origin = Geometry::from_cells(1000, 2000);
        let east = Geometry::from_cells(1000, 2001);
        let north = Geometry::from_cells(1001, 2000);
        let far = Geometry::from_cells(1000 + (1 << 20), 2000 + (1 << 20));

        let o = origin.curve_index();
        let e = east.curve_index();
        let n = north.curve_index();
        let f = far.curve_index();

        let dist = |a: u64, b: u64| a.abs_diff(b);
        assert!(dist(o, e) < dist(o, f), "lon+1 nearer than far cell");
        assert!(dist(o, n) < dist(o, f), "lat+1 nearer than far cell");

        // memcmp agrees with Hilbert ranking.
        let enc = |g: Geometry| encode_owned(&DataValue::Geometry(g));
        assert!(enc(origin).as_bytes().cmp(enc(east).as_bytes()) == o.cmp(&e));
        assert!(enc(origin).as_bytes().cmp(enc(far).as_bytes()) == o.cmp(&f));
    }

    /// Rounding-free at cell boundaries: adjacent integer cells are
    /// distinct identities with distinct Hilbert keys — no float floor
    /// can collapse them. Exact cell recovery through the codec.
    #[test]
    fn rounding_free_at_cell_boundaries() -> Result<()> {
        let lon = 42u32;
        let mut prev = Geometry::from_cells(0, lon);
        for lat in 1u32..64 {
            let cur = Geometry::from_cells(lat, lon);
            assert_ne!(prev, cur, "adjacent cells must not share identity");
            assert_ne!(
                prev.curve_index(),
                cur.curve_index(),
                "adjacent cells must not share a Hilbert key"
            );
            // Exact cell recovery — no boundary rounding.
            assert_eq!(cur.lat().get(), lat);
            assert_eq!(cur.lon().get(), lon);
            let enc = encode_owned(&DataValue::Geometry(cur));
            let back = decode(enc.as_bytes()).into_diagnostic()?;
            assert_eq!(back, DataValue::Geometry(cur));
            // Ord mirrors the curve key (not axis order — Hilbert snakes).
            assert_eq!(prev.cmp(&cur), prev.curve_index().cmp(&cur.curve_index()));
            prev = cur;
        }
        let top = Geometry::from_cells(u32::MAX, lon);
        let below = Geometry::from_cells(u32::MAX - 1, lon);
        assert_ne!(below, top);
        assert_ne!(below.curve_index(), top.curve_index());
        assert_eq!(below.cmp(&top), below.curve_index().cmp(&top.curve_index()));
        // PartialOrd is total (no hole).
        assert_eq!(below.partial_cmp(&top), Some(below.cmp(&top)));
        assert_ne!(Ordering::Equal, below.cmp(&top));
        Ok(())
    }

    /// Hilbert codec is a bijection on the cell pair.
    #[test]
    fn hilbert_codec_is_bijective_on_cells() {
        let samples = [
            (0u32, 0u32),
            (1, 0),
            (0, 1),
            (1, 1),
            (2, 3),
            (0xFFFF_FFFF, 0),
            (0, 0xFFFF_FFFF),
            (0xFFFF_FFFF, 0xFFFF_FFFF),
            (0x1234_5678, 0x9ABC_DEF0),
            (0xAAAA_AAAA, 0x5555_5555),
        ];
        for &(lat, lon) in &samples {
            let code = hilbert_encode(lat, lon);
            assert_eq!(hilbert_decode(code), (lat, lon));
            let g = Geometry::from_curve_key(code);
            assert_eq!(g.lat().get(), lat);
            assert_eq!(g.lon().get(), lon);
            assert_eq!(g.curve_index(), code);
        }
        // Decode → encode is also identity on a sample of keys.
        let keys = [
            0u64,
            1,
            2,
            3,
            4,
            u64::MAX,
            0x0123_4567_89AB_CDEF,
            0xFFFF_0000_FFFF_0000,
        ];
        for &k in &keys {
            let (lat, lon) = hilbert_decode(k);
            assert_eq!(hilbert_encode(lat, lon), k);
        }
    }

    /// Pinned 32-bit Hilbert orientation at the origin 2×2 (lat=x, lon=y).
    /// Odd empty high levels reflect the classic order-1 U; values are
    /// permanent once format v1 ships.
    #[test]
    fn hilbert_origin_block_orientation_pinned() {
        assert_eq!(hilbert_encode(0, 0), 0);
        assert_eq!(hilbert_encode(1, 0), 1);
        assert_eq!(hilbert_encode(1, 1), 2);
        assert_eq!(hilbert_encode(0, 1), 3);
    }
}
