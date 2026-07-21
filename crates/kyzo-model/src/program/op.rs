//! Builtin op declarations — name, arity, vararg, determinism. No body.
//!
//! Sealed by move_plan.json: OpDecl has no callable field.

/// Declaration of a builtin KyzoScript op. No implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpDecl {
    /// Screaming-case const name, e.g. `"OP_ADD"`.
    pub name: &'static str,
    /// Minimum arity (exact when `!vararg`).
    pub min_arity: usize,
    /// Accepts more than `min_arity` arguments.
    pub vararg: bool,
    /// Same args ⇒ same result; `false` forbids constant folding.
    pub deterministic: bool,
}

impl OpDecl {
    pub const fn new(
        name: &'static str,
        min_arity: usize,
        vararg: bool,
        deterministic: bool,
    ) -> Self {
        Self {
            name,
            min_arity,
            vararg,
            deterministic,
        }
    }

    pub const fn is_vararg(self) -> bool {
        self.vararg
    }
    pub const fn is_deterministic(self) -> bool {
        self.deterministic
    }

    pub fn arity_matches(self, n: usize) -> bool {
        if self.vararg {
            n >= self.min_arity
        } else {
            n == self.min_arity
        }
    }

    pub fn display_name(self) -> String {
        self.name
            .strip_prefix("OP_")
            .unwrap_or(self.name)
            .to_lowercase()
    }

    pub fn arity_requirement(self) -> String {
        if self.vararg {
            format!("at least {} argument(s)", self.min_arity)
        } else {
            format!("exactly {} argument(s)", self.min_arity)
        }
    }
}

pub const OP_ABS: OpDecl = OpDecl::new("OP_ABS", 1, false, true);
pub const OP_ACOS: OpDecl = OpDecl::new("OP_ACOS", 1, false, true);
pub const OP_ACOSH: OpDecl = OpDecl::new("OP_ACOSH", 1, false, true);
pub const OP_ADD: OpDecl = OpDecl::new("OP_ADD", 0, true, true);
pub const OP_APPEND: OpDecl = OpDecl::new("OP_APPEND", 2, false, true);
pub const OP_ASIN: OpDecl = OpDecl::new("OP_ASIN", 1, false, true);
pub const OP_ASINH: OpDecl = OpDecl::new("OP_ASINH", 1, false, true);
pub const OP_ASSERT: OpDecl = OpDecl::new("OP_ASSERT", 1, true, true);
pub const OP_ATAN: OpDecl = OpDecl::new("OP_ATAN", 1, false, true);
pub const OP_ATAN2: OpDecl = OpDecl::new("OP_ATAN2", 2, false, true);
pub const OP_ATANH: OpDecl = OpDecl::new("OP_ATANH", 1, false, true);
pub const OP_BIT_AND: OpDecl = OpDecl::new("OP_BIT_AND", 2, false, true);
pub const OP_BIT_NOT: OpDecl = OpDecl::new("OP_BIT_NOT", 1, false, true);
pub const OP_BIT_OR: OpDecl = OpDecl::new("OP_BIT_OR", 2, false, true);
pub const OP_BIT_XOR: OpDecl = OpDecl::new("OP_BIT_XOR", 2, false, true);
pub const OP_CEIL: OpDecl = OpDecl::new("OP_CEIL", 1, false, true);
pub const OP_CHARS: OpDecl = OpDecl::new("OP_CHARS", 1, false, true);
pub const OP_CHUNKS: OpDecl = OpDecl::new("OP_CHUNKS", 2, false, true);
pub const OP_CHUNKS_EXACT: OpDecl = OpDecl::new("OP_CHUNKS_EXACT", 2, false, true);
pub const OP_CONCAT: OpDecl = OpDecl::new("OP_CONCAT", 1, true, true);
pub const OP_COS: OpDecl = OpDecl::new("OP_COS", 1, false, true);
pub const OP_COSH: OpDecl = OpDecl::new("OP_COSH", 1, false, true);
pub const OP_COS_DIST: OpDecl = OpDecl::new("OP_COS_DIST", 2, false, true);
pub const OP_DECODE_BASE64: OpDecl = OpDecl::new("OP_DECODE_BASE64", 1, false, true);
pub const OP_DEG_TO_RAD: OpDecl = OpDecl::new("OP_DEG_TO_RAD", 1, false, true);
pub const OP_DIFFERENCE: OpDecl = OpDecl::new("OP_DIFFERENCE", 2, true, true);
pub const OP_DIV: OpDecl = OpDecl::new("OP_DIV", 2, false, true);
pub const OP_DUMP_JSON: OpDecl = OpDecl::new("OP_DUMP_JSON", 1, false, true);
pub const OP_ENCODE_BASE64: OpDecl = OpDecl::new("OP_ENCODE_BASE64", 1, false, true);
pub const OP_ENDS_WITH: OpDecl = OpDecl::new("OP_ENDS_WITH", 2, false, true);
pub const OP_EQ: OpDecl = OpDecl::new("OP_EQ", 2, false, true);
pub const OP_EXP: OpDecl = OpDecl::new("OP_EXP", 1, false, true);
pub const OP_EXP2: OpDecl = OpDecl::new("OP_EXP2", 1, false, true);
pub const OP_FIRST: OpDecl = OpDecl::new("OP_FIRST", 1, false, true);
pub const OP_FLOOR: OpDecl = OpDecl::new("OP_FLOOR", 1, false, true);
pub const OP_FORMAT_TIMESTAMP: OpDecl = OpDecl::new("OP_FORMAT_TIMESTAMP", 1, true, true);
pub const OP_FROM_SUBSTRINGS: OpDecl = OpDecl::new("OP_FROM_SUBSTRINGS", 1, false, true);
pub const OP_GE: OpDecl = OpDecl::new("OP_GE", 2, false, true);
pub const OP_GET: OpDecl = OpDecl::new("OP_GET", 2, true, true);
pub const OP_GT: OpDecl = OpDecl::new("OP_GT", 2, false, true);
pub const OP_HAVERSINE: OpDecl = OpDecl::new("OP_HAVERSINE", 4, false, true);
pub const OP_HAVERSINE_DEG_INPUT: OpDecl = OpDecl::new("OP_HAVERSINE_DEG_INPUT", 4, false, true);
pub const OP_INTERSECTION: OpDecl = OpDecl::new("OP_INTERSECTION", 1, true, true);
pub const OP_INTERVAL_BEFORE: OpDecl = OpDecl::new("OP_INTERVAL_BEFORE", 2, false, true);
pub const OP_INTERVAL_DURING: OpDecl = OpDecl::new("OP_INTERVAL_DURING", 2, false, true);
pub const OP_INTERVAL_END: OpDecl = OpDecl::new("OP_INTERVAL_END", 1, false, true);
pub const OP_INTERVAL_FINISHES: OpDecl = OpDecl::new("OP_INTERVAL_FINISHES", 2, false, true);
pub const OP_INTERVAL_HAS_END: OpDecl = OpDecl::new("OP_INTERVAL_HAS_END", 1, false, true);
pub const OP_INTERVAL_HAS_START: OpDecl = OpDecl::new("OP_INTERVAL_HAS_START", 1, false, true);
pub const OP_INTERVAL_INTERSECTS: OpDecl = OpDecl::new("OP_INTERVAL_INTERSECTS", 2, false, true);
pub const OP_INTERVAL_IS_END_UNBOUNDED: OpDecl =
    OpDecl::new("OP_INTERVAL_IS_END_UNBOUNDED", 1, false, true);
