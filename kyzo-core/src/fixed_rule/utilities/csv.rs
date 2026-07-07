/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). The `file://` path is fully ported (the `csv` crate is pure
 * Rust: csv-core/itoa/ryu/serde, no C toolchain). SEAM(network): the
 * original fetched any non-`file://` URL over HTTP via `minreq` behind
 * the `requests` feature; whether a pure embedded engine should carry a
 * network stack (TLS roots and all) is a product decision for the
 * maintainer, so no network dependency is added and the URL arm refuses
 * with a typed error naming the decision — DECISION(maintainer): enable
 * HTTP(S) fetching for CsvReader/JsonReader (re-adding a minreq-class
 * dependency behind a feature), or keep the engine offline. The two
 * unwraps on the `types` option after coercion are annotated as
 * structural (`coerce` to `[String]` proved the shape); the type-mismatch
 * `bail!`s gain the option's span via `WrongFixedRuleOptionError` where
 * they were bare strings. Output rows flow through the arity-checked
 * writer.
 */

//! `CsvReader`: reads a delimited file into a relation, coercing each
//! column per the `types` option.

use std::collections::BTreeMap;

use csv::StringRecord;
use miette::{IntoDiagnostic, Result, bail, ensure};
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::functions::{op_to_float, op_to_uuid};
use crate::data::program::{FixedRuleOptionNotFoundError, WrongFixedRuleOptionError};
use crate::data::relation::{ColType, NullableColType};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::Tuple;
use crate::data::value::{DataValue, TERMINAL_VALIDITY};
use crate::fixed_rule::utilities::jlines::UrlFetchUnavailable;
use crate::fixed_rule::{
    CancelFlag, CannotDetermineArity, FixedRule, FixedRuleOutput, FixedRulePayload,
};
use crate::parse::parse_type;

pub(crate) struct CsvReader;

impl FixedRule for CsvReader {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let delimiter = payload.string_option("delimiter", Some(","))?;
        let delimiter = delimiter.as_bytes();
        ensure!(
            delimiter.len() == 1,
            WrongFixedRuleOptionError {
                name: "delimiter".to_string(),
                span: payload.span(),
                rule_name: "CsvReader".to_string(),
                help: "'delimiter' must be a single-byte string".to_string()
            }
        );
        let delimiter = delimiter[0];
        let prepend_index = payload.bool_option("prepend_index", Some(false))?;
        let has_headers = payload.bool_option("has_headers", Some(true))?;
        let types_opts = payload.expr_option("types", None)?.eval_to_const()?;
        let typing = NullableColType {
            coltype: ColType::List {
                eltype: Box::new(NullableColType {
                    coltype: ColType::String,
                    nullable: false,
                }),
                len: None,
            },
            nullable: false,
        };
        let types_opts = typing.coerce(types_opts, TERMINAL_VALIDITY.timestamp)?;
        let mut types = vec![];
        // Structural: `coerce` to `[String]` above proved the outer list
        // and the element strings.
        for type_str in types_opts.get_slice().unwrap() {
            let type_str = type_str.get_str().unwrap();
            let typ = parse_type(type_str).map_err(|e| WrongFixedRuleOptionError {
                name: "types".to_string(),
                span: payload.span(),
                rule_name: "CsvReader".to_string(),
                help: e.to_string(),
            })?;
            types.push(typ);
        }

        let mut rdr_builder = csv::ReaderBuilder::new();
        rdr_builder
            .delimiter(delimiter)
            .has_headers(has_headers)
            .flexible(true);

        let mut counter = -1i64;
        let out_tuple_size = if prepend_index {
            types.len() + 1
        } else {
            types.len()
        };
        let mut process_row = |row: StringRecord| -> Result<()> {
            let mut out_tuple = Tuple::with_capacity(out_tuple_size);
            if prepend_index {
                counter += 1;
                out_tuple.push(DataValue::from(counter));
            }
            for (i, typ) in types.iter().enumerate() {
                match row.get(i) {
                    None => {
                        if typ.nullable {
                            out_tuple.push(DataValue::Null)
                        } else {
                            bail!(
                                "encountered null value when processing CSV when non-null required"
                            )
                        }
                    }
                    Some(s) => {
                        let dv = DataValue::from(s);
                        match &typ.coltype {
                            ColType::Any | ColType::String => out_tuple.push(dv),
                            ColType::Uuid => out_tuple.push(match op_to_uuid(&[dv]) {
                                Ok(uuid) => uuid,
                                Err(err) => {
                                    if typ.nullable {
                                        DataValue::Null
                                    } else {
                                        bail!(err)
                                    }
                                }
                            }),
                            ColType::Float => out_tuple.push(match op_to_float(&[dv]) {
                                Ok(data) => data,
                                Err(err) => {
                                    if typ.nullable {
                                        DataValue::Null
                                    } else {
                                        bail!(err)
                                    }
                                }
                            }),
                            ColType::Int => {
                                // Parse as a float, then take it as an int
                                // only if it is exactly integral: "1" ->
                                // 1, "1.5"/"oops" -> null or a typed
                                // refusal. `get_int` alone is strict on
                                // representation (a float 1.0 is not an
                                // int), so the integral check is explicit.
                                let integral = op_to_float(&[dv])
                                    .ok()
                                    .and_then(|v| v.get_float())
                                    .filter(|x| x.is_finite() && x.fract() == 0.0);
                                match integral {
                                    Some(x) => out_tuple.push(DataValue::from(x as i64)),
                                    None if typ.nullable => out_tuple.push(DataValue::Null),
                                    None => bail!("cannot convert {} to type {}", s, typ),
                                };
                            }
                            _ => bail!("cannot convert {} to type {}", s, typ),
                        }
                    }
                }
            }
            out.put(out_tuple)?;
            Ok(())
        };

