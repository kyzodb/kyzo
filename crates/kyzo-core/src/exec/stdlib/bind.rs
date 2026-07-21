/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Sole BoundOp mint + sole public `resolve_op` registry.
use super::bound_op::{self, BoundOp};
use super::collection;
use super::compare;
use super::convert;
use super::geo;
use super::interval;
use super::metric;
use super::nondet;
use super::numeric;
use super::temporal_format;
use super::text;
use kyzo_model::program::op::{self as opdecl, OpDecl};
use kyzo_model::value::DataValue;
use miette::Result;

/// Sole public door that pairs an [`OpDecl`] with a body.
pub const fn bind_op(decl: OpDecl, body: fn(&[DataValue]) -> Result<DataValue>) -> BoundOp {
    bound_op::mint(decl, body)
}

pub static OP_ABS: BoundOp = bind_op(opdecl::OP_ABS, numeric::op_abs);
pub static OP_ACOS: BoundOp = bind_op(opdecl::OP_ACOS, numeric::op_acos);
pub static OP_ACOSH: BoundOp = bind_op(opdecl::OP_ACOSH, numeric::op_acosh);
pub static OP_ADD: BoundOp = bind_op(opdecl::OP_ADD, numeric::op_add);
pub static OP_APPEND: BoundOp = bind_op(opdecl::OP_APPEND, collection::op_append);
pub static OP_ASIN: BoundOp = bind_op(opdecl::OP_ASIN, numeric::op_asin);
pub static OP_ASINH: BoundOp = bind_op(opdecl::OP_ASINH, numeric::op_asinh);
pub static OP_ASSERT: BoundOp = bind_op(opdecl::OP_ASSERT, compare::op_assert);
pub static OP_ATAN: BoundOp = bind_op(opdecl::OP_ATAN, numeric::op_atan);
pub static OP_ATAN2: BoundOp = bind_op(opdecl::OP_ATAN2, numeric::op_atan2);
pub static OP_ATANH: BoundOp = bind_op(opdecl::OP_ATANH, numeric::op_atanh);
pub static OP_BIT_AND: BoundOp = bind_op(opdecl::OP_BIT_AND, numeric::op_bit_and);
pub static OP_BIT_NOT: BoundOp = bind_op(opdecl::OP_BIT_NOT, numeric::op_bit_not);
pub static OP_BIT_OR: BoundOp = bind_op(opdecl::OP_BIT_OR, numeric::op_bit_or);
pub static OP_BIT_XOR: BoundOp = bind_op(opdecl::OP_BIT_XOR, numeric::op_bit_xor);
pub static OP_CEIL: BoundOp = bind_op(opdecl::OP_CEIL, numeric::op_ceil);
pub static OP_CHARS: BoundOp = bind_op(opdecl::OP_CHARS, text::op_chars);
pub static OP_CHUNKS: BoundOp = bind_op(opdecl::OP_CHUNKS, collection::op_chunks);
pub static OP_CHUNKS_EXACT: BoundOp = bind_op(opdecl::OP_CHUNKS_EXACT, collection::op_chunks_exact);
pub static OP_CONCAT: BoundOp = bind_op(opdecl::OP_CONCAT, collection::op_concat);
pub static OP_COS: BoundOp = bind_op(opdecl::OP_COS, numeric::op_cos);
pub static OP_COSH: BoundOp = bind_op(opdecl::OP_COSH, numeric::op_cosh);
pub static OP_COS_DIST: BoundOp = bind_op(opdecl::OP_COS_DIST, metric::op_cos_dist);
pub static OP_DECODE_BASE64: BoundOp = bind_op(opdecl::OP_DECODE_BASE64, convert::op_decode_base64);
pub static OP_DEG_TO_RAD: BoundOp = bind_op(opdecl::OP_DEG_TO_RAD, geo::op_deg_to_rad);
pub static OP_DIFFERENCE: BoundOp = bind_op(opdecl::OP_DIFFERENCE, collection::op_difference);
pub static OP_DIV: BoundOp = bind_op(opdecl::OP_DIV, numeric::op_div);
pub static OP_DUMP_JSON: BoundOp = bind_op(opdecl::OP_DUMP_JSON, collection::op_dump_json);
pub static OP_ENCODE_BASE64: BoundOp = bind_op(opdecl::OP_ENCODE_BASE64, convert::op_encode_base64);
pub static OP_ENDS_WITH: BoundOp = bind_op(opdecl::OP_ENDS_WITH, text::op_ends_with);
pub static OP_EQ: BoundOp = bind_op(opdecl::OP_EQ, compare::op_eq);
pub static OP_EXP: BoundOp = bind_op(opdecl::OP_EXP, numeric::op_exp);
pub static OP_EXP2: BoundOp = bind_op(opdecl::OP_EXP2, numeric::op_exp2);
pub static OP_FIRST: BoundOp = bind_op(opdecl::OP_FIRST, collection::op_first);
pub static OP_FLOOR: BoundOp = bind_op(opdecl::OP_FLOOR, numeric::op_floor);
pub static OP_FORMAT_TIMESTAMP: BoundOp = bind_op(
    opdecl::OP_FORMAT_TIMESTAMP,
    temporal_format::op_format_timestamp,
);
pub static OP_FROM_SUBSTRINGS: BoundOp =
    bind_op(opdecl::OP_FROM_SUBSTRINGS, text::op_from_substrings);