pub const OP_INTERVAL_IS_START_UNBOUNDED: OpDecl =
    OpDecl::new("OP_INTERVAL_IS_START_UNBOUNDED", 1, false, true);
pub const OP_INTERVAL_MEETS: OpDecl = OpDecl::new("OP_INTERVAL_MEETS", 2, false, true);
pub const OP_INTERVAL_OVERLAPS: OpDecl = OpDecl::new("OP_INTERVAL_OVERLAPS", 2, false, true);
pub const OP_INTERVAL_START: OpDecl = OpDecl::new("OP_INTERVAL_START", 1, false, true);
pub const OP_INTERVAL_STARTS: OpDecl = OpDecl::new("OP_INTERVAL_STARTS", 2, false, true);
pub const OP_INT_RANGE: OpDecl = OpDecl::new("OP_INT_RANGE", 1, true, true);
pub const OP_IP_DIST: OpDecl = OpDecl::new("OP_IP_DIST", 2, false, true);
pub const OP_IS_BYTES: OpDecl = OpDecl::new("OP_IS_BYTES", 1, false, true);
pub const OP_IS_FINITE: OpDecl = OpDecl::new("OP_IS_FINITE", 1, false, true);
pub const OP_IS_FLOAT: OpDecl = OpDecl::new("OP_IS_FLOAT", 1, false, true);
pub const OP_IS_IN: OpDecl = OpDecl::new("OP_IS_IN", 2, false, true);
pub const OP_IS_INFINITE: OpDecl = OpDecl::new("OP_IS_INFINITE", 1, false, true);
pub const OP_IS_INT: OpDecl = OpDecl::new("OP_IS_INT", 1, false, true);
pub const OP_IS_JSON: OpDecl = OpDecl::new("OP_IS_JSON", 1, false, true);
pub const OP_IS_LIST: OpDecl = OpDecl::new("OP_IS_LIST", 1, false, true);
pub const OP_IS_NAN: OpDecl = OpDecl::new("OP_IS_NAN", 1, false, true);
pub const OP_IS_NULL: OpDecl = OpDecl::new("OP_IS_NULL", 1, false, true);
pub const OP_IS_NUM: OpDecl = OpDecl::new("OP_IS_NUM", 1, false, true);
pub const OP_IS_STRING: OpDecl = OpDecl::new("OP_IS_STRING", 1, false, true);
pub const OP_IS_UUID: OpDecl = OpDecl::new("OP_IS_UUID", 1, false, true);
pub const OP_IS_VEC: OpDecl = OpDecl::new("OP_IS_VEC", 1, false, true);
pub const OP_JSON: OpDecl = OpDecl::new("OP_JSON", 1, false, true);
pub const OP_JSON_OBJECT: OpDecl = OpDecl::new("OP_JSON_OBJECT", 0, true, true);
pub const OP_JSON_TO_SCALAR: OpDecl = OpDecl::new("OP_JSON_TO_SCALAR", 1, false, true);
pub const OP_L2_DIST: OpDecl = OpDecl::new("OP_L2_DIST", 2, false, true);
pub const OP_L2_NORMALIZE: OpDecl = OpDecl::new("OP_L2_NORMALIZE", 1, false, true);
pub const OP_LAST: OpDecl = OpDecl::new("OP_LAST", 1, false, true);
pub const OP_LE: OpDecl = OpDecl::new("OP_LE", 2, false, true);
pub const OP_LENGTH: OpDecl = OpDecl::new("OP_LENGTH", 1, false, true);
pub const OP_LIST: OpDecl = OpDecl::new("OP_LIST", 0, true, true);
pub const OP_LN: OpDecl = OpDecl::new("OP_LN", 1, false, true);
pub const OP_LOG10: OpDecl = OpDecl::new("OP_LOG10", 1, false, true);
pub const OP_LOG2: OpDecl = OpDecl::new("OP_LOG2", 1, false, true);
pub const OP_LOWERCASE: OpDecl = OpDecl::new("OP_LOWERCASE", 1, false, true);
pub const OP_LT: OpDecl = OpDecl::new("OP_LT", 2, false, true);
pub const OP_MAKE_INTERVAL: OpDecl = OpDecl::new("OP_MAKE_INTERVAL", 2, false, true);
pub const OP_MAX: OpDecl = OpDecl::new("OP_MAX", 1, true, true);
pub const OP_MAYBE_GET: OpDecl = OpDecl::new("OP_MAYBE_GET", 2, false, true);
pub const OP_MIN: OpDecl = OpDecl::new("OP_MIN", 1, true, true);
pub const OP_MINUS: OpDecl = OpDecl::new("OP_MINUS", 1, false, true);
pub const OP_MOD: OpDecl = OpDecl::new("OP_MOD", 2, false, true);
pub const OP_MUL: OpDecl = OpDecl::new("OP_MUL", 0, true, true);
pub const OP_NEGATE: OpDecl = OpDecl::new("OP_NEGATE", 1, false, true);
pub const OP_NEQ: OpDecl = OpDecl::new("OP_NEQ", 2, false, true);
pub const OP_NOW: OpDecl = OpDecl::new("OP_NOW", 0, false, false);
pub const OP_PACK_BITS: OpDecl = OpDecl::new("OP_PACK_BITS", 1, false, true);
pub const OP_PARSE_JSON: OpDecl = OpDecl::new("OP_PARSE_JSON", 1, false, true);
pub const OP_PARSE_TIMESTAMP: OpDecl = OpDecl::new("OP_PARSE_TIMESTAMP", 1, false, true);
pub const OP_POW: OpDecl = OpDecl::new("OP_POW", 2, false, true);
pub const OP_PREPEND: OpDecl = OpDecl::new("OP_PREPEND", 2, false, true);
pub const OP_RAD_TO_DEG: OpDecl = OpDecl::new("OP_RAD_TO_DEG", 1, false, true);
pub const OP_RAND_BERNOULLI: OpDecl = OpDecl::new("OP_RAND_BERNOULLI", 1, false, false);
pub const OP_RAND_CHOOSE: OpDecl = OpDecl::new("OP_RAND_CHOOSE", 1, false, false);
pub const OP_RAND_FLOAT: OpDecl = OpDecl::new("OP_RAND_FLOAT", 0, false, false);
pub const OP_RAND_INT: OpDecl = OpDecl::new("OP_RAND_INT", 2, false, false);
pub const OP_RAND_UUID_V1: OpDecl = OpDecl::new("OP_RAND_UUID_V1", 0, false, false);
pub const OP_RAND_UUID_V4: OpDecl = OpDecl::new("OP_RAND_UUID_V4", 0, false, false);
pub const OP_RAND_VEC: OpDecl = OpDecl::new("OP_RAND_VEC", 1, true, false);
pub const OP_REGEX: OpDecl = OpDecl::new("OP_REGEX", 1, false, true);
pub const OP_REGEX_EXTRACT: OpDecl = OpDecl::new("OP_REGEX_EXTRACT", 2, false, true);
pub const OP_REGEX_EXTRACT_FIRST: OpDecl = OpDecl::new("OP_REGEX_EXTRACT_FIRST", 2, false, true);
pub const OP_REGEX_MATCHES: OpDecl = OpDecl::new("OP_REGEX_MATCHES", 2, false, true);
pub const OP_REGEX_REPLACE: OpDecl = OpDecl::new("OP_REGEX_REPLACE", 3, false, true);
pub const OP_REGEX_REPLACE_ALL: OpDecl = OpDecl::new("OP_REGEX_REPLACE_ALL", 3, false, true);
pub const OP_REMOVE_JSON_PATH: OpDecl = OpDecl::new("OP_REMOVE_JSON_PATH", 2, false, true);
pub const OP_REVERSE: OpDecl = OpDecl::new("OP_REVERSE", 1, false, true);
pub const OP_ROUND: OpDecl = OpDecl::new("OP_ROUND", 1, false, true);
pub const OP_SET_JSON_PATH: OpDecl = OpDecl::new("OP_SET_JSON_PATH", 3, false, true);
pub const OP_SIGNUM: OpDecl = OpDecl::new("OP_SIGNUM", 1, false, true);
pub const OP_SIN: OpDecl = OpDecl::new("OP_SIN", 1, false, true);
pub const OP_SINH: OpDecl = OpDecl::new("OP_SINH", 1, false, true);
pub const OP_SLICE: OpDecl = OpDecl::new("OP_SLICE", 3, false, true);
pub const OP_SLICE_STRING: OpDecl = OpDecl::new("OP_SLICE_STRING", 3, false, true);
pub const OP_SORTED: OpDecl = OpDecl::new("OP_SORTED", 1, false, true);
pub const OP_SQRT: OpDecl = OpDecl::new("OP_SQRT", 1, false, true);
pub const OP_STARTS_WITH: OpDecl = OpDecl::new("OP_STARTS_WITH", 2, false, true);
pub const OP_STR_INCLUDES: OpDecl = OpDecl::new("OP_STR_INCLUDES", 2, false, true);
pub const OP_SUB: OpDecl = OpDecl::new("OP_SUB", 2, false, true);
pub const OP_T2S: OpDecl = OpDecl::new("OP_T2S", 1, false, true);
pub const OP_TAN: OpDecl = OpDecl::new("OP_TAN", 1, false, true);
pub const OP_TANH: OpDecl = OpDecl::new("OP_TANH", 1, false, true);
pub const OP_TO_BOOL: OpDecl = OpDecl::new("OP_TO_BOOL", 1, false, true);
pub const OP_TO_FLOAT: OpDecl = OpDecl::new("OP_TO_FLOAT", 1, false, true);
pub const OP_TO_INT: OpDecl = OpDecl::new("OP_TO_INT", 1, false, true);
pub const OP_TO_STRING: OpDecl = OpDecl::new("OP_TO_STRING", 1, false, true);
pub const OP_TO_UNITY: OpDecl = OpDecl::new("OP_TO_UNITY", 1, false, true);
pub const OP_TO_UUID: OpDecl = OpDecl::new("OP_TO_UUID", 1, false, true);
pub const OP_TRIM: OpDecl = OpDecl::new("OP_TRIM", 1, false, true);
pub const OP_TRIM_END: OpDecl = OpDecl::new("OP_TRIM_END", 1, false, true);
pub const OP_TRIM_START: OpDecl = OpDecl::new("OP_TRIM_START", 1, false, true);
pub const OP_UNICODE_NORMALIZE: OpDecl = OpDecl::new("OP_UNICODE_NORMALIZE", 2, false, true);
pub const OP_UNION: OpDecl = OpDecl::new("OP_UNION", 1, true, true);
pub const OP_UNPACK_BITS: OpDecl = OpDecl::new("OP_UNPACK_BITS", 1, false, true);
pub const OP_UPPERCASE: OpDecl = OpDecl::new("OP_UPPERCASE", 1, false, true);
pub const OP_UUID_TIMESTAMP: OpDecl = OpDecl::new("OP_UUID_TIMESTAMP", 1, false, true);
pub const OP_VALIDITY: OpDecl = OpDecl::new("OP_VALIDITY", 1, true, true);
pub const OP_VEC: OpDecl = OpDecl::new("OP_VEC", 1, true, true);
pub const OP_WINDOWS: OpDecl = OpDecl::new("OP_WINDOWS", 2, false, true);