        let url = payload.string_option("url", None)?;
        match url.strip_prefix("file://") {
            Some(file_path) => {
                let mut rdr = rdr_builder.from_path(file_path).into_diagnostic()?;
                // The record loop is unbounded in the file's row count, so it
                // must be interruptible. Poll every 1024 rows — a stride, not
                // per row, since parsing one CSV record is cheap and the flag
                // read would otherwise dominate; a cancel still lands within a
                // bounded number of rows. `check` only reads the flag, so this
                // never changes the rows emitted when the flag is unset.
                for (i, record) in rdr.records().enumerate() {
                    if i % 1024 == 0 {
                        cancel.check()?;
                    }
                    let record = record.into_diagnostic()?;
                    process_row(record)?;
                }
            }
            // SEAM(network): see the header. The original fetched here via
            // minreq behind the `requests` feature.
            None => bail!(UrlFetchUnavailable {
                url: url.to_string(),
                span: payload.span(),
            }),
        }
        Ok(())
    }

    fn arity(
        &self,
        options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        span: SourceSpan,
    ) -> Result<usize> {
        let with_row_num = match options.get("prepend_index") {
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
                "CsvReader".to_string(),
                "invalid option 'prepend_index' given, expect a boolean".to_string(),
                span
            )),
        };
        let columns = options
            .get("types")
            .ok_or_else(|| FixedRuleOptionNotFoundError {
                name: "types".to_string(),
                span,
                rule_name: "CsvReader".to_string(),
            })?;
        let columns = columns.clone().eval_to_const()?;
        if let Some(l) = columns.get_slice() {
            return Ok(l.len() + with_row_num);
        }
        bail!(CannotDetermineArity(
            "CsvReader".to_string(),
            "invalid option 'types' given, expect positive number or list".to_string(),
            span
        ))
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
                SmartString::from("types"),
                Expr::Const {
                    val: DataValue::List(vec![
                        DataValue::from("String"),
                        DataValue::from("Int"),
                        DataValue::from("Float?"),
                    ]),
                    span: SourceSpan::default(),
                },
            ),
            (
                SmartString::from("has_headers"),
                Expr::Const {
                    val: DataValue::from(false),
                    span: SourceSpan::default(),
                },
            ),
        ])
    }

    /// The local-file path reads and coerces per `types`, including a
    /// nullable column absorbing a bad cell.
    #[test]
    fn reads_local_file_with_typing() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "a,1,1.5").unwrap();
        writeln!(f, "b,2,oops").unwrap();
        let url = format!("file://{}", f.path().display());
        let got = run_fixed_rule(&CsvReader, vec![], options(&url), CancelFlag::default()).unwrap();
        assert_eq!(got.len(), 2);
        let want: Tuple = vec![
            DataValue::from("a"),
            DataValue::from(1i64),
            DataValue::from(1.5f64),
        ]
        .into();
        assert_eq!(got[0], want);
        assert_eq!(got[1][2], DataValue::Null); // nullable Float? absorbed "oops"
    }

    /// CANCELLATION: a raised flag refuses before the rows are emitted (the
    /// stride poll fires on the first record, `i == 0`), so a large file is
    /// interruptible; the unset-flag read above emitted every row, so the
    /// poll changes nothing when the flag is clear.
    #[test]
    fn honors_cancel() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "a,1,1.5").unwrap();
        let url = format!("file://{}", f.path().display());
        let flag = CancelFlag::default();
        flag.cancel();
        assert!(run_fixed_rule(&CsvReader, vec![], options(&url), flag).is_err());
    }

    /// The URL arm is the network seam: typed refusal, no fetch.
    #[test]
    fn url_fetch_refuses_typed() {
        let err = run_fixed_rule(
            &CsvReader,
            vec![],
            options("https://example.com/data.csv"),
            CancelFlag::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("network"), "{err}");
    }
}