pub static OP_GE: BoundOp = bind_op(opdecl::OP_GE, compare::op_ge);
pub static OP_GET: BoundOp = bind_op(opdecl::OP_GET, collection::op_get);
pub static OP_GT: BoundOp = bind_op(opdecl::OP_GT, compare::op_gt);
pub static OP_HAVERSINE: BoundOp = bind_op(opdecl::OP_HAVERSINE, geo::op_haversine);
pub static OP_HAVERSINE_DEG_INPUT: BoundOp =
    bind_op(opdecl::OP_HAVERSINE_DEG_INPUT, geo::op_haversine_deg_input);
pub static OP_INTERSECTION: BoundOp = bind_op(opdecl::OP_INTERSECTION, collection::op_intersection);
pub static OP_INTERVAL_BEFORE: BoundOp =
    bind_op(opdecl::OP_INTERVAL_BEFORE, interval::op_interval_before);
pub static OP_INTERVAL_DURING: BoundOp =
    bind_op(opdecl::OP_INTERVAL_DURING, interval::op_interval_during);
pub static OP_INTERVAL_END: BoundOp = bind_op(opdecl::OP_INTERVAL_END, interval::op_interval_end);
pub static OP_INTERVAL_FINISHES: BoundOp =
    bind_op(opdecl::OP_INTERVAL_FINISHES, interval::op_interval_finishes);
pub static OP_INTERVAL_HAS_END: BoundOp =
    bind_op(opdecl::OP_INTERVAL_HAS_END, interval::op_interval_has_end);
pub static OP_INTERVAL_HAS_START: BoundOp = bind_op(
    opdecl::OP_INTERVAL_HAS_START,
    interval::op_interval_has_start,
);
pub static OP_INTERVAL_INTERSECTS: BoundOp = bind_op(
    opdecl::OP_INTERVAL_INTERSECTS,
    interval::op_interval_intersects,
);
pub static OP_INTERVAL_IS_END_UNBOUNDED: BoundOp = bind_op(
    opdecl::OP_INTERVAL_IS_END_UNBOUNDED,
    interval::op_interval_is_end_unbounded,
);
pub static OP_INTERVAL_IS_START_UNBOUNDED: BoundOp = bind_op(
    opdecl::OP_INTERVAL_IS_START_UNBOUNDED,
    interval::op_interval_is_start_unbounded,
);
pub static OP_INTERVAL_MEETS: BoundOp =
    bind_op(opdecl::OP_INTERVAL_MEETS, interval::op_interval_meets);
pub static OP_INTERVAL_OVERLAPS: BoundOp =
    bind_op(opdecl::OP_INTERVAL_OVERLAPS, interval::op_interval_overlaps);