/// Resolve a KyzoScript / serde display name to an [`OpDecl`] (no body).
pub fn resolve_decl(name: &str) -> Option<OpDecl> {
    let key = name
        .strip_prefix("OP_")
        .unwrap_or(name)
        .to_ascii_lowercase();
    Some(match key.as_str() {
        "abs" => OP_ABS,
        "acos" => OP_ACOS,
        "acosh" => OP_ACOSH,
        "add" => OP_ADD,
        "append" => OP_APPEND,
        "asin" => OP_ASIN,
        "asinh" => OP_ASINH,
        "assert" => OP_ASSERT,
        "atan" => OP_ATAN,
        "atan2" => OP_ATAN2,
        "atanh" => OP_ATANH,
        "bit_and" => OP_BIT_AND,
        "bit_not" => OP_BIT_NOT,
        "bit_or" => OP_BIT_OR,
        "bit_xor" => OP_BIT_XOR,
        "ceil" => OP_CEIL,
        "chars" => OP_CHARS,
        "chunks" => OP_CHUNKS,
        "chunks_exact" => OP_CHUNKS_EXACT,
        "concat" => OP_CONCAT,
        "cos" => OP_COS,
        "cosh" => OP_COSH,
        "cos_dist" => OP_COS_DIST,
        "decode_base64" => OP_DECODE_BASE64,
        "deg_to_rad" => OP_DEG_TO_RAD,
        "difference" => OP_DIFFERENCE,
        "div" => OP_DIV,
        "dump_json" => OP_DUMP_JSON,
        "encode_base64" => OP_ENCODE_BASE64,
        "ends_with" => OP_ENDS_WITH,
        "eq" => OP_EQ,
        "exp" => OP_EXP,
        "exp2" => OP_EXP2,
        "first" => OP_FIRST,
        "floor" => OP_FLOOR,
        "format_timestamp" => OP_FORMAT_TIMESTAMP,
        "from_substrings" => OP_FROM_SUBSTRINGS,
        "ge" => OP_GE,
        "get" => OP_GET,
        "gt" => OP_GT,
        "haversine" => OP_HAVERSINE,
        "haversine_deg_input" => OP_HAVERSINE_DEG_INPUT,
        "intersection" => OP_INTERSECTION,
        "interval_before" => OP_INTERVAL_BEFORE,
        "interval_during" => OP_INTERVAL_DURING,
        "interval_end" => OP_INTERVAL_END,
        "interval_finishes" => OP_INTERVAL_FINISHES,
        "interval_has_end" => OP_INTERVAL_HAS_END,
        "interval_has_start" => OP_INTERVAL_HAS_START,
        "interval_intersects" => OP_INTERVAL_INTERSECTS,
        "interval_is_end_unbounded" => OP_INTERVAL_IS_END_UNBOUNDED,
        "interval_is_start_unbounded" => OP_INTERVAL_IS_START_UNBOUNDED,
        "interval_meets" => OP_INTERVAL_MEETS,
        "interval_overlaps" => OP_INTERVAL_OVERLAPS,
        "interval_start" => OP_INTERVAL_START,
        "interval_starts" => OP_INTERVAL_STARTS,
        "int_range" => OP_INT_RANGE,
        "ip_dist" => OP_IP_DIST,
        "is_bytes" => OP_IS_BYTES,
        "is_finite" => OP_IS_FINITE,
        "is_float" => OP_IS_FLOAT,
        "is_in" => OP_IS_IN,
        "is_infinite" => OP_IS_INFINITE,
        "is_int" => OP_IS_INT,
        "is_json" => OP_IS_JSON,
        "is_list" => OP_IS_LIST,
        "is_nan" => OP_IS_NAN,
        "is_null" => OP_IS_NULL,
        "is_num" => OP_IS_NUM,
        "is_string" => OP_IS_STRING,
        "is_uuid" => OP_IS_UUID,
        "is_vec" => OP_IS_VEC,
        "json" => OP_JSON,
        "json_object" => OP_JSON_OBJECT,
        "json_to_scalar" => OP_JSON_TO_SCALAR,
        "l2_dist" => OP_L2_DIST,
        "l2_normalize" => OP_L2_NORMALIZE,
        "last" => OP_LAST,
        "le" => OP_LE,
        "length" => OP_LENGTH,
        "list" => OP_LIST,
        "ln" => OP_LN,
        "log10" => OP_LOG10,
        "log2" => OP_LOG2,
        "lowercase" => OP_LOWERCASE,
        "lt" => OP_LT,
        "make_interval" => OP_MAKE_INTERVAL,
        "max" => OP_MAX,
        "maybe_get" => OP_MAYBE_GET,
        "min" => OP_MIN,
        "minus" => OP_MINUS,
        "mod" => OP_MOD,
        "mul" => OP_MUL,
        "negate" => OP_NEGATE,
        "neq" => OP_NEQ,
        "now" => OP_NOW,
        "pack_bits" => OP_PACK_BITS,
        "parse_json" => OP_PARSE_JSON,
        "parse_timestamp" => OP_PARSE_TIMESTAMP,
        "pow" => OP_POW,
        "prepend" => OP_PREPEND,
        "rad_to_deg" => OP_RAD_TO_DEG,
        "rand_bernoulli" => OP_RAND_BERNOULLI,
        "rand_choose" => OP_RAND_CHOOSE,
        "rand_float" => OP_RAND_FLOAT,
        "rand_int" => OP_RAND_INT,
        "rand_uuid_v1" => OP_RAND_UUID_V1,
        "rand_uuid_v4" => OP_RAND_UUID_V4,
        "rand_vec" => OP_RAND_VEC,
        "regex" => OP_REGEX,
        "regex_extract" => OP_REGEX_EXTRACT,
        "regex_extract_first" => OP_REGEX_EXTRACT_FIRST,
        "regex_matches" => OP_REGEX_MATCHES,
        "regex_replace" => OP_REGEX_REPLACE,
        "regex_replace_all" => OP_REGEX_REPLACE_ALL,
        "remove_json_path" => OP_REMOVE_JSON_PATH,
        "reverse" => OP_REVERSE,
        "round" => OP_ROUND,
        "set_json_path" => OP_SET_JSON_PATH,
        "signum" => OP_SIGNUM,
        "sin" => OP_SIN,
        "sinh" => OP_SINH,
        "slice" => OP_SLICE,
        "slice_string" => OP_SLICE_STRING,
        "sorted" => OP_SORTED,
        "sqrt" => OP_SQRT,
        "starts_with" => OP_STARTS_WITH,
        "str_includes" => OP_STR_INCLUDES,
        "sub" => OP_SUB,
        "t2s" => OP_T2S,
        "tan" => OP_TAN,
        "tanh" => OP_TANH,
        "to_bool" => OP_TO_BOOL,
        "to_float" => OP_TO_FLOAT,
        "to_int" => OP_TO_INT,
        "to_string" => OP_TO_STRING,
        "to_unity" => OP_TO_UNITY,
        "to_uuid" => OP_TO_UUID,
        "trim" => OP_TRIM,
        "trim_end" => OP_TRIM_END,
        "trim_start" => OP_TRIM_START,
        "unicode_normalize" => OP_UNICODE_NORMALIZE,
        "union" => OP_UNION,
        "unpack_bits" => OP_UNPACK_BITS,
        "uppercase" => OP_UPPERCASE,
        "uuid_timestamp" => OP_UUID_TIMESTAMP,
        "validity" => OP_VALIDITY,
        "vec" => OP_VEC,
        "windows" => OP_WINDOWS,
        _ => return None,
    })
}

