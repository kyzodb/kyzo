/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original (MPL-2.0).
 */

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ndarray::Array1;
use std::cmp::{Ordering, Reverse};
use std::collections::BTreeSet;
use std::fmt::{Debug, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::ops::Deref;

use ordered_float::OrderedFloat;
use regex::Regex;
use serde::de::{SeqAccess, Visitor};
use serde::ser::SerializeTuple;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
pub(crate) use serde_json::Value as JsonValue;
use sha2::digest::FixedOutput;
use sha2::{Digest, Sha256};
use smartstring::{LazyCompact, SmartString};
use uuid::Uuid;

/// UUID value in the database
#[derive(Clone, Hash, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
pub struct UuidWrapper(pub Uuid);

impl PartialOrd<Self> for UuidWrapper {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for UuidWrapper {
    fn cmp(&self, other: &Self) -> Ordering {
        let (s_l, s_m, s_h, s_rest) = self.0.as_fields();
        let (o_l, o_m, o_h, o_rest) = other.0.as_fields();
        s_h.cmp(&o_h)
            .then_with(|| s_m.cmp(&o_m))
            .then_with(|| s_l.cmp(&o_l))
            .then_with(|| s_rest.cmp(o_rest))
    }
}

/// A Regex in the database. Used internally in functions.
#[derive(Clone)]
pub struct RegexWrapper(pub Regex);

impl Hash for RegexWrapper {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.as_str().hash(state)
    }
}

impl Serialize for RegexWrapper {
    fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Regexes are query-internal and must never be persisted; an engine
        // bug that tries becomes an error, not a process abort.
        Err(serde::ser::Error::custom(
            "regex values cannot be serialized",
        ))
    }
}

impl<'de> Deserialize<'de> for RegexWrapper {
    fn deserialize<D>(_deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Corrupt or hostile stored bytes naming the Regex variant are a
        // decode error, never a panic (value-side Law 3).
        Err(serde::de::Error::custom(
            "regex values cannot be deserialized",
        ))
    }
}

impl PartialEq for RegexWrapper {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_str() == other.0.as_str()
    }
}

impl Eq for RegexWrapper {}

impl Ord for RegexWrapper {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.as_str().cmp(other.0.as_str())
    }
}

impl PartialOrd for RegexWrapper {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The temporal coordinate of a validity claim: microseconds since epoch,
/// wrapped in `Reverse` so newer moments sort first among a fact's versions
/// — which is what lets an as-of seek land on the newest eligible version.
///
/// The tuple field is private. `i64::MAX` is a legitimate `ValidityTs` —
/// it is [`MAX_VALIDITY_TS`], the open-end sentinel every derived interval
/// and open-end read relies on — so a blanket ban on that value would be
/// wrong; only a USER-ASSERTED write validity may never land there (issue
/// #62's ruling). Two constructors say which case a caller is in:
/// [`Self::from_raw`] for any read-side/internal construction (the sentinel
/// const, decode-from-stored, system-stamp resolution, open-end reads, an
/// embedder's own `AsOf` coordinate — every `i64` is fine), and
/// [`Self::for_assertion`] for a user-write instant, the one path that
/// refuses `i64::MAX`. A raw `ValidityTs(Reverse(x))` literal is no longer
/// buildable anywhere outside this module — every external and internal
/// caller alike goes through one of the two named constructors above, so a
/// future user-write path that skips `for_assertion` is a compile error,
/// not a bug waiting for a test to catch it.
#[derive(
    Copy,
    Clone,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    serde_derive::Deserialize,
    serde_derive::Serialize,
    Hash,
    Debug,
)]
pub struct ValidityTs(Reverse<i64>);

