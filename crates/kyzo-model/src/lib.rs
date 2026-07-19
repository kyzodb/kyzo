//! Shared vocabulary crate: values, program IR atoms, wire envelopes, parse.
//!
//! Ideal map seat for constructs under `crates/kyzo-model/src/*`.
//! The parse zone (`parse::parse_script`) is the public KyzoScript language
//! door — text → typed IR with spans and refusals; no engine imports.

#![forbid(unsafe_code)]

pub mod envelope;
pub mod parse;
pub mod program;
pub mod schema;
pub mod typestate;
pub mod value;

pub use program::{BindingPos, Decision, Expr, LazyOp, OpDecl, SourceSpan, Symbol, SymbolKind, resolve_decl};
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
pub use value::json_convert::{to_json, json2val, interval_to_json};