use serde::Deserialize;

impl serde::Serialize for OpDecl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.name)
    }
}

impl<'de> serde::Deserialize<'de> for OpDecl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = String::deserialize(deserializer)?;
        resolve_decl(&v).ok_or_else(|| serde::de::Error::custom(format!("unknown op: {v}")))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Fixed-rule option declarations — closed name vocabulary, no bodies.
// ─────────────────────────────────────────────────────────────────────────

/// Declaration of a fixed-rule option name. Closed vocabulary: an unknown
/// name cannot resolve, so it cannot enter a [`crate::program::rule::FixedRuleOptions`] bag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FixedRuleOptionDecl {
    /// Wire / surface name, e.g. `"data"`.
    pub name: &'static str,
}

impl FixedRuleOptionDecl {
    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }
}

pub const OPT_BREAK_TIES: FixedRuleOptionDecl = FixedRuleOptionDecl::new("break_ties");
pub const OPT_CONDITION: FixedRuleOptionDecl = FixedRuleOptionDecl::new("condition");
pub const OPT_CONTENT: FixedRuleOptionDecl = FixedRuleOptionDecl::new("content");
pub const OPT_DATA: FixedRuleOptionDecl = FixedRuleOptionDecl::new("data");
pub const OPT_DELIMITER: FixedRuleOptionDecl = FixedRuleOptionDecl::new("delimiter");
pub const OPT_DELTA: FixedRuleOptionDecl = FixedRuleOptionDecl::new("delta");
pub const OPT_DESCENDING: FixedRuleOptionDecl = FixedRuleOptionDecl::new("descending");
pub const OPT_EPSILON: FixedRuleOptionDecl = FixedRuleOptionDecl::new("epsilon");
pub const OPT_FIELDS: FixedRuleOptionDecl = FixedRuleOptionDecl::new("fields");
pub const OPT_HAS_HEADERS: FixedRuleOptionDecl = FixedRuleOptionDecl::new("has_headers");
pub const OPT_HEURISTIC: FixedRuleOptionDecl = FixedRuleOptionDecl::new("heuristic");
pub const OPT_ITERATIONS: FixedRuleOptionDecl = FixedRuleOptionDecl::new("iterations");
pub const OPT_JSON_LINES: FixedRuleOptionDecl = FixedRuleOptionDecl::new("json_lines");
pub const OPT_K: FixedRuleOptionDecl = FixedRuleOptionDecl::new("k");
pub const OPT_KEEP_DEPTH: FixedRuleOptionDecl = FixedRuleOptionDecl::new("keep_depth");
pub const OPT_KEEP_TIES: FixedRuleOptionDecl = FixedRuleOptionDecl::new("keep_ties");
pub const OPT_LIMIT: FixedRuleOptionDecl = FixedRuleOptionDecl::new("limit");
pub const OPT_MAX_CLIQUES: FixedRuleOptionDecl = FixedRuleOptionDecl::new("max_cliques");
pub const OPT_MAX_ITER: FixedRuleOptionDecl = FixedRuleOptionDecl::new("max_iter");
pub const OPT_NULL_IF_ABSENT: FixedRuleOptionDecl = FixedRuleOptionDecl::new("null_if_absent");
pub const OPT_OUT: FixedRuleOptionDecl = FixedRuleOptionDecl::new("out");
pub const OPT_PREPEND_INDEX: FixedRuleOptionDecl = FixedRuleOptionDecl::new("prepend_index");
pub const OPT_SEED: FixedRuleOptionDecl = FixedRuleOptionDecl::new("seed");
pub const OPT_SKIP: FixedRuleOptionDecl = FixedRuleOptionDecl::new("skip");
pub const OPT_SORT_BY: FixedRuleOptionDecl = FixedRuleOptionDecl::new("sort_by");
pub const OPT_STEPS: FixedRuleOptionDecl = FixedRuleOptionDecl::new("steps");
pub const OPT_TAKE: FixedRuleOptionDecl = FixedRuleOptionDecl::new("take");
pub const OPT_THETA: FixedRuleOptionDecl = FixedRuleOptionDecl::new("theta");
pub const OPT_TYPES: FixedRuleOptionDecl = FixedRuleOptionDecl::new("types");
pub const OPT_UNDIRECTED: FixedRuleOptionDecl = FixedRuleOptionDecl::new("undirected");
pub const OPT_URL: FixedRuleOptionDecl = FixedRuleOptionDecl::new("url");
pub const OPT_WEIGHT: FixedRuleOptionDecl = FixedRuleOptionDecl::new("weight");