/// `valid = i64::MAX` (`'END'`, or the literal microsecond itself) is the
/// reserved terminal tick every open-end sentinel depends on being
/// unwritable (issue #62's ruling: the temporal oracle and the `Interval`
/// `DataValue` both read "no stored event governs past here" as "still
/// open" — a fact actually stored AT that instant would collide with that
/// reading and derive as a zero-width interval). `@ 'END'` stays legal on
/// the READ side (`data_value_to_vld_spec`'s "as of the end of time");
/// this refusal is write-only, raised by [`ValidityTs::for_assertion`] —
/// the single diagnostic shared by every user-write call site (the parse-time
/// constant `@` coordinate and the per-row `@` coordinate alike), so the two
/// near-duplicate error types this codebase used to carry (one per call
/// site) collapse to the one constructor that actually owns the rule.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error(
    "the valid instant `i64::MAX` (`'END'`) is reserved as the open-end sentinel and cannot be written to; name a concrete instant, or omit `@` (every row lands at the transaction's own stamp)"
)]
#[diagnostic(code(data::write_validity_at_terminal_instant))]
pub(crate) struct WriteValidityAtTerminalInstant(#[label] pub(crate) crate::data::span::SourceSpan);

impl ValidityTs {
    /// Construction from an already-known raw microsecond coordinate: the
    /// sentinel const, decode-from-stored keys, system-stamp resolution,
    /// open-end reads, and any embedder building a read-side `AsOf`
    /// coordinate — every context that legitimately needs a `ValidityTs`
    /// carrying `i64::MAX` (or any other `i64`). Public because `ValidityTs`
    /// and `AsOf` are themselves part of this crate's public read API
    /// (`lib.rs`'s re-export list); NOT for a user-asserted WRITE validity —
    /// that path is [`Self::for_assertion`], the only constructor that
    /// refuses the sentinel, and it stays `pub(crate)` because the write
    /// coordinate type (`data::program::WriteValidity`) that consumes it is
    /// itself `pub(crate)` — a script is the only way to assert a write
    /// validity, so there is no public constructor to seal against.
    pub const fn from_raw(micros: i64) -> Self {
        ValidityTs(Reverse(micros))
    }

    /// The raw microsecond coordinate — the read counterpart to
    /// [`Self::from_raw`], for consumers (key/value encoding, arithmetic,
    /// formatting, diagnostics, and any embedder reading a resolved `AsOf`
    /// coordinate back out) that need it back out.
    pub const fn raw(&self) -> i64 {
        self.0.0
    }

    /// Smart constructor for a USER-ASSERTED write validity: refuses
    /// `i64::MAX`, the reserved open-end sentinel, as a typed diagnostic
    /// rather than a value a caller must remember to equality-check after
    /// the fact. Parse, don't validate — every `ValidityTs` this returns
    /// carries proof it isn't the sentinel.
    pub(crate) fn for_assertion(
        instant: i64,
        span: crate::data::span::SourceSpan,
    ) -> miette::Result<Self> {
        if instant == i64::MAX {
            miette::bail!(WriteValidityAtTerminalInstant(span));
        }
        Ok(ValidityTs(Reverse(instant)))
    }
}

/// A time-stamped existence claim — the last slot of a versioned fact's key.
/// Assertion states the fact exists from this moment; retraction is a
/// first-class assertion of absence, not a deletion.
#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    serde_derive::Deserialize,
    serde_derive::Serialize,
    Hash,
)]
pub struct Validity {
    /// Timestamp, sorted descendingly
    pub timestamp: ValidityTs,
    /// Whether this validity is an assertion, sorted descendingly
    pub is_assert: Reverse<bool>,
}

impl From<(i64, bool)> for Validity {
    fn from(value: (i64, bool)) -> Self {
        Self {
            timestamp: ValidityTs::from_raw(value.0),
            is_assert: Reverse(value.1),
        }
    }
}

/// A storable bitemporal key-tail slot: the write-time proof that a
/// fact's two time slots (valid instant, system version —
/// `EncodedKey::BITEMPORAL_TAIL_LEN`, `data/tuple.rs`) are always
/// well-formed. [`Self::new`] is the only constructor and it pins
/// `is_assert` to `true` unconditionally, so a stored slot carrying a
/// retract flag is unrepresentable at the point of writing, not merely
/// rejected on read.
///
/// This is the enforcement-ladder promotion of the convention every fact
/// key composer used to hand-roll (`runtime/relation.rs`'s
/// `encode_bitemporal_key_for_store` kept a local `slot()` closure doing
/// exactly this before this type existed). It also changes what
/// `data::bitemporal::check_key_for_bitemporal`'s stored-flag check
/// (`data/bitemporal.rs`, the `!valid.is_assert.0 || !sys.is_assert.0`
/// refusal) MEANS: with every write path routed through
/// [`Self::new`], that check no longer guards a runtime invariant the
/// write path could violate — it demotes to a disk-corruption guard,
/// catching only bytes this process never wrote (a corrupted store, a
/// crafted dump). The check itself is unchanged; its job description is.
///
/// A general (unpinned) [`Validity`] remains the right type for read-time
/// coordinates that are not stored key material — `check_key_for_bitemporal`'s
/// own splice bounds, for instance, synthesize scan-seek bytes that are
/// never a persisted fact's own slot, and both flags there are read, not
/// written, by the scan.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct StoredValiditySlot(Validity);

impl StoredValiditySlot {
    /// The only constructor: `is_assert` is `true` by construction, never
    /// a parameter.
    pub(crate) fn new(ts: ValidityTs) -> Self {
        StoredValiditySlot(Validity {
            timestamp: ts,
            is_assert: Reverse(true),
        })
    }

    /// The slot's written form: a key-tail data value, ready for
    /// `MemCmpEncoder::encode_datavalue`.
    pub(crate) fn as_datavalue(self) -> DataValue {
        DataValue::Validity(self.0)
    }
}

/// The current time as a validity timestamp (microseconds since epoch).
///
/// A host clock before 1970 is an error, not an abort — the same policy the
/// builtin `now()` applies (one clock, one policy).
pub fn current_validity() -> miette::Result<ValidityTs> {
    #[cfg(target_arch = "wasm32")]
    compile_error!(
        "the wasm validity clock (js Date) lands with the wasm binding; \
         it must be implemented, not stubbed, when that target is built"
    );
    let now = std::time::SystemTime::now();
    let ts_micros = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| miette::miette!("host clock reports a time before 1970"))?
        .as_micros() as i64;
    Ok(ValidityTs::from_raw(ts_micros))
}

