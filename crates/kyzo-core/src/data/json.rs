/*
 * Copyright 2022, The Cozo Project Authors. / Copyright 2026, The KyzoDB Authors.
 * MPL-2.0.
 */

//! Host JSON envelopes that need `NamedRows` / miette rendering.
//! `DataValue` <-> JSON conversions live in `kyzo_model::envelope::json`.

use miette::{
    GraphicalReportHandler, GraphicalTheme, JSONReportHandler, Report, ThemeCharacters,
    ThemeStyles,
};
pub use serde_json::Value as JsonValue;
use serde_json::json;
use std::sync::LazyLock;
use thiserror::Error;

use kyzo_model::value::DataValue;
use crate::fixed_rule::NamedRows;

pub use kyzo_model::envelope::json::{
    JsonData, json_from_serde, json_to_datavalue, serde_from_json,
};

impl NamedRows {
    /// Render as the envelope every binding's success path returns:
    /// `{"headers": [...], "rows": [[...], ...], "next": null | <nested
    /// envelope>}`.
    pub fn into_json(self) -> JsonValue {
        let (headers, rows, next) = self.into_parts();
        let next = match next {
            None => JsonValue::Null,
            Some(more) => more.into_json(),
        };
        let rows: Vec<JsonValue> = rows
            .into_iter()
            .map(|row| JsonValue::Array(row.into_iter().map(JsonValue::from).collect()))
            .collect();
        json!({
            "headers": headers,
            "rows": rows,
            "next": next,
        })
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