pub static OP_INTERVAL_START: BoundOp =
    bind_op(opdecl::OP_INTERVAL_START, interval::op_interval_start);
pub static OP_INTERVAL_STARTS: BoundOp =
    bind_op(opdecl::OP_INTERVAL_STARTS, interval::op_interval_starts);
pub static OP_INT_RANGE: BoundOp = bind_op(opdecl::OP_INT_RANGE, collection::op_int_range);
pub static OP_IP_DIST: BoundOp = bind_op(opdecl::OP_IP_DIST, metric::op_ip_dist);
pub static OP_IS_BYTES: BoundOp = bind_op(opdecl::OP_IS_BYTES, compare::op_is_bytes);
pub static OP_IS_FINITE: BoundOp = bind_op(opdecl::OP_IS_FINITE, compare::op_is_finite);
pub static OP_IS_FLOAT: BoundOp = bind_op(opdecl::OP_IS_FLOAT, compare::op_is_float);
pub static OP_IS_IN: BoundOp = bind_op(opdecl::OP_IS_IN, compare::op_is_in);
pub static OP_IS_INFINITE: BoundOp = bind_op(opdecl::OP_IS_INFINITE, compare::op_is_infinite);
pub static OP_IS_INT: BoundOp = bind_op(opdecl::OP_IS_INT, compare::op_is_int);
pub static OP_IS_JSON: BoundOp = bind_op(opdecl::OP_IS_JSON, compare::op_is_json);
pub static OP_IS_LIST: BoundOp = bind_op(opdecl::OP_IS_LIST, compare::op_is_list);
pub static OP_IS_NAN: BoundOp = bind_op(opdecl::OP_IS_NAN, compare::op_is_nan);
pub static OP_IS_NULL: BoundOp = bind_op(opdecl::OP_IS_NULL, compare::op_is_null);
pub static OP_IS_NUM: BoundOp = bind_op(opdecl::OP_IS_NUM, compare::op_is_num);
pub static OP_IS_STRING: BoundOp = bind_op(opdecl::OP_IS_STRING, compare::op_is_string);
pub static OP_IS_UUID: BoundOp = bind_op(opdecl::OP_IS_UUID, compare::op_is_uuid);
pub static OP_IS_VEC: BoundOp = bind_op(opdecl::OP_IS_VEC, compare::op_is_vec);
pub static OP_JSON: BoundOp = bind_op(opdecl::OP_JSON, collection::op_json);
pub static OP_JSON_OBJECT: BoundOp = bind_op(opdecl::OP_JSON_OBJECT, collection::op_json_object);
pub static OP_JSON_TO_SCALAR: BoundOp =
    bind_op(opdecl::OP_JSON_TO_SCALAR, collection::op_json_to_scalar);
pub static OP_L2_DIST: BoundOp = bind_op(opdecl::OP_L2_DIST, metric::op_l2_dist);
pub static OP_L2_NORMALIZE: BoundOp = bind_op(opdecl::OP_L2_NORMALIZE, metric::op_l2_normalize);
pub static OP_LAST: BoundOp = bind_op(opdecl::OP_LAST, collection::op_last);
pub static OP_LE: BoundOp = bind_op(opdecl::OP_LE, compare::op_le);
pub static OP_LENGTH: BoundOp = bind_op(opdecl::OP_LENGTH, collection::op_length);
pub static OP_LIST: BoundOp = bind_op(opdecl::OP_LIST, collection::op_list);
pub static OP_LN: BoundOp = bind_op(opdecl::OP_LN, numeric::op_ln);
pub static OP_LOG10: BoundOp = bind_op(opdecl::OP_LOG10, numeric::op_log10);
pub static OP_LOG2: BoundOp = bind_op(opdecl::OP_LOG2, numeric::op_log2);
pub static OP_LOWERCASE: BoundOp = bind_op(opdecl::OP_LOWERCASE, text::op_lowercase);
pub static OP_LT: BoundOp = bind_op(opdecl::OP_LT, compare::op_lt);
pub static OP_MAKE_INTERVAL: BoundOp =
    bind_op(opdecl::OP_MAKE_INTERVAL, interval::op_make_interval);
