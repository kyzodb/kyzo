//! text.rs — stdlib kernel (move_plan).

use itertools::Itertools;
use miette::{Result, bail, ensure, miette};
use unicode_normalization::UnicodeNormalization;

use kyzo_model::data_value_any;
use kyzo_model::value::json_convert::to_json;
use kyzo_model::value::{DataValue, Json, RegexFlags, RegexSource};

pub(crate) fn op_chars(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::List(
        args[0]
            .get_str()
            .ok_or_else(|| miette!("'chars' requires strings"))?
            .chars()
            .map(|c| DataValue::from(c.to_string()))
            .collect_vec(),
    ))
}

fn affix_op(
    op: &'static str,
    args: &[DataValue],
    on_str: impl FnOnce(&str, &str) -> bool,
    on_bytes: impl FnOnce(&[u8], &[u8]) -> bool,
) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(l), DataValue::Str(r)) => Ok(DataValue::from(on_str(l, r))),
        (DataValue::Bytes(l), DataValue::Bytes(r)) => Ok(DataValue::from(on_bytes(l, r))),
        _ => bail!("'{op}' requires strings or bytes"),
    }
}

pub(crate) fn op_ends_with(args: &[DataValue]) -> Result<DataValue> {
    affix_op(
        "ends_with",
        args,
        |l, r| l.ends_with(r),
        |l, r| l.ends_with(r),
    )
}

pub(crate) fn op_from_substrings(args: &[DataValue]) -> Result<DataValue> {
    let mut ret = String::new();
    match &args[0] {
        DataValue::List(ss) => {
            for arg in ss {
                if let DataValue::Str(s) = arg {
                    ret.push_str(s);
                } else {
                    bail!("'from_substring' requires a list of strings")
                }
            }
        }
        DataValue::Set(ss) => {
            for arg in ss {
                if let DataValue::Str(s) = arg {
                    ret.push_str(s);
                } else {
                    bail!("'from_substring' requires a list of strings")
                }
            }
        }
        data_value_any!() => bail!("'from_substring' requires a list of strings"),
    }
    Ok(DataValue::from(ret))
}

pub(crate) fn op_lowercase(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(s) => Ok(DataValue::from(s.to_lowercase())),
        data_value_any!() => bail!("'lowercase' requires strings"),
    }
}

pub(crate) fn op_regex(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        r @ DataValue::Regex(_) => r.clone(),
        DataValue::Str(s) => DataValue::Regex(
            RegexSource::validated(RegexFlags::NONE, s.clone())
                .map_err(|err| miette!("The string cannot be interpreted as regex: {err:?}"))?,
        ),
        data_value_any!() => bail!("'regex' requires strings"),
    })
}

pub(crate) fn op_regex_extract(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(s), DataValue::Regex(r)) => {
            let compiled = compile_regex_value(r)?;
            let found = compiled.find_iter(s).map(DataValue::from).collect_vec();
            Ok(DataValue::List(found))
        }
        _ => bail!("'regex_extract' requires strings"),
    }
}

pub(crate) fn op_regex_extract_first(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(s), DataValue::Regex(r)) => {
            let found = compile_regex_value(r)?.find(s).map(DataValue::from);
            Ok(found.unwrap_or(DataValue::Null))
        }
        _ => bail!("'regex_extract_first' requires strings"),
    }
}

pub(crate) fn op_regex_matches(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(s), DataValue::Regex(r)) => {
            Ok(DataValue::from(compile_regex_value(r)?.is_match(s)))
        }
        _ => bail!("'regex_matches' requires strings"),
    }
}

pub(crate) fn op_regex_replace(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1], &args[2]) {
        (DataValue::Str(s), DataValue::Regex(r), DataValue::Str(rp)) => Ok(DataValue::Str(
            compile_regex_value(r)?.replace(s, rp).into_owned(),
        )),
        _ => bail!("'regex_replace' requires strings"),
    }
}

