//! Shared vocabulary crate: values, program IR atoms, wire envelopes.
//!
//! Ideal map seat for constructs under `crates/kyzo-model/src/*`.
//! Parse/format bodies land here only with their IR dependencies — never as
//! parked copies that still import `kyzo` internals.

#![forbid(unsafe_code)]

pub mod envelope;
pub mod program;
pub mod typestate;
pub mod value;

pub use program::{SourceSpan, Symbol, SymbolKind};
pub use value::{
    Admission, Arity, AsOf, Bound, CompiledRegexV1, DataValue, DecodeError, Denial, Interval,
    Json, JsonNum, JsonObj, MAX_VALIDITY_TS, Num, NumRepr, NumericOrd, RegexFlags, RegexSource,
    RelationId, SearchHits, StorageKey, StoredValiditySlot, TERMINAL_VALIDITY, Tag, TupleKey,
    TupleT, UuidWrapper, Validity, ValiditySeekBound, ValiditySlot, ValidityTs, Vector,
    VectorComponent, VectorDimension, append_canonical, decode, encode_owned,
};
pub use value::validity_coerce::{
    BadValiditySpecification, data_value_to_vld_spec, str2vld, timestamp_to_micros,
};
pub use envelope::{JsonData, JsonValue, json_from_serde, json_to_datavalue, serde_from_json};
