/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). The `file://` paths (JSON-lines and whole-array) are fully
 * ported. SEAM(network): the original fetched any non-`file://` URL over
 * HTTP via `minreq` behind the `requests` feature (and its
 * `get_file_content_from_url` helper lived here); no network dependency
 * is added — the URL arm refuses with the typed [`UrlFetchUnavailable`],
 * pending DECISION(maintainer): should a pure embedded engine carry an
 * HTTP(S) client for the reader utilities? (See `csv.rs`, same seam.)
 * SEAM(json): the JSON→DataValue conversion lived in upstream
 * `data/json.rs`, which the kernel port dropped as then-unused; the
 * conversion is local here (`json_to_datavalue`) and re-homes to
 * `data/json.rs` when that file is reinstated (`ColType::Json` coercion
 * needs it too — one name per concept, tracked in the reconciliation
 * notes). `log::error!` on fetch failure is gone with the fetch.
 */

//! `JsonReader`: reads a JSON-lines file (or a single JSON array of
//! objects) into a relation, projecting the named `fields`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufRead;
use std::{fs, io};

use itertools::Itertools;
use miette::{Diagnostic, IntoDiagnostic, Result, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::json::JsonValue;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::data::value::Tuple;
use crate::fixed_rule::{
    CancelFlag, CannotDetermineArity, FixedRule, FixedRuleOutput, FixedRulePayload,
};

/// The network seam of the reader utilities: fetching a non-`file://` URL
/// needs an HTTP(S) client the engine deliberately does not carry (yet) —
/// see the file header.
#[derive(Debug, Error, Diagnostic)]
#[error("Fetching '{url}' requires network access, which this build does not include")]
#[diagnostic(code(algo::url_fetch_unavailable))]
#[diagnostic(help(
    "Only 'file://' URLs are supported. Whether KyzoDB should carry an \
     HTTP(S) client for the reader utilities is an open product decision."
))]
pub(crate) struct UrlFetchUnavailable {
    pub(crate) url: String,
    #[label]
    pub(crate) span: SourceSpan,
}

/// JSON value → engine value, the upstream `data/json.rs` mapping:
/// numbers become ints when integral (falling back to floats, then to the
/// decimal string for numbers representable as neither); arrays recurse
/// into lists; objects stay opaque `Json` values.
pub(crate) fn json_to_datavalue(v: &JsonValue) -> DataValue {
    match v {
        JsonValue::Null => DataValue::Null,
        JsonValue::Bool(b) => DataValue::Bool(*b),
        JsonValue::Number(n) => match n.as_i64() {
            Some(i) => DataValue::from(i),
            None => match n.as_f64() {
                Some(f) => DataValue::from(f),
                None => DataValue::from(n.to_string()),
            },
        },
        JsonValue::String(s) => DataValue::Str(s.into()),
        JsonValue::Array(arr) => DataValue::List(arr.iter().map(json_to_datavalue).collect()),
        JsonValue::Object(d) => DataValue::Json(crate::data::json::json_from_serde(
            &JsonValue::Object(d.clone()),
        )),
    }
}

pub(crate) struct JsonReader;

impl FixedRule for JsonReader {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let url = payload.string_option("url", None)?;
        let json_lines = payload.bool_option("json_lines", Some(true))?;
        let null_if_absent = payload.bool_option("null_if_absent", Some(false))?;
        let prepend_index = payload.bool_option("prepend_index", Some(false))?;

        #[derive(Error, Diagnostic, Debug)]
        #[error("fields specification must be a list of strings")]
        #[diagnostic(code(eval::algo_bad_fields))]
        struct BadFields(#[label] SourceSpan);

        let fields_expr = payload.expr_option("fields", None)?;
        let fields_span = fields_expr.span();
        let fields: Vec<_> = match fields_expr.eval_to_const()? {
            DataValue::List(l) => l
                .into_iter()
                .map(|d| match d {
                    DataValue::Str(s) => Ok(s),
                    _ => Err(BadFields(fields_span)),
                })
                .try_collect()?,
            _ => bail!(BadFields(fields_span)),
        };
        let mut counter = -1i64;
        let mut process_row = |row: &BTreeMap<String, JsonValue>| -> Result<()> {
            let mut ret: Tuple = if prepend_index {
                counter += 1;
                Tuple::from_vec(vec![DataValue::from(counter)])
            } else {
                Tuple::new()
            };
            for field in &fields {
                let val = match row.get(field as &str) {
                    None => {
                        if null_if_absent {
                            DataValue::Null
                        } else {
                            bail!("field {} is absent from JSON line", field);
                        }
                    }
                    Some(v) => json_to_datavalue(v),
                };
                ret.push(val);
            }
            out.put(ret)?;
            Ok(())
        };
        match url.strip_prefix("file://") {
            Some(file_path) => {
                // Both loops are unbounded in the file's line/row count, so
                // they must be interruptible. Poll every 1024 items — a
                // stride, not per item, since one line's parse is cheap; a
                // cancel still lands within a bounded number of lines. `check`
                // only reads the flag, so the rows emitted are unchanged when
                // it is unset.
                if json_lines {
                    let file = File::open(file_path).into_diagnostic()?;
                    for (i, line) in io::BufReader::new(file).lines().enumerate() {
                        if i % 1024 == 0 {
                            cancel.check()?;
                        }
                        let line = line.into_diagnostic()?;
                        let line = line.trim();
                        if !line.is_empty() {
                            let row: BTreeMap<String, JsonValue> =
                                serde_json::from_str(line).into_diagnostic()?;
                            process_row(&row)?;
                        }
                    }
                } else {
                    let content = fs::read_to_string(file_path).into_diagnostic()?;
                    let rows: Vec<BTreeMap<String, JsonValue>> =
                        serde_json::from_str(&content).into_diagnostic()?;
                    for (i, row) in rows.iter().enumerate() {
                        if i % 1024 == 0 {
                            cancel.check()?;
                        }
                        process_row(row)?;
                    }
                }
            }
            // SEAM(network): see the file header. The original fetched
            // here via minreq behind the `requests` feature.
            None => bail!(UrlFetchUnavailable {
                url: url.to_string(),
                span: payload.span(),
            }),
        }
        Ok(())
    }