/// Resolve a fixed-rule option surface name to a [`FixedRuleOptionDecl`].
/// Unknown / misspelled names return `None` — unconstructible into an options bag.
pub fn resolve_fixed_rule_option(name: &str) -> Option<FixedRuleOptionDecl> {
    Some(match name {
        "break_ties" => OPT_BREAK_TIES,
        "condition" => OPT_CONDITION,
        "content" => OPT_CONTENT,
        "data" => OPT_DATA,
        "delimiter" => OPT_DELIMITER,
        "delta" => OPT_DELTA,
        "descending" => OPT_DESCENDING,
        "epsilon" => OPT_EPSILON,
        "fields" => OPT_FIELDS,
        "has_headers" => OPT_HAS_HEADERS,
        "heuristic" => OPT_HEURISTIC,
        "iterations" => OPT_ITERATIONS,
        "json_lines" => OPT_JSON_LINES,
        "k" => OPT_K,
        "keep_depth" => OPT_KEEP_DEPTH,
        "keep_ties" => OPT_KEEP_TIES,
        "limit" => OPT_LIMIT,
        "max_cliques" => OPT_MAX_CLIQUES,
        "max_iter" => OPT_MAX_ITER,
        "null_if_absent" => OPT_NULL_IF_ABSENT,
        "out" => OPT_OUT,
        "prepend_index" => OPT_PREPEND_INDEX,
        "seed" => OPT_SEED,
        "skip" => OPT_SKIP,
        "sort_by" => OPT_SORT_BY,
        "steps" => OPT_STEPS,
        "take" => OPT_TAKE,
        "theta" => OPT_THETA,
        "types" => OPT_TYPES,
        "undirected" => OPT_UNDIRECTED,
        "url" => OPT_URL,
        "weight" => OPT_WEIGHT,
        _ => return None,
    })
}