/// A bitemporal read coordinate: WHERE in recorded history a query
/// stands. `sys` picks the record's state (what was known), `valid`
/// picks the world instant asked about (what held). One named pair
/// instead of two adjacent `ValidityTs` arguments, so a sys/valid swap
/// is a type the compiler never sees, not a bug a test must catch.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct AsOf {
    /// The system-time coordinate: resolve each fact by the newest
    /// system version at or before this instant.
    pub sys: ValidityTs,
    /// The valid-time coordinate: among believed claims, the newest
    /// valid instant at or before this one governs.
    pub valid: ValidityTs,
}

impl AsOf {
    /// The record's current belief about the world at `valid` — the
    /// coordinate every non-historical read uses.
    pub const fn current(valid: ValidityTs) -> Self {
        AsOf {
            sys: MAX_VALIDITY_TS,
            valid,
        }
    }

    /// What the record said at system time `sys` about valid time
    /// `valid` — the general two-coordinate historical read.
    /// [`Self::current`] is exactly the special case `sys ==
    /// MAX_VALIDITY_TS`; this is the smart-constructor promotion of the
    /// raw `AsOf { sys, valid }` struct literal every historical read
    /// site used to hand-roll.
    pub const fn at(sys: ValidityTs, valid: ValidityTs) -> Self {
        AsOf { sys, valid }
    }
}

pub(crate) const MAX_VALIDITY_TS: ValidityTs = ValidityTs::from_raw(i64::MAX);
pub(crate) const TERMINAL_VALIDITY: Validity = Validity {
    timestamp: ValidityTs::from_raw(i64::MIN),
    is_assert: Reverse(false),
};

/// A first-class half-open interval `[start, end)` over microsecond ticks —
/// a read-time VALUE (story #62's derived-interval algebra), not the on-disk
/// `Validity` key-tail. The two types are deliberately kept apart: `Validity`
/// is `Reverse`-flipped so newer versions seek first among a fact's key
/// prefix; `Interval` orders ascending `(start, end)`, the plain intuitive
/// order for a value that gets compared, sorted, and put in a `List` like
/// any other `DataValue`.
///
/// Design decisions (ratified in issue #62's consolidated design ruling):
///
/// - **Closed-open, `end` exclusive.** An instant is `[v, v+1)`.
/// - **`end <= start` is refused** by [`Interval::new`], never a panic.
///   Empty (`end == start`) intervals are refused along with inverted ones:
///   derived interval output is defined as maximal constant runs, which are
///   never empty by construction, and no prior system in the survey
///   (SQL:2011, Postgres range types, XTDB, Datomic) permits a stored
///   zero-width fact. Refusing here keeps "an `Interval` value exists" mean
///   "some instant is in it," an invariant of the type instead of a
///   convention every consumer must re-check.
/// - **Open-ended intervals use `i64::MAX` as a plain `end` value, not a
///   distinguished sentinel representation.** This matches `MAX_VALIDITY_TS`
///   (`ValidityTs(Reverse(i64::MAX))`) and `data_value_to_vld_spec`'s `"END"`
///   spelling (`functions.rs`), both of which already treat `i64::MAX` as
///   "no upper bound" without a wrapper variant or an exclusive/inclusive
///   special case. Giving `Interval` a second, distinct representation of
///   the same idea would be a new special case where the codebase already
///   has one uniform spelling; every predicate below is therefore ordinary
///   integer comparison, with no `if end == MAX` branch anywhere. The corner
///   this convention would otherwise leave open — the single instant
///   `i64::MAX` itself technically excluded by a `[.., i64::MAX)` interval —
///   is closed by ruling, not by assumption: `@ 'END'` writes at that exact
///   valid instant are REFUSED (the terminal tick is reserved; issue #62
///   comment 4882951801), enforced on the write path outside this file. The
///   open-end convention here is sound *because* that instant is
///   unwritable, not because it was assumed unreachable.
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Hash, serde_derive::Serialize)]
pub struct Interval {
    /// Inclusive lower bound, in microseconds since the epoch.
    start: i64,
    /// Exclusive upper bound, in microseconds since the epoch.
    end: i64,
}

/// Deserialize is hand-written, NOT derived: a derived impl lives in this
/// same module, so it would see `start`/`end` as ordinary fields and build
/// an `Interval` by direct field assignment — bypassing [`Interval::new`]
/// and its `end > start` invariant entirely. Every wire-format decode
/// (`fact_payload.rs`'s msgpack `FIELD_OTHER` island, `tuple.rs`'s legacy
/// `rmp_serde` path) recurses into `DataValue`'s derived `Deserialize`,
/// which bottoms out here — so this is the one seam that must re-validate,
/// exactly like the memcmp key path's `Interval::new` call in
/// `memcmp.rs::decode_from_key`. A shadow struct carries the raw fields
/// through serde's machinery so the constructor sees them as untrusted
/// input, not a preexisting invariant to trust.
impl<'de> Deserialize<'de> for Interval {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(serde_derive::Deserialize)]
        struct IntervalShadow {
            start: i64,
            end: i64,
        }
        let IntervalShadow { start, end } = IntervalShadow::deserialize(deserializer)?;
        Interval::new(start, end).map_err(serde::de::Error::custom)
    }
}