    fn arity(
        &self,
        opts: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        span: SourceSpan,
    ) -> Result<usize> {
        let with_row_num = match opts.get("prepend_index") {
            None => 0,
            Some(Expr::Const {
                val: DataValue::Bool(true),
                ..
            }) => 1,
            Some(Expr::Const {
                val: DataValue::Bool(false),
                ..
            }) => 0,
            _ => bail!(CannotDetermineArity(
                "JsonReader".to_string(),
                "invalid option 'prepend_index' given, expect a boolean".to_string(),
                span
            )),
        };
        let fields = opts.get("fields").ok_or_else(|| {
            CannotDetermineArity(
                "JsonReader".to_string(),
                "option 'fields' not provided".to_string(),
                span,
            )
        })?;
        Ok(match fields.clone().eval_to_const()? {
            DataValue::List(l) => l.len() + with_row_num,
            _ => bail!(CannotDetermineArity(
                "JsonReader".to_string(),
                "invalid option 'fields' given, expect a list".to_string(),
                span
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed_rule::tests_support::run_fixed_rule;
    use std::io::Write;

    fn options(url: &str) -> BTreeMap<SmartString<LazyCompact>, Expr> {
        BTreeMap::from([
            (
                SmartString::from("url"),
                Expr::Const {
                    val: DataValue::from(url),
                    span: SourceSpan::default(),
                },
            ),
            (
                SmartString::from("fields"),
                Expr::Const {
                    val: DataValue::List(vec![DataValue::from("id"), DataValue::from("name")]),
                    span: SourceSpan::default(),
                },
            ),
            (
                SmartString::from("null_if_absent"),
                Expr::Const {
                    val: DataValue::from(true),
                    span: SourceSpan::default(),
                },
            ),
        ])
    }

    /// JSON-lines local file: fields projected, absent fields Null.
    #[test]
    fn reads_json_lines() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"id": 1, "name": "a"}}"#).unwrap();
        writeln!(f).unwrap();
        writeln!(f, r#"{{"id": 2}}"#).unwrap();
        let url = format!("file://{}", f.path().display());
        let got =
            run_fixed_rule(&JsonReader, vec![], options(&url), CancelFlag::default()).unwrap();
        assert_eq!(got.len(), 2);
        let want0: Tuple = Tuple::from_vec(vec![DataValue::from(1i64), DataValue::from("a")]);
        let want1: Tuple = Tuple::from_vec(vec![DataValue::from(2i64), DataValue::Null]);
        assert_eq!(got[0], want0);
        assert_eq!(got[1], want1);
    }

    /// CANCELLATION: a raised flag refuses before rows are emitted (the
    /// stride poll fires on the first line, `i == 0`), so a large JSON-lines
    /// file is interruptible; the unset-flag read above emitted every row, so
    /// the poll changes nothing when the flag is clear.
    #[test]
    fn honors_cancel() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"id": 1, "name": "a"}}"#).unwrap();
        let url = format!("file://{}", f.path().display());
        let flag = CancelFlag::default();
        flag.cancel();
        assert!(run_fixed_rule(&JsonReader, vec![], options(&url), flag).is_err());
    }

    /// The URL arm is the network seam: typed refusal, no fetch.
    #[test]
    fn url_fetch_refuses_typed() {
        let err = run_fixed_rule(
            &JsonReader,
            vec![],
            options("https://example.com/rows.jsonl"),
            CancelFlag::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("network"), "{err}");
    }

    /// The local JSON→DataValue mapping (re-homes to `data/json.rs`).
    #[test]
    fn json_conversion() {
        let v: JsonValue =
            serde_json::from_str(r#"[1, 2.5, "x", null, true, [1], {"a": 1}]"#).unwrap();
        let got = json_to_datavalue(&v);
        let l = got.get_slice().unwrap();
        assert_eq!(l[0], DataValue::from(1i64));
        assert_eq!(l[1], DataValue::from(2.5f64));
        assert_eq!(l[2], DataValue::from("x"));
        assert_eq!(l[3], DataValue::Null);
        assert_eq!(l[4], DataValue::from(true));
        assert_eq!(l[5], DataValue::List(vec![DataValue::from(1i64)]));
        assert!(matches!(l[6], DataValue::Json(_)));
    }
}
