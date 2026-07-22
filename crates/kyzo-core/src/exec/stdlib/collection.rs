/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! collection.rs — stdlib kernel (move_plan).
use std::collections::BTreeSet;

use itertools::Itertools;
use miette::{IntoDiagnostic, Result, bail, ensure, miette};
use serde_json::{Value, json};

use kyzo_model::data_value_any;
use kyzo_model::value::DataValue;
use kyzo_model::{json_from_serde, serde_from_json};
use serde_json::Value as JsonValue;

use crate::exec::stdlib::text::val2str;

use kyzo_model::value::json_convert::{json2val, to_json};

pub(crate) fn op_append(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::List(l) => {
            let mut l = l.clone();
            l.push(args[1].clone());
            Ok(DataValue::List(l))
        }
        DataValue::Set(l) => {
            let mut l = l.iter().cloned().collect_vec();
            l.push(args[1].clone());
            Ok(DataValue::List(l))
        }
        data_value_any!() => bail!("'append' requires first argument to be a list"),
    }
}

fn list_window_op(
    op: &'static str,
    args: &[DataValue],
    windows: impl FnOnce(&[DataValue], usize) -> Vec<DataValue>,
) -> Result<DataValue> {
    let arg = args[0]
        .get_slice()
        .ok_or_else(|| miette!("first argument of '{op}' must be a list"))?;
    let n = args[1]
        .get_int()
        .ok_or_else(|| miette!("second argument of '{op}' must be an integer"))?;
    ensure!(n > 0, "second argument to '{op}' must be positive");
    Ok(DataValue::List(windows(arg, n as usize)))
}

pub(crate) fn op_chunks(args: &[DataValue]) -> Result<DataValue> {
    list_window_op("chunks", args, |arg, n| {
        arg.chunks(n)
            .map(|el| DataValue::List(el.to_vec()))
            .collect_vec()
    })
}

pub(crate) fn op_chunks_exact(args: &[DataValue]) -> Result<DataValue> {
    list_window_op("chunks_exact", args, |arg, n| {
        arg.chunks_exact(n)
            .map(|el| DataValue::List(el.to_vec()))
            .collect_vec()
    })
}

pub(crate) fn op_concat(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(_) => {
            let mut ret: String = Default::default();
            for arg in args {
                if let DataValue::Str(s) = arg {
                    ret += s;
                } else {
                    bail!("'concat' requires strings, or lists");
                }
            }
            Ok(DataValue::from(ret))
        }
        DataValue::List(_) | DataValue::Set(_) => {
            let mut ret = vec![];
            for arg in args {
                if let DataValue::List(l) = arg {
                    ret.extend_from_slice(l);
                } else if let DataValue::Set(s) = arg {
                    ret.extend(s.iter().cloned());
                } else {
                    bail!("'concat' requires strings, or lists");
                }
            }
            Ok(DataValue::List(ret))
        }
        DataValue::Json(_) => {
            let mut ret = json!(null);
            for arg in args {
                if let DataValue::Json(j) = arg {
                    ret = deep_merge_json(ret, serde_from_json(j));
                } else {
                    bail!("'concat' requires strings, lists, or JSON objects");
                }
            }
            Ok(DataValue::Json(json_from_serde(&ret)))
        }
        data_value_any!() => bail!("'concat' requires strings, lists, or JSON objects"),
    }
}

pub(crate) fn op_difference(args: &[DataValue]) -> Result<DataValue> {
    let mut start: BTreeSet<_> = match &args[0] {
        DataValue::List(l) => l.iter().cloned().collect(),
        DataValue::Set(s) => s.iter().cloned().collect(),
        data_value_any!() => bail!("'difference' requires lists"),
    };
    for arg in &args[1..] {
        match arg {
            DataValue::List(l) => {
                for el in l {
                    start.remove(el);
                }
            }
            DataValue::Set(s) => {
                for el in s {
                    start.remove(el);
                }
            }
            data_value_any!() => bail!("'difference' requires lists"),
        }
    }
    Ok(DataValue::List(start.into_iter().collect()))
}

pub(crate) fn op_dump_json(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Json(j) => Ok(DataValue::Str(serde_from_json(j).to_string())),
        data_value_any!() => bail!("dump_json requires a json argument"),
    }
}