impl Interval {
    /// Smart constructor: refuses `end <= start` (both the inverted and the
    /// empty case) as a typed error, never a panic.
    pub(crate) fn new(start: i64, end: i64) -> miette::Result<Self> {
        if end <= start {
            miette::bail!(
                "interval end ({end}) must be strictly after start ({start}); \
                 zero-width and inverted intervals are refused by construction"
            );
        }
        Ok(Self { start, end })
    }

    pub(crate) fn start(&self) -> i64 {
        self.start
    }

    pub(crate) fn end(&self) -> i64 {
        self.end
    }

    /// Allen's `before`: `self` ends strictly before `other` starts (a gap
    /// between them). The inverse (`after`) is `other.before(self)` — Allen
    /// relations don't each get a same-arity mirror op; swap the call-site
    /// argument order instead (documented once here for all six).
    pub(crate) fn before(&self, other: &Self) -> bool {
        self.end < other.start
    }

    /// Allen's `meets`: `self` ends exactly where `other` starts, with no
    /// gap and no overlap — closed-open semantics make this unambiguous
    /// (there is exactly one instant, `self.end`, that decides it).
    pub(crate) fn meets(&self, other: &Self) -> bool {
        self.end == other.start
    }

    /// Allen's `overlaps`: `self` starts first, and the two genuinely
    /// overlap without either containing the other.
    pub(crate) fn overlaps(&self, other: &Self) -> bool {
        self.start < other.start && self.end > other.start && self.end < other.end
    }

    /// Allen's `starts`: same start, `self` ends first (a strict prefix).
    pub(crate) fn starts(&self, other: &Self) -> bool {
        self.start == other.start && self.end < other.end
    }

    /// Allen's `during`: `self` is strictly contained within `other`.
    pub(crate) fn during(&self, other: &Self) -> bool {
        self.start > other.start && self.end < other.end
    }

    /// Allen's `finishes`: same end, `self` starts later (a strict suffix).
    pub(crate) fn finishes(&self, other: &Self) -> bool {
        self.end == other.end && self.start > other.start
    }

    /// The workhorse predicate: do the two intervals share any instant.
    /// Equivalent to `!(self.before(other) || other.before(self) ||
    /// self.meets(other) || other.meets(self))`, but stated directly as the
    /// standard half-open overlap test.
    pub(crate) fn intersects(&self, other: &Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[derive(
    Debug, Copy, Clone, Eq, PartialEq, Hash, serde_derive::Deserialize, serde_derive::Serialize,
)]
pub enum VecElementType {
    F32,
    F64,
}

/// The atom of meaning: every datum in the system is one of these thirteen
/// kinds. Totally ordered — and the declaration order below IS the
/// cross-type order, mirrored exactly by the on-disk tag bytes (see
/// `data/memcmp.rs`); reordering variants is a format migration.
#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, serde_derive::Deserialize, serde_derive::Serialize, Hash,
)]
pub enum DataValue {
    /// null
    Null,
    /// boolean
    Bool(bool),
    /// number, may be int or float
    Num(Num),
    /// string
    Str(SmartString<LazyCompact>),
    /// bytes
    #[serde(with = "serde_bytes")]
    Bytes(Vec<u8>),
    /// UUID
    Uuid(UuidWrapper),
    /// Regex, used internally only
    Regex(RegexWrapper),
    /// list
    List(Vec<DataValue>),
    /// set, used internally only
    Set(BTreeSet<DataValue>),
    /// Array, mainly for proximity search
    Vec(Vector),
    /// Json
    Json(JsonData),
    /// validity,
    Validity(Validity),
    /// a first-class half-open interval value; see [`Interval`]'s doc
    /// comment for the empty/END-sentinel design decisions
    Interval(Interval),
    /// bottom type, used internally only
    Bot,
}

/// Wrapper for JsonValue
#[derive(Clone, PartialEq, Eq, serde_derive::Deserialize, serde_derive::Serialize)]
pub struct JsonData(pub JsonValue);

impl PartialOrd<Self> for JsonData {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JsonData {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.to_string().cmp(&other.0.to_string())
    }
}

impl Deref for JsonData {
    type Target = JsonValue;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Hash for JsonData {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_string().hash(state)
    }
}

/// Vector of floating numbers
#[derive(Debug, Clone)]
pub enum Vector {
    /// 32-bit float array
    F32(Array1<f32>),
    /// 64-bit float array
    F64(Array1<f64>),
}

struct VecBytes<'a>(&'a [u8]);

impl serde::Serialize for VecBytes<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(self.0)
    }
}

impl serde::Serialize for Vector {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // The byte payload is explicitly little-endian, making the stored
        // representation identical on every platform.
        let mut state = serializer.serialize_tuple(2)?;
        match self {
            Vector::F32(a) => {
                state.serialize_element(&0u8)?;
                let mut bytes = Vec::with_capacity(a.len() * 4);
                for el in a {
                    bytes.extend_from_slice(&el.to_le_bytes());
                }
                state.serialize_element(&VecBytes(&bytes))?;
            }
            Vector::F64(a) => {
                state.serialize_element(&1u8)?;
                let mut bytes = Vec::with_capacity(a.len() * 8);
                for el in a {
                    bytes.extend_from_slice(&el.to_le_bytes());
                }
                state.serialize_element(&VecBytes(&bytes))?;
            }
        }
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for Vector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_tuple(2, VectorVisitor)
    }
}