pub static OP_MAX: BoundOp = bind_op(opdecl::OP_MAX, numeric::op_max);
pub static OP_MAYBE_GET: BoundOp = bind_op(opdecl::OP_MAYBE_GET, collection::op_maybe_get);
pub static OP_MIN: BoundOp = bind_op(opdecl::OP_MIN, numeric::op_min);
pub static OP_MINUS: BoundOp = bind_op(opdecl::OP_MINUS, numeric::op_minus);
pub static OP_MOD: BoundOp = bind_op(opdecl::OP_MOD, numeric::op_mod);
pub static OP_MUL: BoundOp = bind_op(opdecl::OP_MUL, numeric::op_mul);
pub static OP_NEGATE: BoundOp = bind_op(opdecl::OP_NEGATE, numeric::op_negate);
pub static OP_NEQ: BoundOp = bind_op(opdecl::OP_NEQ, compare::op_neq);
pub static OP_NOW: BoundOp = bind_op(opdecl::OP_NOW, nondet::op_now);
pub static OP_PACK_BITS: BoundOp = bind_op(opdecl::OP_PACK_BITS, numeric::op_pack_bits);
pub static OP_PARSE_JSON: BoundOp = bind_op(opdecl::OP_PARSE_JSON, collection::op_parse_json);
pub static OP_PARSE_TIMESTAMP: BoundOp = bind_op(
    opdecl::OP_PARSE_TIMESTAMP,
    temporal_format::op_parse_timestamp,
);
pub static OP_POW: BoundOp = bind_op(opdecl::OP_POW, numeric::op_pow);
pub static OP_PREPEND: BoundOp = bind_op(opdecl::OP_PREPEND, collection::op_prepend);
pub static OP_RAD_TO_DEG: BoundOp = bind_op(opdecl::OP_RAD_TO_DEG, geo::op_rad_to_deg);
pub static OP_RAND_BERNOULLI: BoundOp =
    bind_op(opdecl::OP_RAND_BERNOULLI, nondet::op_rand_bernoulli);
pub static OP_RAND_CHOOSE: BoundOp = bind_op(opdecl::OP_RAND_CHOOSE, nondet::op_rand_choose);
pub static OP_RAND_FLOAT: BoundOp = bind_op(opdecl::OP_RAND_FLOAT, nondet::op_rand_float);
pub static OP_RAND_INT: BoundOp = bind_op(opdecl::OP_RAND_INT, nondet::op_rand_int);
pub static OP_RAND_UUID_V1: BoundOp = bind_op(opdecl::OP_RAND_UUID_V1, nondet::op_rand_uuid_v1);
pub static OP_RAND_UUID_V4: BoundOp = bind_op(opdecl::OP_RAND_UUID_V4, nondet::op_rand_uuid_v4);
pub static OP_RAND_VEC: BoundOp = bind_op(opdecl::OP_RAND_VEC, nondet::op_rand_vec);
pub static OP_REGEX: BoundOp = bind_op(opdecl::OP_REGEX, text::op_regex);
pub static OP_REGEX_EXTRACT: BoundOp = bind_op(opdecl::OP_REGEX_EXTRACT, text::op_regex_extract);
pub static OP_REGEX_EXTRACT_FIRST: BoundOp =
    bind_op(opdecl::OP_REGEX_EXTRACT_FIRST, text::op_regex_extract_first);
pub static OP_REGEX_MATCHES: BoundOp = bind_op(opdecl::OP_REGEX_MATCHES, text::op_regex_matches);
pub static OP_REGEX_REPLACE: BoundOp = bind_op(opdecl::OP_REGEX_REPLACE, text::op_regex_replace);
pub static OP_REGEX_REPLACE_ALL: BoundOp =
    bind_op(opdecl::OP_REGEX_REPLACE_ALL, text::op_regex_replace_all);
pub static OP_REMOVE_JSON_PATH: BoundOp =
    bind_op(opdecl::OP_REMOVE_JSON_PATH, collection::op_remove_json_path);