pub(crate) fn op_first(args: &[DataValue]) -> Result<DataValue> {
    Ok(match args[0]
        .get_slice()
        .ok_or_else(|| miette!("'first' requires lists"))?
        .first()
        .cloned()
    {
        Some(v) => v,
        None => DataValue::Null,
    })
}

pub(crate) fn op_get(args: &[DataValue]) -> Result<DataValue> {
    match get_impl(args) {
        Ok(res) => Ok(res),
        Err(err) => {
            if let Some(default) = args.get(2) {
                Ok(default.clone())
            } else {
                Err(err)
            }
        }
    }
}

pub(crate) fn op_intersection(args: &[DataValue]) -> Result<DataValue> {
    let mut start: BTreeSet<_> = match &args[0] {
        DataValue::List(l) => l.iter().cloned().collect(),
        DataValue::Set(s) => s.iter().cloned().collect(),
        data_value_any!() => bail!("'intersection' requires lists"),
    };
    for arg in &args[1..] {
        match arg {
            DataValue::List(l) => {
                let other: BTreeSet<_> = l.iter().cloned().collect();
                start = start.intersection(&other).cloned().collect();
            }
            DataValue::Set(s) => start = start.intersection(s).cloned().collect(),
            data_value_any!() => bail!("'intersection' requires lists"),
        }
    }
    Ok(DataValue::List(start.into_iter().collect()))
}

pub(crate) fn op_int_range(args: &[DataValue]) -> Result<DataValue> {
    let [start, end] = match args.len() {
        1 => {
            let end = args[0]
                .get_int()
                .ok_or_else(|| miette!("'int_range' requires integer argument for end"))?;
            [0, end]
        }
        2 => {
            let start = args[0]
                .get_int()
                .ok_or_else(|| miette!("'int_range' requires integer argument for start"))?;
            let end = args[1]
                .get_int()
                .ok_or_else(|| miette!("'int_range' requires integer argument for end"))?;
            [start, end]
        }
        3 => {
            let start = args[0]
                .get_int()
                .ok_or_else(|| miette!("'int_range' requires integer argument for start"))?;
            let end = args[1]
                .get_int()
                .ok_or_else(|| miette!("'int_range' requires integer argument for end"))?;
            let step = args[2]
                .get_int()
                .ok_or_else(|| miette!("'int_range' requires integer argument for step"))?;
            let mut current = start;
            let mut result = vec![];
            if step > 0 {
                while current < end {
                    result.push(DataValue::from(current));
                    // Checked: a step landing past i64::MAX would otherwise
                    // wrap (or abort in debug builds) near the type's edge.
                    current = match current.checked_add(step) {
                        Some(nxt) => nxt,
                        None => break,
                    };
                }
            } else {
                while current > end {
                    result.push(DataValue::from(current));
                    current = match current.checked_add(step) {
                        Some(nxt) => nxt,
                        None => break,
                    };
                }
            }
            return Ok(DataValue::List(result));
        }
        other_len => bail!("'int_range' requires 1 to 3 argument, got {other_len}"),
    };
    Ok(DataValue::List((start..end).map(DataValue::from).collect()))
}

pub(crate) fn op_json(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::Json(json_from_serde(&to_json(&args[0]))))
}

pub(crate) fn op_json_object(args: &[DataValue]) -> Result<DataValue> {
    ensure!(
        args.len().is_multiple_of(2),
        "json_object requires an even number of arguments"
    );
    let mut obj = serde_json::Map::with_capacity(args.len() / 2);
    for pair in args.chunks_exact(2) {
        let key = val2str(&pair[0]);
        let value = to_json(&pair[1]);
        obj.insert(key.to_string(), value);
    }
    Ok(DataValue::Json(json_from_serde(&Value::Object(obj))))
}

pub(crate) fn op_json_to_scalar(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Json(j) => json2val(serde_from_json(j)),
        d @ (data_value_any!()) => d.clone(),
    })
}

pub(crate) fn op_last(args: &[DataValue]) -> Result<DataValue> {
    Ok(match args[0]
        .get_slice()
        .ok_or_else(|| miette!("'last' requires lists"))?
        .last()
        .cloned()
    {
        Some(v) => v,
        None => DataValue::Null,
    })
}

pub(crate) fn op_length(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match &args[0] {
        DataValue::Set(s) => s.len() as i64,
        DataValue::List(l) => l.len() as i64,
        DataValue::Str(s) => s.chars().count() as i64,
        DataValue::Bytes(b) => b.len() as i64,
        DataValue::Vector(v) => v.len() as i64,
        data_value_any!() => bail!("'length' requires lists"),
    }))
}