/// Bytes that borrow from the input when the deserializer can lend and
/// copy when it cannot — total over serde's byte access patterns.
struct CowBytes<'de>(std::borrow::Cow<'de, [u8]>);

impl<'de> serde::Deserialize<'de> for CowBytes<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = CowBytes<'de>;
            fn expecting(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.write_str("a byte array")
            }
            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E> {
                Ok(CowBytes(std::borrow::Cow::Borrowed(v)))
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E> {
                Ok(CowBytes(std::borrow::Cow::Owned(v.to_vec())))
            }
            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                Ok(CowBytes(std::borrow::Cow::Owned(v)))
            }
        }
        deserializer.deserialize_bytes(V)
    }
}

struct VectorVisitor;

impl<'de> Visitor<'de> for VectorVisitor {
    type Value = Vector;

    fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("vector representation")
    }
    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let tag: u8 = seq
            .next_element()?
            .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
        // Borrow when the source can lend (slice-backed decode), copy when
        // it cannot (reader-backed decode): a vector must deserialize from
        // BOTH, so demanding `&'de [u8]` here would wrongly refuse any
        // reader-based deserializer.
        let bytes: std::borrow::Cow<'de, [u8]> = seq
            .next_element::<CowBytes<'de>>()?
            .ok_or_else(|| serde::de::Error::invalid_length(1, &self))?
            .0;
        let bytes: &[u8] = &bytes;
        match tag {
            0u8 => {
                if !bytes.len().is_multiple_of(4) {
                    return Err(serde::de::Error::invalid_length(bytes.len(), &self));
                }
                let v: Vec<f32> = bytes
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes(c.try_into().expect("chunk of 4")))
                    .collect();
                Ok(Vector::F32(Array1::from(v)))
            }
            1u8 => {
                if !bytes.len().is_multiple_of(8) {
                    return Err(serde::de::Error::invalid_length(bytes.len(), &self));
                }
                let v: Vec<f64> = bytes
                    .chunks_exact(8)
                    .map(|c| f64::from_le_bytes(c.try_into().expect("chunk of 8")))
                    .collect();
                Ok(Vector::F64(Array1::from(v)))
            }
            _ => Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Unsigned(tag as u64),
                &self,
            )),
        }
    }
}

impl Vector {
    /// Get the length of the vector
    pub fn len(&self) -> usize {
        match self {
            Vector::F32(v) => v.len(),
            Vector::F64(v) => v.len(),
        }
    }
    /// Check if the vector is empty
    pub fn is_empty(&self) -> bool {
        match self {
            Vector::F32(v) => v.is_empty(),
            Vector::F64(v) => v.is_empty(),
        }
    }
    pub(crate) fn el_type(&self) -> VecElementType {
        match self {
            Vector::F32(_) => VecElementType::F32,
            Vector::F64(_) => VecElementType::F64,
        }
    }
    pub(crate) fn get_hash(&self) -> impl AsRef<[u8]> {
        let mut hasher = Sha256::new();
        match self {
            Vector::F32(v) => {
                for e in v.iter() {
                    hasher.update(e.to_le_bytes());
                }
            }
            Vector::F64(v) => {
                for e in v.iter() {
                    hasher.update(e.to_le_bytes());
                }
            }
        }
        hasher.finalize_fixed()
    }
}

impl PartialEq<Self> for Vector {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Vector::F32(l), Vector::F32(r)) => {
                if l.len() != r.len() {
                    return false;
                }
                for (le, re) in l.iter().zip(r) {
                    if !OrderedFloat(*le).eq(&OrderedFloat(*re)) {
                        return false;
                    }
                }
                true
            }
            (Vector::F64(l), Vector::F64(r)) => {
                if l.len() != r.len() {
                    return false;
                }
                for (le, re) in l.iter().zip(r) {
                    if !OrderedFloat(*le).eq(&OrderedFloat(*re)) {
                        return false;
                    }
                }
                true
            }
            _ => false,
        }
    }
}

impl Eq for Vector {}

impl PartialOrd for Vector {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Vector {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Vector::F32(l), Vector::F32(r)) => {
                match l.len().cmp(&r.len()) {
                    Ordering::Equal => (),
                    o => return o,
                }
                for (le, re) in l.iter().zip(r) {
                    match OrderedFloat(*le).cmp(&OrderedFloat(*re)) {
                        Ordering::Equal => continue,
                        o => return o,
                    }
                }
                Ordering::Equal
            }
            (Vector::F32(_), Vector::F64(_)) => Ordering::Less,
            (Vector::F64(l), Vector::F64(r)) => {
                match l.len().cmp(&r.len()) {
                    Ordering::Equal => (),
                    o => return o,
                }
                for (le, re) in l.iter().zip(r) {
                    match OrderedFloat(*le).cmp(&OrderedFloat(*re)) {
                        Ordering::Equal => continue,
                        o => return o,
                    }
                }
                Ordering::Equal
            }
            (Vector::F64(_), Vector::F32(_)) => Ordering::Greater,
        }
    }
}