pub static OP_REVERSE: BoundOp = bind_op(opdecl::OP_REVERSE, collection::op_reverse);
pub static OP_ROUND: BoundOp = bind_op(opdecl::OP_ROUND, numeric::op_round);
pub static OP_SET_JSON_PATH: BoundOp =
    bind_op(opdecl::OP_SET_JSON_PATH, collection::op_set_json_path);
pub static OP_SIGNUM: BoundOp = bind_op(opdecl::OP_SIGNUM, numeric::op_signum);
pub static OP_SIN: BoundOp = bind_op(opdecl::OP_SIN, numeric::op_sin);
pub static OP_SINH: BoundOp = bind_op(opdecl::OP_SINH, numeric::op_sinh);
pub static OP_SLICE: BoundOp = bind_op(opdecl::OP_SLICE, collection::op_slice);
pub static OP_SLICE_STRING: BoundOp = bind_op(opdecl::OP_SLICE_STRING, text::op_slice_string);
pub static OP_SORTED: BoundOp = bind_op(opdecl::OP_SORTED, collection::op_sorted);
pub static OP_SQRT: BoundOp = bind_op(opdecl::OP_SQRT, numeric::op_sqrt);
pub static OP_STARTS_WITH: BoundOp = bind_op(opdecl::OP_STARTS_WITH, text::op_starts_with);
pub static OP_STR_INCLUDES: BoundOp = bind_op(opdecl::OP_STR_INCLUDES, text::op_str_includes);
pub static OP_SUB: BoundOp = bind_op(opdecl::OP_SUB, numeric::op_sub);
pub static OP_T2S: BoundOp = bind_op(opdecl::OP_T2S, text::op_t2s);
pub static OP_TAN: BoundOp = bind_op(opdecl::OP_TAN, numeric::op_tan);
pub static OP_TANH: BoundOp = bind_op(opdecl::OP_TANH, numeric::op_tanh);
pub static OP_TO_BOOL: BoundOp = bind_op(opdecl::OP_TO_BOOL, convert::op_to_bool);
pub static OP_TO_FLOAT: BoundOp = bind_op(opdecl::OP_TO_FLOAT, convert::op_to_float);
pub static OP_TO_INT: BoundOp = bind_op(opdecl::OP_TO_INT, convert::op_to_int);
pub static OP_TO_STRING: BoundOp = bind_op(opdecl::OP_TO_STRING, convert::op_to_string);
pub static OP_TO_UNITY: BoundOp = bind_op(opdecl::OP_TO_UNITY, convert::op_to_unity);
pub static OP_TO_UUID: BoundOp = bind_op(opdecl::OP_TO_UUID, convert::op_to_uuid);
pub static OP_TRIM: BoundOp = bind_op(opdecl::OP_TRIM, text::op_trim);
pub static OP_TRIM_END: BoundOp = bind_op(opdecl::OP_TRIM_END, text::op_trim_end);
pub static OP_TRIM_START: BoundOp = bind_op(opdecl::OP_TRIM_START, text::op_trim_start);
pub static OP_UNICODE_NORMALIZE: BoundOp =
    bind_op(opdecl::OP_UNICODE_NORMALIZE, text::op_unicode_normalize);
pub static OP_UNION: BoundOp = bind_op(opdecl::OP_UNION, collection::op_union);
pub static OP_UNPACK_BITS: BoundOp = bind_op(opdecl::OP_UNPACK_BITS, numeric::op_unpack_bits);
pub static OP_UPPERCASE: BoundOp = bind_op(opdecl::OP_UPPERCASE, text::op_uppercase);
pub static OP_UUID_TIMESTAMP: BoundOp =
    bind_op(opdecl::OP_UUID_TIMESTAMP, convert::op_uuid_timestamp);
pub static OP_VALIDITY: BoundOp = bind_op(opdecl::OP_VALIDITY, convert::op_validity);
pub static OP_VEC: BoundOp = bind_op(opdecl::OP_VEC, convert::op_vec);
pub static OP_WINDOWS: BoundOp = bind_op(opdecl::OP_WINDOWS, collection::op_windows);

