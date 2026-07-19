/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). Calculation rules do not open disk or fetch network: the host
 * loads bytes and passes them via the `content` option; `file://` URLs refuse
 * with [`LocalFileFetchUnavailable`], non-file URLs with
 * [`UrlFetchUnavailable`]. SEAM(network): the original fetched any non-`file://`
 * URL over HTTP via `minreq` behind the `requests` feature (and its
 * `get_file_content_from_url` helper lived here); no network dependency
 * is added — pending DECISION(maintainer): should a pure embedded engine
 * carry an HTTP(S) client for the reader utilities? (See `csv.rs`, same
 * seam.) JSON→DataValue uses the single kernel door
 * `data::json::json_to_datavalue` (no local twin).
 */

//! `JsonReader`: reads a JSON-lines file (or a single JSON array of
//! objects) into a relation, projecting the named `fields`.

use std::collections::BTreeMap;

use itertools::Itertools;
use miette::{Diagnostic, IntoDiagnostic, Result, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use kyzo_model::program::expr::Expr;
use crate::data::json::{JsonValue, json_to_datavalue};
use kyzo_model::SourceSpan;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::DataValue;
use kyzo_model::value::Tuple;
use crate::rules::contract::{
    CancelAuthority, CancelFlag, CannotDetermineArity, FixedRule, FixedRuleOutput,
    FixedRulePayload,
};
use kyzo_model::data_value_any;

/// The filesystem seam of the reader utilities: calculation rules do not
/// open local paths — the host loads bytes and passes them via `content`.
#[derive(Debug, Error, Diagnostic)]
#[error(
    "Reading '{path}' requires filesystem access, which calculation rules do not perform"
)]
#[diagnostic(code(algo::local_file_fetch_unavailable))]
#[diagnostic(help(
    "Host must load the file and pass its bytes via the 'content' option."
))]
pub(crate) struct LocalFileFetchUnavailable {
    pub(crate) path: String,
    #[label]
    pub(crate) span: SourceSpan,
}

/// The network seam of the reader utilities: fetching a non-`file://` URL
/// needs an HTTP(S) client the engine deliberately does not carry (yet) —
/// see the file header.
#[derive(Debug, Error, Diagnostic)]
#[error("Fetching '{url}' requires network access, which this build does not include")]
#[diagnostic(code(algo::url_fetch_unavailable))]
#[diagnostic(help(
    "Host must load local files and pass bytes via the 'content' option. \
     Whether KyzoDB should carry an HTTP(S) client for the reader utilities \
     is an open product decision."
))]
pub(crate) struct UrlFetchUnavailable {
    pub(crate) url: String,
    #[label]
    pub(crate) span: SourceSpan,
}

pub(crate) struct JsonReader;

impl FixedRule for JsonReader {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
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
                    data_value_any!() => Err(BadFields(fields_span)),
                })
                .try_collect()?,
            data_value_any!() => bail!(BadFields(fields_span)),
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
        let content = payload.string_option("content", Some(""))?;
        if !content.is_empty() {
            // Both loops are unbounded in the payload's line/row count, so
            // they must be interruptible. Poll every 1024 items — a stride,
            // not per item, since one line's parse is cheap; a cancel still
            // lands within a bounded number of lines. `check` only reads the
            // flag, so the rows emitted are unchanged when it is unset.
            if json_lines {
                for (i, line) in content.lines().enumerate() {
                    if i % 1024 == 0 {
                        cancel.check()?;
                    }
                    let line = line.trim();
                    if !line.is_empty() {
                        let row: BTreeMap<String, JsonValue> =
                            serde_json::from_str(line).into_diagnostic()?;
                        process_row(&row)?;
                    }
                }
            } else {
                let rows: Vec<BTreeMap<String, JsonValue>> =
                    serde_json::from_str(&content).into_diagnostic()?;
                for (i, row) in rows.iter().enumerate() {
                    if i % 1024 == 0 {
                        cancel.check()?;
                    }
                    process_row(row)?;
                }
            }
        } else {
            let url = payload.string_option("url", None)?;
            match url.strip_prefix("file://") {
                Some(file_path) => bail!(LocalFileFetchUnavailable {
                    path: file_path.to_string(),
                    span: payload.span(),
                }),
                // SEAM(network): see the file header. The original fetched
                // here via minreq behind the `requests` feature.
                None => bail!(UrlFetchUnavailable {
                    url: url.to_string(),
                    span: payload.span(),
                }),
            }
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
            data_value_any!() => bail!(CannotDetermineArity(
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
    use crate::rules::contract::tests_support::run_fixed_rule;

    fn options(content: &str) -> BTreeMap<SmartString<LazyCompact>, Expr> {
        BTreeMap::from([
            (
                SmartString::from("content"),
                Expr::Const {
                    val: DataValue::from(content),
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

    /// JSON-lines host content: fields projected, absent fields Null.
    #[test]
    fn reads_json_lines() {
        let content = r#"{"id": 1, "name": "a"}

{"id": 2}"#;
        let got =
            run_fixed_rule(&JsonReader, vec![], options(content), CancelFlag::default()).unwrap();
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
        let content = r#"{"id": 1, "name": "a"}"#;
        let (auth, flag) = CancelAuthority::arm();
        let _ = auth.cancel();
        assert!(run_fixed_rule(&JsonReader, vec![], options(content), flag).is_err());
    }

    /// The `file://` arm is the filesystem seam: typed refusal, no open.
    #[test]
    fn file_fetch_refuses_typed() {
        let err = run_fixed_rule(
            &JsonReader,
            vec![],
            BTreeMap::from([
                (
                    SmartString::from("url"),
                    Expr::Const {
                        val: DataValue::from("file:///tmp/rows.jsonl"),
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
            ]),
            CancelFlag::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("filesystem"), "{err}");
    }

    /// The URL arm is the network seam: typed refusal, no fetch.
    #[test]
    fn url_fetch_refuses_typed() {
        let err = run_fixed_rule(
            &JsonReader,
            vec![],
            BTreeMap::from([
                (
                    SmartString::from("url"),
                    Expr::Const {
                        val: DataValue::from("https://example.com/rows.jsonl"),
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
            ]),
            CancelFlag::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("network"), "{err}");
    }

    /// Kernel JSON→DataValue door (via `data::json::json_to_datavalue`).
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