impl Hash for Vector {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Vector::F32(a) => {
                for el in a {
                    OrderedFloat(*el).hash(state)
                }
            }
            Vector::F64(a) => {
                for el in a {
                    OrderedFloat(*el).hash(state)
                }
            }
        }
    }
}

impl From<bool> for DataValue {
    fn from(value: bool) -> Self {
        DataValue::Bool(value)
    }
}

impl From<i64> for DataValue {
    fn from(v: i64) -> Self {
        DataValue::Num(Num::Int(v))
    }
}

impl From<i32> for DataValue {
    fn from(v: i32) -> Self {
        DataValue::Num(Num::Int(v as i64))
    }
}

impl From<f64> for DataValue {
    fn from(v: f64) -> Self {
        DataValue::Num(Num::Float(v))
    }
}

impl From<&str> for DataValue {
    fn from(v: &str) -> Self {
        DataValue::Str(SmartString::from(v))
    }
}

impl From<String> for DataValue {
    fn from(v: String) -> Self {
        DataValue::Str(SmartString::from(v))
    }
}

impl From<Vec<u8>> for DataValue {
    fn from(v: Vec<u8>) -> Self {
        DataValue::Bytes(v)
    }
}

impl<T: Into<DataValue>> From<Vec<T>> for DataValue {
    fn from(v: Vec<T>) -> Self
    where
        T: Into<DataValue>,
    {
        DataValue::List(v.into_iter().map(Into::into).collect())
    }
}

/// Representing a number
#[derive(Copy, Clone, serde_derive::Deserialize, serde_derive::Serialize)]
pub enum Num {
    /// intger number
    Int(i64),
    /// float number
    Float(f64),
}

impl Hash for Num {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Num::Int(i) => i.hash(state),
            Num::Float(f) => OrderedFloat(*f).hash(state),
        }
    }
}

impl Num {
    pub(crate) fn get_int(&self) -> Option<i64> {
        match self {
            Num::Int(i) => Some(*i),
            Num::Float(f) => {
                // Only floats that are integral AND inside i64's exact range:
                // a saturating `as` cast would silently corrupt an index key.
                //
                // `i64::MAX as f64` rounds *up* to 2^63 (the true max, 2^63-1,
                // isn't exactly representable in f64), so comparing against it
                // admits 2^63 itself — a value one past the real boundary that
                // then saturates to `i64::MAX` on cast, silently fabricating a
                // different number. Use the exact power-of-two bound instead.
                const I64_MAX_BOUND_EXCLUSIVE: f64 = 9223372036854775808.0; // 2^63
                if f.round() == *f && *f >= i64::MIN as f64 && *f < I64_MAX_BOUND_EXCLUSIVE {
                    Some(*f as i64)
                } else {
                    None
                }
            }
        }
    }
    pub(crate) fn get_float(&self) -> f64 {
        match self {
            Num::Int(i) => *i as f64,
            Num::Float(f) => *f,
        }
    }
}

impl PartialEq for Num {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Num {}

impl Display for Num {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Num::Int(i) => write!(f, "{i}"),
            Num::Float(n) => {
                if n.is_nan() {
                    write!(f, r#"to_float("NAN")"#)
                } else if n.is_infinite() {
                    if n.is_sign_negative() {
                        write!(f, r#"to_float("NEG_INF")"#)
                    } else {
                        write!(f, r#"to_float("INF")"#)
                    }
                } else {
                    write!(f, "{n}")
                }
            }
        }
    }
}

impl Debug for Num {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Num::Int(i) => write!(f, "{i}"),
            Num::Float(n) => write!(f, "{n}"),
        }
    }
}

impl PartialOrd for Num {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Num {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Num::Int(i), Num::Float(r)) => {
                let l = *i as f64;
                match l.total_cmp(r) {
                    Ordering::Less => Ordering::Less,
                    Ordering::Equal => Ordering::Less,
                    Ordering::Greater => Ordering::Greater,
                }
            }
            (Num::Float(l), Num::Int(i)) => {
                let r = *i as f64;
                match l.total_cmp(&r) {
                    Ordering::Less => Ordering::Less,
                    Ordering::Equal => Ordering::Greater,
                    Ordering::Greater => Ordering::Greater,
                }
            }
            (Num::Int(l), Num::Int(r)) => l.cmp(r),
            (Num::Float(l), Num::Float(r)) => l.total_cmp(r),
        }
    }
}