/// SEALED NAME — sole public name→BoundOp resolve API.
pub fn resolve_op(name: &str) -> Option<&'static BoundOp> {
    let key = name
        .strip_prefix("OP_")
        .unwrap_or(name)
        .to_ascii_lowercase();
    Some(match key.as_str() {
        "abs" => &OP_ABS,
        "acos" => &OP_ACOS,
        "acosh" => &OP_ACOSH,
        "add" => &OP_ADD,
        "append" => &OP_APPEND,
        "asin" => &OP_ASIN,
        "asinh" => &OP_ASINH,
        "assert" => &OP_ASSERT,
        "atan" => &OP_ATAN,
        "atan2" => &OP_ATAN2,
        "atanh" => &OP_ATANH,
        "bit_and" => &OP_BIT_AND,
        "bit_not" => &OP_BIT_NOT,
        "bit_or" => &OP_BIT_OR,
        "bit_xor" => &OP_BIT_XOR,
        "ceil" => &OP_CEIL,
        "chars" => &OP_CHARS,
        "chunks" => &OP_CHUNKS,
        "chunks_exact" => &OP_CHUNKS_EXACT,
        "concat" => &OP_CONCAT,
        "cos" => &OP_COS,
        "cosh" => &OP_COSH,
        "cos_dist" => &OP_COS_DIST,
        "decode_base64" => &OP_DECODE_BASE64,
        "deg_to_rad" => &OP_DEG_TO_RAD,
        "difference" => &OP_DIFFERENCE,
        "div" => &OP_DIV,
        "dump_json" => &OP_DUMP_JSON,
        "encode_base64" => &OP_ENCODE_BASE64,
        "ends_with" => &OP_ENDS_WITH,
        "eq" => &OP_EQ,
        "exp" => &OP_EXP,
        "exp2" => &OP_EXP2,
        "first" => &OP_FIRST,
        "floor" => &OP_FLOOR,
        "format_timestamp" => &OP_FORMAT_TIMESTAMP,
        "from_substrings" => &OP_FROM_SUBSTRINGS,
        "ge" => &OP_GE,
        "get" => &OP_GET,
        "gt" => &OP_GT,
        "haversine" => &OP_HAVERSINE,
        "haversine_deg_input" => &OP_HAVERSINE_DEG_INPUT,
        "intersection" => &OP_INTERSECTION,
        "interval_before" => &OP_INTERVAL_BEFORE,
        "interval_during" => &OP_INTERVAL_DURING,
        "interval_end" => &OP_INTERVAL_END,
        "interval_finishes" => &OP_INTERVAL_FINISHES,
        "interval_has_end" => &OP_INTERVAL_HAS_END,
        "interval_has_start" => &OP_INTERVAL_HAS_START,
        "interval_intersects" => &OP_INTERVAL_INTERSECTS,
        "interval_is_end_unbounded" => &OP_INTERVAL_IS_END_UNBOUNDED,
        "interval_is_start_unbounded" => &OP_INTERVAL_IS_START_UNBOUNDED,
        "interval_meets" => &OP_INTERVAL_MEETS,
        "interval_overlaps" => &OP_INTERVAL_OVERLAPS,
        "interval_start" => &OP_INTERVAL_START,
        "interval_starts" => &OP_INTERVAL_STARTS,
        "int_range" => &OP_INT_RANGE,
        "ip_dist" => &OP_IP_DIST,
        "is_bytes" => &OP_IS_BYTES,
        "is_finite" => &OP_IS_FINITE,
        "is_float" => &OP_IS_FLOAT,
        "is_in" => &OP_IS_IN,
        "is_infinite" => &OP_IS_INFINITE,
        "is_int" => &OP_IS_INT,
        "is_json" => &OP_IS_JSON,
        "is_list" => &OP_IS_LIST,
        "is_nan" => &OP_IS_NAN,
        "is_null" => &OP_IS_NULL,
        "is_num" => &OP_IS_NUM,
        "is_string" => &OP_IS_STRING,
        "is_uuid" => &OP_IS_UUID,
        "is_vec" => &OP_IS_VEC,
        "json" => &OP_JSON,
        "json_object" => &OP_JSON_OBJECT,
        "json_to_scalar" => &OP_JSON_TO_SCALAR,
        "l2_dist" => &OP_L2_DIST,
        "l2_normalize" => &OP_L2_NORMALIZE,
        "last" => &OP_LAST,
        "le" => &OP_LE,
        "length" => &OP_LENGTH,
        "list" => &OP_LIST,
        "ln" => &OP_LN,
        "log10" => &OP_LOG10,
        "log2" => &OP_LOG2,
        "lowercase" => &OP_LOWERCASE,
        "lt" => &OP_LT,
        "make_interval" => &OP_MAKE_INTERVAL,
        "max" => &OP_MAX,
        "maybe_get" => &OP_MAYBE_GET,
        "min" => &OP_MIN,
        "minus" => &OP_MINUS,
        "mod" => &OP_MOD,
        "mul" => &OP_MUL,
        "negate" => &OP_NEGATE,
        "neq" => &OP_NEQ,
        "now" => &OP_NOW,
        "pack_bits" => &OP_PACK_BITS,
        "parse_json" => &OP_PARSE_JSON,
        "parse_timestamp" => &OP_PARSE_TIMESTAMP,
        "pow" => &OP_POW,
        "prepend" => &OP_PREPEND,
        "rad_to_deg" => &OP_RAD_TO_DEG,
        "rand_bernoulli" => &OP_RAND_BERNOULLI,
        "rand_choose" => &OP_RAND_CHOOSE,
        "rand_float" => &OP_RAND_FLOAT,
        "rand_int" => &OP_RAND_INT,
        "rand_uuid_v1" => &OP_RAND_UUID_V1,
        "rand_uuid_v4" => &OP_RAND_UUID_V4,
        "rand_vec" => &OP_RAND_VEC,
        "regex" => &OP_REGEX,
        "regex_extract" => &OP_REGEX_EXTRACT,
        "regex_extract_first" => &OP_REGEX_EXTRACT_FIRST,
        "regex_matches" => &OP_REGEX_MATCHES,
        "regex_replace" => &OP_REGEX_REPLACE,
        "regex_replace_all" => &OP_REGEX_REPLACE_ALL,
        "remove_json_path" => &OP_REMOVE_JSON_PATH,
        "reverse" => &OP_REVERSE,
        "round" => &OP_ROUND,
        "set_json_path" => &OP_SET_JSON_PATH,
        "signum" => &OP_SIGNUM,
        "sin" => &OP_SIN,
        "sinh" => &OP_SINH,
        "slice" => &OP_SLICE,
        "slice_string" => &OP_SLICE_STRING,
        "sorted" => &OP_SORTED,
        "sqrt" => &OP_SQRT,
        "starts_with" => &OP_STARTS_WITH,
        "str_includes" => &OP_STR_INCLUDES,
        "sub" => &OP_SUB,
        "t2s" => &OP_T2S,
        "tan" => &OP_TAN,
        "tanh" => &OP_TANH,
        "to_bool" => &OP_TO_BOOL,
        "to_float" => &OP_TO_FLOAT,
        "to_int" => &OP_TO_INT,
        "to_string" => &OP_TO_STRING,
        "to_unity" => &OP_TO_UNITY,
        "to_uuid" => &OP_TO_UUID,
        "trim" => &OP_TRIM,
        "trim_end" => &OP_TRIM_END,
        "trim_start" => &OP_TRIM_START,
        "unicode_normalize" => &OP_UNICODE_NORMALIZE,
        "union" => &OP_UNION,
        "unpack_bits" => &OP_UNPACK_BITS,
        "uppercase" => &OP_UPPERCASE,
        "uuid_timestamp" => &OP_UUID_TIMESTAMP,
        "validity" => &OP_VALIDITY,
        "vec" => &OP_VEC,
        "windows" => &OP_WINDOWS,
        _ => return None,
    })
}