pub(crate) fn op_regex_replace_all(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1], &args[2]) {
        (DataValue::Str(s), DataValue::Regex(r), DataValue::Str(rp)) => Ok(DataValue::Str(
            compile_regex_value(r)?.replace_all(s, rp).into_owned(),
        )),
        _ => bail!("'regex_replace' requires strings"),
    }
}

pub(crate) fn op_slice_string(args: &[DataValue]) -> Result<DataValue> {
    let s = args[0]
        .get_str()
        .ok_or_else(|| miette!("first argument to 'slice_string' mut be a string"))?;
    let m = args[1]
        .get_int()
        .ok_or_else(|| miette!("second argument to 'slice_string' mut be an integer"))?;
    ensure!(
        m >= 0,
        "second argument to 'slice_string' mut be a positive integer"
    );
    let n = args[2]
        .get_int()
        .ok_or_else(|| miette!("third argument to 'slice_string' mut be an integer"))?;
    ensure!(
        n >= m,
        "third argument to 'slice_string' mut be a positive integer greater than the second argument"
    );
    Ok(DataValue::Str(
        s.chars().skip(m as usize).take((n - m) as usize).collect(),
    ))
}

pub(crate) fn op_starts_with(args: &[DataValue]) -> Result<DataValue> {
    affix_op(
        "starts_with",
        args,
        |l, r| l.starts_with(r),
        |l, r| l.starts_with(r),
    )
}

pub(crate) fn op_str_includes(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(l), DataValue::Str(r)) => Ok(DataValue::from(l.find(r as &str).is_some())),
        _ => bail!("'str_includes' requires strings"),
    }
}

pub(crate) fn op_t2s(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Str(s) => DataValue::Str(fast2s::convert(s)),
        d @ (data_value_any!()) => d.clone(),
    })
}

pub(crate) fn op_trim(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(s) => Ok(DataValue::from(s.trim())),
        data_value_any!() => bail!("'trim' requires strings"),
    }
}

pub(crate) fn op_trim_end(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(s) => Ok(DataValue::from(s.trim_end())),
        data_value_any!() => bail!("'trim_end' requires strings"),
    }
}

pub(crate) fn op_trim_start(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(s) => Ok(DataValue::from(s.trim_start())),
        v @ (data_value_any!()) => bail!("'trim_start' requires strings, got {}", v),
    }
}

pub(crate) fn op_unicode_normalize(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(s), DataValue::Str(n)) => Ok(DataValue::Str(match n as &str {
            "nfc" => s.nfc().collect(),
            "nfd" => s.nfd().collect(),
            "nfkc" => s.nfkc().collect(),
            "nfkd" => s.nfkd().collect(),
            u => bail!("unknown normalization {} for 'unicode_normalize'", u),
        })),
        _ => bail!("'unicode_normalize' requires strings"),
    }
}

pub(crate) fn op_uppercase(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(s) => Ok(DataValue::from(s.to_uppercase())),
        data_value_any!() => bail!("'uppercase' requires strings"),
    }
}

/// Compile a regex VALUE for execution. The value itself is storage
/// identity only (`RegexSource` — deliberately no compiled state inside
/// the value plane); the parser's constant-folding hoists `op_regex` so
/// patterns validate once at compile time, but row-wise execution
/// recompiles here. A future optimization would hoist COMPILATION (not
/// just validation) into the operator layer instead, measured by the bench
/// lane.
fn compile_regex_value(r: &RegexSource) -> Result<kyzo_model::value::CompiledRegexV1> {
    r.compile()
        .map_err(|err| miette!("stored regex failed to compile: {err:?}"))
}

pub(crate) fn val2str(arg: &DataValue) -> String {
    match arg {
        DataValue::Str(s) => s.to_string(),
        DataValue::Json(j) => match j {
            Json::Str(s) => s.clone(),
            Json::Null | Json::Bool(_) | Json::Num(_) | Json::Arr(_) | Json::Obj(_) => {
                to_json(arg).to_string()
            }
        },
        data_value_any!() => {
            let jv = to_json(arg);
            jv.to_string()
        }
    }
}