impl Debug for DataValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl Display for DataValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DataValue::Null => f.write_str("null"),
            DataValue::Bool(b) => write!(f, "{b}"),
            DataValue::Num(n) => write!(f, "{n}"),
            DataValue::Str(s) => write!(f, "{s:?}"),
            DataValue::Bytes(b) => {
                let bs = STANDARD.encode(b);
                write!(f, "decode_base64({bs:?})")
            }
            DataValue::Uuid(u) => {
                let us = u.0.to_string();
                write!(f, "to_uuid({us:?})")
            }
            DataValue::Regex(rx) => {
                write!(f, "regex({:?})", rx.0.as_str())
            }
            DataValue::List(ls) => f.debug_list().entries(ls).finish(),
            DataValue::Set(s) => f.debug_list().entries(s).finish(),
            DataValue::Bot => write!(f, "null"),
            DataValue::Validity(v) => f
                .debug_struct("Validity")
                .field("timestamp", &v.timestamp.raw())
                .field("is_assert", &v.is_assert.0)
                .finish(),
            DataValue::Interval(iv) => write!(f, "make_interval({}, {})", iv.start(), iv.end()),
            DataValue::Vec(a) => match a {
                Vector::F32(a) => {
                    write!(f, "vec({:?})", a.to_vec())
                }
                Vector::F64(a) => {
                    write!(f, "vec({:?}, \"F64\")", a.to_vec())
                }
            },
            DataValue::Json(j) => {
                if j.is_object() {
                    write!(f, "{}", j.0)
                } else {
                    write!(f, "json({})", j.0)
                }
            }
        }
    }
}

impl DataValue {
    /// Returns a slice of bytes if this one is a Bytes
    pub fn get_bytes(&self) -> Option<&[u8]> {
        match self {
            DataValue::Bytes(b) => Some(b),
            _ => None,
        }
    }
    /// Returns a slice of DataValues if this one is a List
    pub fn get_slice(&self) -> Option<&[DataValue]> {
        match self {
            DataValue::List(l) => Some(l),
            _ => None,
        }
    }
    /// Returns the raw str if this one is a Str
    pub fn get_str(&self) -> Option<&str> {
        match self {
            DataValue::Str(s) => Some(s),
            _ => None,
        }
    }
    /// Returns int if this one is an int
    pub fn get_int(&self) -> Option<i64> {
        match self {
            DataValue::Num(n) => n.get_int(),
            _ => None,
        }
    }
    pub(crate) fn get_non_neg_int(&self) -> Option<u64> {
        match self {
            DataValue::Num(n) => n
                .get_int()
                .and_then(|i| if i < 0 { None } else { Some(i as u64) }),
            _ => None,
        }
    }
    /// Returns float if this one is.
    pub fn get_float(&self) -> Option<f64> {
        match self {
            DataValue::Num(n) => Some(n.get_float()),
            _ => None,
        }
    }
    /// Returns bool if this one is.
    pub fn get_bool(&self) -> Option<bool> {
        match self {
            DataValue::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub(crate) fn uuid(uuid: Uuid) -> Self {
        Self::Uuid(UuidWrapper(uuid))
    }
    pub(crate) fn get_uuid(&self) -> Option<Uuid> {
        match self {
            DataValue::Uuid(UuidWrapper(uuid)) => Some(*uuid),
            DataValue::Str(s) => uuid::Uuid::try_parse(s).ok(),
            _ => None,
        }
    }
    /// Returns the interval if this one is.
    pub(crate) fn get_interval(&self) -> Option<&Interval> {
        match self {
            DataValue::Interval(iv) => Some(iv),
            _ => None,
        }
    }
}

pub(crate) const LARGEST_UTF_CHAR: char = '\u{10ffff}';

#[cfg(test)]
mod num_get_int_tests {
    use super::Num;

    #[test]
    fn rejects_float_at_2_pow_63() {
        // 2^63 is exactly representable in f64 and integral, but one past
        // i64::MAX (2^63 - 1): must not silently saturate to i64::MAX.
        let f = Num::Float(9223372036854775808.0);
        assert_eq!(f.get_int(), None);
    }

    #[test]
    fn rejects_float_below_i64_min() {
        // -2^63 - 2048: the nearest f64-representable integer strictly
        // below -2^63 (ULP doubles to 2048 past that magnitude, so
        // "-2^63 - 1" itself isn't representable and rounds back to -2^63).
        let f = Num::Float(-9223372036854777856.0);
        assert_eq!(f.get_int(), None);
    }

    #[test]
    fn accepts_i64_min_exactly() {
        // -2^63 is exactly representable and *is* a valid i64.
        let f = Num::Float(-9223372036854775808.0);
        assert_eq!(f.get_int(), Some(i64::MIN));
    }

    #[test]
    fn accepts_largest_exactly_representable_integer_below_2_pow_63() {
        // 2^63 - 1024 is the nearest f64-representable integer strictly
        // below 2^63, and it fits comfortably in i64.
        let f = Num::Float(9223372036854774784.0);
        assert_eq!(f.get_int(), Some(9223372036854774784_i64));
    }

    #[test]
    fn accepts_ordinary_ints_and_rejects_non_integral() {
        assert_eq!(Num::Float(42.0).get_int(), Some(42));
        assert_eq!(Num::Float(42.5).get_int(), None);
        assert_eq!(Num::Int(-7).get_int(), Some(-7));
    }
}

#[cfg(test)]
mod interval_deserialize_tests {
    use super::Interval;

    /// Hostile-review finding on story #62 chunk 2: a derived `Deserialize`
    /// on `Interval` lives in this module, so it sees `start`/`end` as
    /// ordinary fields and would build the struct by direct assignment —
    /// bypassing `Interval::new`'s `end > start` invariant. This is the
    /// reviewer's exact probe (`Interval { start: 100, end: 5 }`), taken to
    /// the wire level: a plain `(i64, i64)` tuple serializes byte-identically
    /// to a positional 2-field struct in msgpack, so this is what those
    /// bytes look like on disk without ever constructing the illegal value
    /// in Rust.
    #[test]
    fn valid_interval_round_trips_through_rmp_serde() {
        let iv = Interval::new(5, 15).unwrap();
        let bytes = rmp_serde::to_vec(&iv).unwrap();
        let back: Interval = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, iv);
    }

    #[test]
    fn backwards_interval_bytes_refuse_never_construct() {
        let bad_bytes = rmp_serde::to_vec(&(100i64, 5i64)).unwrap();
        let result: Result<Interval, _> = rmp_serde::from_slice(&bad_bytes);
        assert!(
            result.is_err(),
            "backwards interval bytes must be refused, never constructed"
        );
    }

    #[test]
    fn empty_interval_bytes_refuse_never_construct() {
        let bad_bytes = rmp_serde::to_vec(&(5i64, 5i64)).unwrap();
        let result: Result<Interval, _> = rmp_serde::from_slice(&bad_bytes);
        assert!(
            result.is_err(),
            "empty interval bytes must be refused, never constructed"
        );
    }
}

#[cfg(test)]
mod stored_validity_slot_and_as_of_constructor_tests {
    use super::{AsOf, MAX_VALIDITY_TS, StoredValiditySlot, Validity, ValidityTs};
    use crate::data::value::DataValue;