impl serde::Serialize for FixedRuleOptionDecl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.name)
    }
}

impl<'de> serde::Deserialize<'de> for FixedRuleOptionDecl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = String::deserialize(deserializer)?;
        resolve_fixed_rule_option(&v)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown fixed-rule option: {v}")))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Search modality option declarations — extensible closed vocabulary.
//
// [OPEN] cross-story dep — #249 T1 under epic #353: engine `SearchConfig`
// today admits only Hnsw/Fts/Lsh. Spatial/sparse modality option names
// (`SEARCH_OPT_*`) land here when those `SearchConfig` variants exist.
// Unknown surface names already refuse via `resolve_search_modality_option`
// → `UnknownSearchModalityOption` (not a placeholder path).
// ─────────────────────────────────────────────────────────────────────────

/// Declaration of a search-atom modality option name. Closed vocabulary:
/// an unknown name cannot resolve, so it cannot enter a
/// [`crate::program::rule::SearchModalityOptions`] bag.
///
/// Extensible by appending `SEARCH_OPT_*` constants — not by an open string
/// bag. `query` and `filter` are **not** modality options: they are
/// first-class fields on [`crate::program::rule::SearchInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SearchModalityOptionDecl {
    /// Wire / surface name, e.g. `"k"`.
    pub name: &'static str,
}