pub(crate) fn op_list(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::List(args.to_vec()))
}

pub(crate) fn op_maybe_get(args: &[DataValue]) -> Result<DataValue> {
    match get_impl(args) {
        Ok(res) => Ok(res),
        Err(_) => Ok(DataValue::Null),
    }
}

pub(crate) fn op_parse_json(args: &[DataValue]) -> Result<DataValue> {
    match args[0].get_str() {
        Some(s) => {
            let value = serde_json::from_str(s).into_diagnostic()?;
            Ok(DataValue::Json(json_from_serde(&value)))
        }
        None => bail!("parse_json requires a string argument"),
    }
}

pub(crate) fn op_prepend(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::List(pl) => {
            let mut l = vec![args[1].clone()];
            l.extend_from_slice(pl);
            Ok(DataValue::List(l))
        }
        DataValue::Set(pl) => {
            let mut l = vec![args[1].clone()];
            l.extend(pl.iter().cloned());
            Ok(DataValue::List(l))
        }
        data_value_any!() => bail!("'prepend' requires first argument to be a list"),
    }
}

pub(crate) fn op_remove_json_path(args: &[DataValue]) -> Result<DataValue> {
    let mut result = to_json(&args[0]);
    let path = args[1]
        .get_slice()
        .ok_or_else(|| miette!("json path must be a list"))?;
    let (last, path) = path
        .split_last()
        .ok_or_else(|| miette!("json path must not be empty"))?;
    let pointer = get_json_path(&mut result, path)?;
    match pointer {
        JsonValue::Object(obj) => {
            let key = val2str(last);
            obj.remove(&key);
        }
        JsonValue::Array(arr) => {
            let key = json_array_index(last)?;
            // `Vec::remove` panics out of range; a missing path is an error
            // like everywhere else in the path walkers (the original panicked).
            ensure!(key < arr.len(), "json path does not exist");
            arr.remove(key);
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {
            bail!("json path does not exist")
        }
    }
    Ok(DataValue::Json(json_from_serde(&result)))
}

pub(crate) fn op_reverse(args: &[DataValue]) -> Result<DataValue> {
    let mut arg = args[0]
        .get_slice()
        .ok_or_else(|| miette!("'reverse' requires lists"))?
        .to_vec();
    arg.reverse();
    Ok(DataValue::List(arg))
}

pub(crate) fn op_set_json_path(args: &[DataValue]) -> Result<DataValue> {
    let mut result = to_json(&args[0]);
    let path = args[1]
        .get_slice()
        .ok_or_else(|| miette!("json path must be a list"))?;
    let pointer = get_json_path(&mut result, path)?;
    let new_val = to_json(&args[2]);
    *pointer = new_val;
    Ok(DataValue::Json(json_from_serde(&result)))
}

pub(crate) fn op_slice(args: &[DataValue]) -> Result<DataValue> {
    let l = args[0]
        .get_slice()
        .ok_or_else(|| miette!("first argument to 'slice' mut be a list"))?;
    let m = args[1]
        .get_int()
        .ok_or_else(|| miette!("second argument to 'slice' mut be an integer"))?;
    let n = args[2]
        .get_int()
        .ok_or_else(|| miette!("third argument to 'slice' mut be an integer"))?;
    let m = get_index(m, l.len(), false)?;
    let n = get_index(n, l.len(), true)?;
    if m > n {
        bail!("slice start index {m} must be <= end index {n}");
    }
    Ok(DataValue::List(l[m..n].to_vec()))
}

pub(crate) fn op_sorted(args: &[DataValue]) -> Result<DataValue> {
    let mut arg = args[0]
        .get_slice()
        .ok_or_else(|| miette!("'sort' requires lists"))?
        .to_vec();
    arg.sort();
    Ok(DataValue::List(arg))
}

pub(crate) fn op_union(args: &[DataValue]) -> Result<DataValue> {
    let mut ret = BTreeSet::new();
    for arg in args {
        match arg {
            DataValue::List(l) => {
                for el in l {
                    ret.insert(el.clone());
                }
            }
            DataValue::Set(s) => {
                for el in s {
                    ret.insert(el.clone());
                }
            }
            data_value_any!() => bail!("'union' requires lists"),
        }
    }
    Ok(DataValue::List(ret.into_iter().collect()))
}

pub(crate) fn op_windows(args: &[DataValue]) -> Result<DataValue> {
    list_window_op("windows", args, |arg, n| {
        arg.windows(n)
            .map(|el| DataValue::List(el.to_vec()))
            .collect_vec()
    })
}

/// A path step into a JSON array, proven non-negative and machine-sized.
/// The original cast `i64 as usize`, so a hostile `-1` became a huge index —
/// harmless on reads, but an OOM-scale `resize_with` on writes.
fn json_array_index(key: &DataValue) -> Result<usize> {
    let i = key
        .get_int()
        .ok_or_else(|| miette!("json path must be a string or a number"))?;
    usize::try_from(i).map_err(|_| miette!("json array index must be non-negative, got {i}"))
}

fn get_json_path<'a>(
    mut pointer: &'a mut JsonValue,
    path: &[DataValue],
) -> Result<&'a mut JsonValue> {
    for key in path {
        match pointer {
            JsonValue::Object(obj) => {
                let key = val2str(key);
                let entry = obj.entry(key).or_insert(json!({}));
                pointer = entry;
            }
            JsonValue::Array(arr) => {
                let key = json_array_index(key)?;
                if arr.len() <= key {
                    arr.resize_with(key + 1, || JsonValue::Null);
                }
                // In bounds: just resized to at least `key + 1`.
                pointer = &mut arr[key];
            }
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {
                bail!("json path does not exist")
            }
        }
    }
    Ok(pointer)
}

