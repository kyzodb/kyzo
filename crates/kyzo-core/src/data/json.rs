/*
 * Copyright 2022, The Cozo Project Authors. / Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Host JSON envelopes that need `NamedRows` / miette rendering.
//! `DataValue` <-> JSON conversions live in `kyzo_model::envelope::json`.

use miette::{
    Diagnostic, GraphicalReportHandler, GraphicalTheme, JSONReportHandler, Report, ThemeCharacters,
    ThemeStyles,
};
pub use serde_json::Value as JsonValue;
use serde_json::json;
use std::sync::LazyLock;
use thiserror::Error;

use kyzo_model::value::{DataValue, NonFiniteJsonNumber, Tuple};

pub use kyzo_model::envelope::json::{JsonData, json_to_datavalue};
#[cfg(test)]
pub use kyzo_model::envelope::json::{json_from_serde, serde_from_json};

/// Private seal: any private field blocks struct-literal minting outside
/// this module, so header/row/next cannot be forged past [`NamedRows::try_new`]
/// (P082).
#[derive(Debug, Clone)]
struct NamedRowsSeal;

/// The rows of a relation, together with its header names.
///
/// Sole door: [`Self::try_new`] proves every row's width equals
/// `headers.len()`. Payload fields are private; the private
/// [`NamedRowsSeal`] blocks struct-literal minting outside this module
/// (P082). Read via [`Self::headers`] / [`Self::rows`] / [`Self::next`];
/// consume via [`Self::into_parts`] / [`Self::into_rows`] /
/// [`Self::with_next`]. Illegal (misaligned) NamedRows are unconstructible.
#[derive(Debug, Clone)]
pub struct NamedRows {
    headers: Vec<String>,
    rows: Vec<Tuple>,
    next: Option<Box<NamedRows>>,
    _seal: NamedRowsSeal,
}

/// Header↔row width mismatch at the [`NamedRows`] door (P082).
#[derive(Debug, Error, Diagnostic)]
#[error(
    "NamedRows arity mismatch: header width {header_arity}, row {row_index} has width {row_arity}"
)]
#[diagnostic(code(fixed_rule::named_rows_arity))]
pub struct NamedRowsArityError {
    pub header_arity: usize,
    pub row_index: usize,
    pub row_arity: usize,
}

impl NamedRows {
    /// Sole door: every row's width equals `headers.len()`.
    pub fn try_new(
        headers: Vec<String>,
        rows: Vec<Tuple>,
    ) -> std::result::Result<Self, NamedRowsArityError> {
        let header_arity = headers.len();
        for (row_index, row) in rows.iter().enumerate() {
            let row_arity = row.len();
            if row_arity != header_arity {
                return Err(NamedRowsArityError {
                    header_arity,
                    row_index,
                    row_arity,
                });
            }
        }
        Ok(Self {
            headers,
            rows,
            next: None,
            _seal: NamedRowsSeal,
        })
    }

    /// One-cell `status: OK` — header/row widths match by construction (P082).
    pub(crate) fn status_ok() -> Self {
        Self {
            headers: vec!["status".to_string()],
            rows: vec![Tuple::from_vec(vec![DataValue::from("OK")])],
            next: None,
            _seal: NamedRowsSeal,
        }
    }

    /// `::verify` directive result — three columns by construction (P082).
    pub(crate) fn verify_status_row(
        status: impl Into<DataValue>,
        summary: impl Into<DataValue>,
        detail: impl Into<DataValue>,
    ) -> Self {
        Self {
            headers: vec![
                "status".to_string(),
                "summary".to_string(),
                "detail".to_string(),
            ],
            rows: vec![Tuple::from_vec(vec![
                status.into(),
                summary.into(),
                detail.into(),
            ])],
            next: None,
            _seal: NamedRowsSeal,
        }
    }

    /// One-cell bool column — header/row widths match by construction (P082).
    pub(crate) fn single_bool_column(name: impl Into<String>, value: bool) -> Self {
        Self {
            headers: vec![name.into()],
            rows: vec![Tuple::from_vec(vec![DataValue::from(value)])],
            next: None,
            _seal: NamedRowsSeal,
        }
    }

    /// Bool column plus a bytes column — widths match by construction (P082).
    pub(crate) fn bool_and_bytes_columns(
        bool_name: impl Into<String>,
        bool_value: bool,
        bytes_name: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            headers: vec![bool_name.into(), bytes_name.into()],
            rows: vec![Tuple::from_vec(vec![
                DataValue::from(bool_value),
                DataValue::Bytes(bytes),
            ])],
            next: None,
            _seal: NamedRowsSeal,
        }
    }

    /// Alias of [`Self::try_new`] — typed refuse, never panic (P082).
    pub fn new(
        headers: Vec<String>,
        rows: Vec<Tuple>,
    ) -> std::result::Result<Self, NamedRowsArityError> {
        Self::try_new(headers, rows)
    }

    /// Header names.
    pub fn headers(&self) -> &[String] {
        &self.headers
    }

    /// Result rows.
    pub fn rows(&self) -> &[Tuple] {
        &self.rows
    }

    /// Follow-on page, when present.
    pub fn next(&self) -> Option<&NamedRows> {
        self.next.as_deref()
    }

    /// Consume into headers, rows, and optional follow-on page.
    pub fn into_parts(self) -> (Vec<String>, Vec<Tuple>, Option<Box<NamedRows>>) {
        (self.headers, self.rows, self.next)
    }

    /// Consume into the row vector only.
    pub fn into_rows(self) -> Vec<Tuple> {
        self.rows
    }

    /// Attach a follow-on page (pagination chain). Does not re-prove arity —
    /// the page was already admitted through [`Self::try_new`].
    pub fn with_next(mut self, next: Option<Box<NamedRows>>) -> Self {
        self.next = next;
        self
    }

    /// Render as the envelope every binding's success path returns:
    /// `{"headers": [...], "rows": [[...], ...], "next": null | <nested
    /// envelope>}`.
    ///
    /// Refuses when any cell is a non-finite [`DataValue::Num`] (or a nested
    /// value that cannot encode honestly as JSON) — never Null/Str-remaps.
    pub fn into_json(self) -> Result<JsonValue, NonFiniteJsonNumber> {
        let (headers, rows, next) = self.into_parts();
        let next = match next {
            None => JsonValue::Null,
            Some(more) => more.into_json()?,
        };
        let rows: Vec<JsonValue> = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(JsonValue::try_from)
                    .collect::<Result<Vec<_>, _>>()
                    .map(JsonValue::Array)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({
            "headers": headers,
            "rows": rows,
            "next": next,
        }))
    }

    /// Encode this page as a self-contained Arrow IPC stream via
    /// [`kyzo_model::envelope::arrow`] (Schema + one RecordBatch + EOS).
    pub fn to_arrow_ipc(&self) -> miette::Result<Vec<u8>> {
        let batch = kyzo_model::envelope::arrow::ColumnBatch::from_rows(
            self.rows.clone(),
            self.headers.len(),
        )
        .map_err(|e| miette::miette!("{e}"))?;
        let names: Vec<&str> = self.headers.iter().map(String::as_str).collect();
        kyzo_model::envelope::arrow::encode_stream(&batch, &names)
    }

    /// Parse the same envelope back into a `NamedRows`.
    pub fn from_json(v: &JsonValue) -> miette::Result<Self> {
        let headers = v
            .get("headers")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| miette::miette!("NamedRows JSON requires a 'headers' array"))?
            .iter()
            .map(|h| {
                h.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| miette::miette!("'headers' must be an array of strings"))
            })
            .collect::<miette::Result<Vec<_>>>()?;
        let rows = v
            .get("rows")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| miette::miette!("NamedRows JSON requires a 'rows' array"))?
            .iter()
            .map(|row| {
                row.as_array()
                    .map(|r| r.iter().map(DataValue::from).collect())
                    .ok_or_else(|| miette::miette!("'rows' must be an array of arrays"))
            })
            .collect::<miette::Result<Vec<_>>>()?;
        Ok(NamedRows::try_new(headers, rows)?)
    }
}

impl IntoIterator for NamedRows {
    type Item = Tuple;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.into_iter()
    }
}

/// Why rendering a diagnostic envelope failed.
#[derive(Debug, Error)]
pub enum FormatErrorDiag {
    #[error("render text error failed: {0}")]
    TextRender(#[source] std::fmt::Error),
    #[error("render json error failed: {0}")]
    JsonRender(#[source] std::fmt::Error),
    #[error("parse rendered json error failed: {0}")]
    JsonParse(#[source] serde_json::Error),
    #[error("miette JSON report was not a JSON object")]
    NotObject,
}

pub fn try_format_error_as_json(
    mut err: Report,
    source: Option<&str>,
) -> std::result::Result<JsonValue, FormatErrorDiag> {
    if err.source_code().is_none()
        && let Some(src) = source
    {
        err = err.with_source_code(format!("{src} "));
    }
    let mut text_err = String::new();
    let mut json_err = String::new();
    TEXT_ERR_HANDLER
        .render_report(&mut text_err, err.as_ref())
        .map_err(FormatErrorDiag::TextRender)?;
    JSON_ERR_HANDLER
        .render_report(&mut json_err, err.as_ref())
        .map_err(FormatErrorDiag::JsonRender)?;
    let mut json: JsonValue =
        serde_json::from_str(&json_err).map_err(FormatErrorDiag::JsonParse)?;
    let map = json.as_object_mut().ok_or(FormatErrorDiag::NotObject)?;
    map.insert("ok".to_string(), json!(false));
    map.insert("message".to_string(), json!(err.to_string()));
    map.insert("display".to_string(), json!(text_err));
    Ok(json)
}

pub fn format_error_as_json(err: Report, source: Option<&str>) -> JsonValue {
    let message = err.to_string();
    match try_format_error_as_json(err, source) {
        Ok(json) => json,
        Err(_) => json!({
            "ok": false,
            "message": message,
            "display": message,
        }),
    }
}

static TEXT_ERR_HANDLER: LazyLock<GraphicalReportHandler> = LazyLock::new(|| {
    GraphicalReportHandler::new().with_theme(GraphicalTheme {
        characters: ThemeCharacters::unicode(),
        styles: ThemeStyles::ansi(),
    })
});
static JSON_ERR_HANDLER: LazyLock<JSONReportHandler> = LazyLock::new(JSONReportHandler::new);

#[cfg(test)]
mod tests {
    use super::*;

    /// `try_new` proves header↔row arity (P082).
    #[test]
    fn named_rows_try_new_proves_arity() -> miette::Result<()> {
        assert!(
            NamedRows::try_new(
                vec!["a".into(), "b".into()],
                vec![Tuple::from_vec(vec![DataValue::from(1)])],
            )
            .is_err()
        );
        let ok = NamedRows::try_new(
            vec!["a".into()],
            vec![Tuple::from_vec(vec![DataValue::from(1)])],
        )?;
        assert_eq!(ok.headers().len(), 1);
        assert_eq!(ok.rows().len(), 1);
        Ok(())
    }
}