impl SearchModalityOptionDecl {
    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }
}

pub const SEARCH_OPT_K: SearchModalityOptionDecl = SearchModalityOptionDecl::new("k");
pub const SEARCH_OPT_EF: SearchModalityOptionDecl = SearchModalityOptionDecl::new("ef");
pub const SEARCH_OPT_RADIUS: SearchModalityOptionDecl = SearchModalityOptionDecl::new("radius");
pub const SEARCH_OPT_BIND_FIELD: SearchModalityOptionDecl =
    SearchModalityOptionDecl::new("bind_field");
pub const SEARCH_OPT_BIND_FIELD_IDX: SearchModalityOptionDecl =
    SearchModalityOptionDecl::new("bind_field_idx");
pub const SEARCH_OPT_BIND_DISTANCE: SearchModalityOptionDecl =
    SearchModalityOptionDecl::new("bind_distance");
pub const SEARCH_OPT_BIND_VECTOR: SearchModalityOptionDecl =
    SearchModalityOptionDecl::new("bind_vector");
pub const SEARCH_OPT_BIND_SCORE: SearchModalityOptionDecl =
    SearchModalityOptionDecl::new("bind_score");
pub const SEARCH_OPT_SCORE_KIND: SearchModalityOptionDecl =
    SearchModalityOptionDecl::new("score_kind");

/// Resolve a search modality option surface name to a [`SearchModalityOptionDecl`].
/// Unknown / misspelled names return `None` — unconstructible into the options bag.
///
/// [OPEN] cross-story dep — #249 T1 / #353: append spatial / sparse
/// `SEARCH_OPT_*` names here when engine `SearchConfig` gains those variants.
/// Until then unknown names refuse; this vocabulary is complete for Hnsw/Fts/Lsh.
pub fn resolve_search_modality_option(name: &str) -> Option<SearchModalityOptionDecl> {
    Some(match name {
        "k" => SEARCH_OPT_K,
        "ef" => SEARCH_OPT_EF,
        "radius" => SEARCH_OPT_RADIUS,
        "bind_field" => SEARCH_OPT_BIND_FIELD,
        "bind_field_idx" => SEARCH_OPT_BIND_FIELD_IDX,
        "bind_distance" => SEARCH_OPT_BIND_DISTANCE,
        "bind_vector" => SEARCH_OPT_BIND_VECTOR,
        "bind_score" => SEARCH_OPT_BIND_SCORE,
        "score_kind" => SEARCH_OPT_SCORE_KIND,
        _ => return None,
    })
}

impl serde::Serialize for SearchModalityOptionDecl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.name)
    }
}

impl<'de> serde::Deserialize<'de> for SearchModalityOptionDecl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = String::deserialize(deserializer)?;
        resolve_search_modality_option(&v)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown search modality option: {v}")))
    }
}