fn get_json_path_immutable<'a>(
    mut pointer: &'a JsonValue,
    path: &[DataValue],
) -> Result<&'a JsonValue> {
    for key in path {
        match pointer {
            JsonValue::Object(obj) => {
                let key = val2str(key);
                let entry = obj
                    .get(&key)
                    .ok_or_else(|| miette!("json path does not exist"))?;
                pointer = entry;
            }
            JsonValue::Array(arr) => {
                let key = json_array_index(key)?;
                let val = arr
                    .get(key)
                    .ok_or_else(|| miette!("json path does not exist"))?;
                pointer = val;
            }
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {
                bail!("json path does not exist")
            }
        }
    }
    Ok(pointer)
}

fn deep_merge_json(value1: JsonValue, value2: JsonValue) -> JsonValue {
    match (value1, value2) {
        (JsonValue::Object(mut obj1), JsonValue::Object(obj2)) => {
            for (key, value2) in obj2 {
                let value1 = obj1.remove(&key);
                obj1.insert(key, deep_merge_json(match value1 { Some(v) => v, None => Value::Null }, value2));
            }
            JsonValue::Object(obj1)
        }
        (JsonValue::Array(mut arr1), JsonValue::Array(arr2)) => {
            arr1.extend(arr2);
            JsonValue::Array(arr1)
        }
        (_, value2) => value2,
    }
}

fn get_index(mut i: i64, total: usize, is_upper: bool) -> Result<usize> {
    if i < 0 {
        i += total as i64;
    }
    Ok(if i >= 0 {
        let i = i as usize;
        if i > total || (!is_upper && i == total) {
            bail!("index {} out of bound", i)
        } else {
            i
        }
    } else {
        bail!("index {} out of bound", i)
    })
}

fn get_impl(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::List(l) => {
            let n = args[1]
                .get_int()
                .ok_or_else(|| miette!("second argument to 'get' mut be an integer"))?;
            let idx = get_index(n, l.len(), false)?;
            Ok(l[idx].clone())
        }
        DataValue::Json(json) => {
            let json = serde_from_json(json);
            let res = match &args[1] {
                DataValue::Str(s) => json
                    .get(s.as_str())
                    .ok_or_else(|| miette!("key '{}' not found in json", s))?
                    .clone(),
                DataValue::Num(i) => {
                    let i = i
                        .as_int()
                        .ok_or_else(|| miette!("index '{:?}' not found in json", i))?;
                    json.get(i as usize)
                        .ok_or_else(|| miette!("index '{}' not found in json", i))?
                        .clone()
                }
                DataValue::List(l) => get_json_path_immutable(&json, l)?.clone(),
                data_value_any!() => bail!("second argument to 'get' mut be a string or integer"),
            };
            let res = json2val(res);
            Ok(res)
        }
        data_value_any!() => bail!("first argument to 'get' mut be a list or json"),
    }
}
