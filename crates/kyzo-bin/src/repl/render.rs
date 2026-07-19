/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Rendering one query result for a human: a table on stdout, or — when
//! `%save <path>` armed it — a JSON file of `{column: value}` records. The
//! one place in the REPL that turns a [`NamedRows`] into something shown to
//! the user (as opposed to [`crate::client`]'s or [`crate::relations`]'s
//! machine-to-machine JSON, which goes through kyzo-core's own envelope).

use std::fs::File;
use std::io::Write;

use kyzo::NamedRows;
use miette::{IntoDiagnostic, Result};
use serde_json::Value;

/// Render `out`: to the file `%save` armed (consuming that arm), or as a
/// table on stdout.
pub(super) fn render(out: NamedRows, save_next: &mut Option<String>) -> Result<()> {
    match save_next.take() {
        Some(path) => save_to_file(&out, &path),
        None => {
            print_table(&out);
            Ok(())
        }
    }
}

fn save_to_file(out: &NamedRows, path: &str) -> Result<()> {
    println!(
        "Query has returned {} rows, saving to file {path}",
        out.rows().len()
    );
    let records: Vec<Value> = out
        .rows()
        .iter()
        .map(|row| -> Value {
            row.iter()
                .zip(out.headers().iter())
                .map(|(v, k)| (k.to_string(), Value::from(v)))
                .collect()
        })
        .collect();
    let mut file = File::create(path).into_diagnostic()?;
    file.write_all(Value::Array(records).to_string().as_bytes())
        .into_diagnostic()
}

fn print_table(out: &NamedRows) {
    use prettytable::format;
    let mut table = prettytable::Table::new();
    let headers = out
        .headers()
        .iter()
        .map(prettytable::Cell::from)
        .collect::<Vec<_>>();
    table.set_titles(prettytable::Row::new(headers));
    for row in out.rows() {
        let cells = row.iter().map(|c| format!("{c}")).collect::<Vec<_>>();
        let cells = cells.iter().map(prettytable::Cell::from).collect();
        table.add_row(prettytable::Row::new(cells));
    }
    table.set_format(*format::consts::FORMAT_NO_BORDER_LINE_SEPARATOR);
    table.printstd();
}