/// Closed opcode table — every `OP_*` decl. The round-trip pin below locks
/// `resolve_decl` + serde against silent drift between the table and the
/// match arms.
pub const ALL_OPS: &[OpDecl] = &[
    OP_ABS,
    OP_ACOS,
    OP_ACOSH,
    OP_ADD,
    OP_APPEND,
    OP_ASIN,
    OP_ASINH,
    OP_ASSERT,
    OP_ATAN,
    OP_ATAN2,
    OP_ATANH,
    OP_BIT_AND,
    OP_BIT_NOT,
    OP_BIT_OR,
    OP_BIT_XOR,
    OP_CEIL,
    OP_CHARS,
    OP_CHUNKS,
    OP_CHUNKS_EXACT,
    OP_CONCAT,
    OP_COS,
    OP_COSH,
    OP_COS_DIST,
    OP_DECODE_BASE64,
    OP_DEG_TO_RAD,
    OP_DIFFERENCE,
    OP_DIV,
    OP_DUMP_JSON,
    OP_ENCODE_BASE64,
    OP_ENDS_WITH,
    OP_EQ,
    OP_EXP,
    OP_EXP2,
    OP_FIRST,
    OP_FLOOR,
    OP_FORMAT_TIMESTAMP,
    OP_FROM_SUBSTRINGS,
    OP_GE,
    OP_GET,
    OP_GT,
    OP_HAVERSINE,
    OP_HAVERSINE_DEG_INPUT,
    OP_INTERSECTION,
    OP_INTERVAL_BEFORE,
    OP_INTERVAL_DURING,
    OP_INTERVAL_END,
    OP_INTERVAL_FINISHES,
    OP_INTERVAL_HAS_END,
    OP_INTERVAL_HAS_START,
    OP_INTERVAL_INTERSECTS,
    OP_INTERVAL_IS_END_UNBOUNDED,
    OP_INTERVAL_IS_START_UNBOUNDED,
    OP_INTERVAL_MEETS,
    OP_INTERVAL_OVERLAPS,
    OP_INTERVAL_START,
    OP_INTERVAL_STARTS,
    OP_INT_RANGE,
    OP_IP_DIST,
    OP_IS_BYTES,
    OP_IS_FINITE,
    OP_IS_FLOAT,
    OP_IS_IN,
    OP_IS_INFINITE,
    OP_IS_INT,
    OP_IS_JSON,
    OP_IS_LIST,
    OP_IS_NAN,
    OP_IS_NULL,
    OP_IS_NUM,
    OP_IS_STRING,
    OP_IS_UUID,
    OP_IS_VEC,
    OP_JSON,
    OP_JSON_OBJECT,
    OP_JSON_TO_SCALAR,
    OP_L2_DIST,
    OP_L2_NORMALIZE,
    OP_LAST,
    OP_LE,
    OP_LENGTH,
    OP_LIST,
    OP_LN,
    OP_LOG10,
    OP_LOG2,
    OP_LOWERCASE,
    OP_LT,
    OP_MAKE_INTERVAL,
    OP_MAX,
    OP_MAYBE_GET,
    OP_MIN,
    OP_MINUS,
    OP_MOD,
    OP_MUL,
    OP_NEGATE,
    OP_NEQ,
    OP_NOW,
    OP_PACK_BITS,
    OP_PARSE_JSON,
    OP_PARSE_TIMESTAMP,
    OP_POW,
    OP_PREPEND,
    OP_RAD_TO_DEG,
    OP_RAND_BERNOULLI,
    OP_RAND_CHOOSE,
    OP_RAND_FLOAT,
    OP_RAND_INT,
    OP_RAND_UUID_V1,
    OP_RAND_UUID_V4,
    OP_RAND_VEC,
    OP_REGEX,
    OP_REGEX_EXTRACT,
    OP_REGEX_EXTRACT_FIRST,
    OP_REGEX_MATCHES,
    OP_REGEX_REPLACE,
    OP_REGEX_REPLACE_ALL,
    OP_REMOVE_JSON_PATH,
    OP_REVERSE,
    OP_ROUND,
    OP_SET_JSON_PATH,
    OP_SIGNUM,
    OP_SIN,
    OP_SINH,
    OP_SLICE,
    OP_SLICE_STRING,
    OP_SORTED,
    OP_SQRT,
    OP_STARTS_WITH,
    OP_STR_INCLUDES,
    OP_SUB,
    OP_T2S,
    OP_TAN,
    OP_TANH,
    OP_TO_BOOL,
    OP_TO_FLOAT,
    OP_TO_INT,
    OP_TO_STRING,
    OP_TO_UNITY,
    OP_TO_UUID,
    OP_TRIM,
    OP_TRIM_END,
    OP_TRIM_START,
    OP_UNICODE_NORMALIZE,
    OP_UNION,
    OP_UNPACK_BITS,
    OP_UPPERCASE,
    OP_UUID_TIMESTAMP,
    OP_VALIDITY,
    OP_VEC,
    OP_WINDOWS,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_table_round_trips_display_name_and_serde() {
        assert_eq!(ALL_OPS.len(), 149, "update ALL_OPS when OP_* consts change");
        for &op in ALL_OPS {
            assert_eq!(
                resolve_decl(&op.display_name()),
                Some(op),
                "display_name resolve missed {}",
                op.name
            );
            assert_eq!(
                resolve_decl(op.name),
                Some(op),
                "OP_ name resolve missed {}",
                op.name
            );
            let wire = serde_json::to_string(&op).expect("serialize");
            let back: OpDecl = serde_json::from_str(&wire).expect("deserialize");
            assert_eq!(back, op, "serde round-trip drifted for {}", op.name);
        }
        assert!(resolve_decl("not_an_op").is_none());
        assert!(serde_json::from_str::<OpDecl>(r#""not_an_op""#).is_err());
    }
}