    /// The whole point of the type: every timestamp it is handed, however
    /// extreme, comes out pinned `is_assert == true`. There is no
    /// constructor parameter that could flip it — this walks the boundary
    /// values a hostile or careless caller might try anyway.
    #[test]
    fn stored_validity_slot_always_pins_assert_true() {
        for ts in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
            let slot = StoredValiditySlot::new(ValidityTs::from_raw(ts));
            match slot.as_datavalue() {
                DataValue::Validity(Validity {
                    timestamp,
                    is_assert,
                }) => {
                    assert!(is_assert.0, "slot for ts={ts} was not pinned to assert");
                    assert_eq!(timestamp, ValidityTs::from_raw(ts));
                }
                other => panic!("expected DataValue::Validity, got {other:?}"),
            }
        }
    }

    /// `AsOf::current(v)` is documented as exactly the special case
    /// `AsOf::at(MAX_VALIDITY_TS, v)` — proved directly rather than left as
    /// a comment, across a spread of `valid` coordinates including the
    /// signed extremes.
    #[test]
    fn as_of_current_is_at_with_max_system_coordinate() {
        for v in [i64::MIN, -100, -1, 0, 1, 100, i64::MAX] {
            let valid = ValidityTs::from_raw(v);
            assert_eq!(AsOf::current(valid), AsOf::at(MAX_VALIDITY_TS, valid));
        }
    }

    /// `AsOf::at` is a pure field assignment — the smart constructor must
    /// be behaviorally identical to the raw struct literal it replaces at
    /// every call site, never a narrower or reordered surface.
    #[test]
    fn as_of_at_matches_the_raw_struct_literal_it_replaces() {
        let sys = ValidityTs::from_raw(42);
        let valid = ValidityTs::from_raw(-7);
        let literal = AsOf { sys, valid };
        assert_eq!(AsOf::at(sys, valid), literal);
    }

    /// The smart constructor at the enforcement-ladder boundary: the
    /// reserved terminal tick is refused as a typed diagnostic, not a
    /// panic, and the message names it "reserved" (the substring every
    /// end-to-end write-path test asserts on).
    #[test]
    fn for_assertion_refuses_the_sentinel() {
        let span = crate::data::span::SourceSpan(0, 0);
        let err = ValidityTs::for_assertion(i64::MAX, span).expect_err("must refuse i64::MAX");
        assert!(err.to_string().contains("reserved"), "got: {err}");
    }

    /// Every other instant, including the signed extremes short of the
    /// sentinel, is accepted unchanged.
    #[test]
    fn for_assertion_accepts_every_non_sentinel_instant() {
        let span = crate::data::span::SourceSpan(0, 0);
        for ts in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1] {
            let vld = ValidityTs::for_assertion(ts, span).expect("must accept non-sentinel");
            assert_eq!(vld, ValidityTs::from_raw(ts));
        }
    }

    /// `from_raw`/`raw` round-trip: the internal-construction path is not
    /// merely permissive, it is lossless — the read counterpart to
    /// `from_raw` recovers exactly the instant handed in, sentinel
    /// included.
    #[test]
    fn from_raw_and_raw_round_trip_including_the_sentinel() {
        for ts in [i64::MIN, -1, 0, 1, i64::MAX] {
            assert_eq!(ValidityTs::from_raw(ts).raw(), ts);
        }
    }
}
