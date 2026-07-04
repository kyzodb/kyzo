/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The KyzoScript language server (story #92): the delivery of story #73's
//! designed diagnostics, live in the editor instead of only after a real
//! run. Every `textDocument/didOpen`/`didChange` re-validates the document
//! through [`kyzo::lsp_api::check_script`] — the same `ParseError`/
//! `AggrNotFound`/`OptionNotConstantError`/… surface #73 redesigned — and
//! publishes the result as LSP `Diagnostic`s: span becomes `Range`, the
//! error's `Display` plus its `#[help]` become `message`, and the
//! diagnostic `code` carries the same `parser::…` code the CLI shows.
//!
//! Transport is hand-rolled stdio JSON-RPC (`Content-Length` framing, no
//! async runtime): diagnostics-on-type is synchronous, single-document
//! work, and framing is small enough on its own that pulling in
//! `tower-lsp`'s async-trait service machinery buys nothing for a first
//! cut whose only hard requirement is diagnostics live end to end. Message
//! *shapes* are not hand-rolled — every request/notification param and the
//! `Diagnostic`/`Range`/`Position` types are `lsp-types`, the same crate
//! rust-analyzer uses, so this server speaks the real protocol, not an
//! approximation of it.
//!
//! Scope of this first cut: diagnostics-on-type only (`initialize` /
//! `initialized` / `didOpen` / `didChange` / `shutdown` / `exit`). Hover,
//! go-to-definition, and completion are the DoD's next pieces and are not
//! started here.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use lsp_types::{
    Diagnostic, DiagnosticSeverity, InitializeResult, NumberOrString, Position,
    PublishDiagnosticsParams, Range, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri,
};
use serde_json::{Value, json};

// ─────────────────────────────────────────────────────────────────────────
// Wire transport: Content-Length-framed JSON-RPC over stdio, per the LSP
// base protocol. No message content is hand-rolled (that's `lsp-types`'
// job) -- only the header/body framing around it.
// ─────────────────────────────────────────────────────────────────────────

/// Read one framed JSON-RPC message from `reader`: a run of `Header: value`
/// lines (only `Content-Length` matters; others, e.g. `Content-Type`, are
/// skipped) ended by a blank line, then exactly `Content-Length` bytes of
/// UTF-8 JSON. `Ok(None)` at a clean EOF between messages (the client
/// closed stdin); an EOF mid-message is an error, not a silent `None`, so a
/// truncated stream is diagnosable instead of read as a quiet shutdown.
fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return if content_length.is_none() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended mid-header",
                ))
            };
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break; // blank line: header section over, body follows
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "bad Content-Length header")
            })?);
        }
    }
    let content_length = content_length
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?;
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let value: Value =
        serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(value))
}

/// Write one JSON-RPC message, framed the same way [`read_message`] reads
/// one. `writer.flush()` matters: stdout is line-buffered by default in
/// some environments and this is not line-terminated JSON.
fn write_message(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

// ─────────────────────────────────────────────────────────────────────────
// Byte offset -> LSP `Position`. LSP positions are UTF-16 code units
// (`PositionEncodingKind::UTF16`, the default every client must support),
// not bytes and not Unicode scalar values -- a span past the first
// non-ASCII character would render at the wrong column in the editor
// otherwise, silently, for exactly the scripts most likely to exercise the
// escape/codepoint diagnostics (#73's `InvalidUtf8Error`/
// `InvalidEscapeSeqError`) that carry non-ASCII spans.
// ─────────────────────────────────────────────────────────────────────────

struct LineIndex {
    /// Byte offset of the start of each line (line 0's start, always 0, is
    /// the first entry).
    line_starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        LineIndex { line_starts }
    }

    /// The `Position` for `byte_offset` into the same `text` this index was
    /// built from. Clamps past-the-end offsets to the document's end
    /// rather than panicking: a span is proven in-bounds against the
    /// *parser's* view of the text, but nothing re-proves that here, and a
    /// clamp is a wrong-looking diagnostic, never a crashed server.
    fn position(&self, text: &str, byte_offset: usize) -> Position {
        let byte_offset = byte_offset.min(text.len());
        let line = self
            .line_starts
            .binary_search(&byte_offset)
            .unwrap_or_else(|insert_at| insert_at - 1);
        let line_start = self.line_starts[line];
        let character = text[line_start..byte_offset].encode_utf16().count() as u32;
        Position::new(line as u32, character)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// miette::Report -> LSP Diagnostic[]: the redesigned #73 surface, live.
// ─────────────────────────────────────────────────────────────────────────

/// One [`Diagnostic`] per label in `err`'s tree (its own labels, then every
/// `#[related]` cause and `diagnostic_source`, walked recursively — the
/// same shape `parse::fuzz_tests::walk_labels` proves every parser error
/// satisfies, just walked here from the public `Diagnostic` trait instead
/// of that crate-internal test helper). Every diagnostic shares one
/// message: the error's own `Display`, plus its `#[help]` text appended
/// when present, so the mechanical fix #73 wrote for a SQL-shaped mistake
/// (or any other designed hint) shows up in the editor, not just at the
/// CLI. Falls back to a single diagnostic at the document's start if the
/// error tree carries no label anywhere (defensive: every error the parse
/// tier itself raises is spanned, per #73's law test, but `check_script`
/// can also surface a non-parser failure, e.g. the system clock, with no
/// span to give).
fn diagnostics_from_report(err: &miette::Report, text: &str, index: &LineIndex) -> Vec<Diagnostic> {
    let message = match err.help() {
        Some(help) => format!("{err}\n\nhelp: {help}"),
        None => err.to_string(),
    };
    let code = err.code().map(|c| NumberOrString::String(c.to_string()));

    let mut out = Vec::new();
    collect_labels(err.as_ref(), &message, &code, text, index, &mut out);
    if out.is_empty() {
        out.push(Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 0)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("kyzoscript".to_string()),
            message,
            code,
            ..Diagnostic::default()
        });
    }
    out
}

fn collect_labels(
    diag: &dyn miette::Diagnostic,
    message: &str,
    code: &Option<NumberOrString>,
    text: &str,
    index: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    if let Some(labels) = diag.labels() {
        for label in labels {
            let start = index.position(text, label.offset());
            let end = index.position(text, label.offset() + label.len());
            out.push(Diagnostic {
                range: Range::new(start, end),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("kyzoscript".to_string()),
                message: message.to_string(),
                code: code.clone(),
                ..Diagnostic::default()
            });
        }
    }
    if let Some(related) = diag.related() {
        for r in related {
            collect_labels(r, message, code, text, index, out);
        }
    }
    if let Some(src) = diag.diagnostic_source() {
        collect_labels(src, message, code, text, index, out);
    }
}

/// Validate `text` and turn the result into the `Diagnostic`s to publish:
/// empty on success (clearing any previously-published diagnostics for
/// this document is just as important as reporting new ones), one or more
/// on failure.
fn validate(text: &str) -> Vec<Diagnostic> {
    match kyzo::lsp_api::check_script(text, &Default::default()) {
        Ok(()) => Vec::new(),
        Err(report) => diagnostics_from_report(&report, text, &LineIndex::new(text)),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The server loop.
// ─────────────────────────────────────────────────────────────────────────

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    // Open documents, by URI (as its string form -- `Uri` isn't `Hash`, and
    // the string form is exactly what round-trips through every message).
    let mut open_docs: HashMap<String, String> = HashMap::new();

    loop {
        let Some(msg) = read_message(&mut reader)? else {
            return Ok(()); // client closed stdin without an `exit`: shut down quietly
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();

        match method {
            "initialize" => {
                let result = InitializeResult {
                    capabilities: ServerCapabilities {
                        text_document_sync: Some(TextDocumentSyncCapability::Kind(
                            TextDocumentSyncKind::FULL,
                        )),
                        ..ServerCapabilities::default()
                    },
                    server_info: Some(ServerInfo {
                        name: "kyzo-lsp".to_string(),
                        version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    }),
                };
                if let Some(id) = id {
                    write_message(&mut writer, &response(id, serde_json::to_value(result)?))?;
                }
            }
            "initialized" => {} // no reply; nothing to do until a document opens
            "textDocument/didOpen" => {
                if let Some(params) = msg.get("params") {
                    let uri = params["textDocument"]["uri"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let text = params["textDocument"]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    publish(&mut writer, &uri, &text)?;
                    open_docs.insert(uri, text);
                }
            }
            "textDocument/didChange" => {
                if let Some(params) = msg.get("params") {
                    let uri = params["textDocument"]["uri"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    // Full-document sync only (declared above): the last
                    // content change IS the whole new document, never a
                    // range patch to apply.
                    if let Some(text) = params["contentChanges"]
                        .as_array()
                        .and_then(|changes| changes.last())
                        .and_then(|change| change["text"].as_str())
                    {
                        let text = text.to_string();
                        publish(&mut writer, &uri, &text)?;
                        open_docs.insert(uri, text);
                    }
                }
            }
            "textDocument/didClose" => {
                if let Some(params) = msg.get("params") {
                    let uri = params["textDocument"]["uri"].as_str().unwrap_or_default();
                    open_docs.remove(uri);
                    // Clear diagnostics for a document the editor no longer
                    // has open, rather than leaving stale squiggles behind.
                    if let Ok(parsed_uri) = uri.parse::<Uri>() {
                        let clear = PublishDiagnosticsParams::new(parsed_uri, Vec::new(), None);
                        write_message(
                            &mut writer,
                            &notification(
                                "textDocument/publishDiagnostics",
                                serde_json::to_value(clear)?,
                            ),
                        )?;
                    }
                }
            }
            "shutdown" => {
                if let Some(id) = id {
                    write_message(&mut writer, &response(id, Value::Null))?;
                }
            }
            "exit" => return Ok(()),
            _ => {} // an unhandled request/notification: ignored, not an error
        }
    }
}

fn publish(writer: &mut impl Write, uri: &str, text: &str) -> io::Result<()> {
    let diagnostics = validate(text);
    let Ok(parsed_uri) = uri.parse::<Uri>() else {
        return Ok(()); // an unparseable URI is the client's bug, not ours to crash over
    };
    let params = PublishDiagnosticsParams::new(parsed_uri, diagnostics, None);
    write_message(
        writer,
        &notification(
            "textDocument/publishDiagnostics",
            serde_json::to_value(params)?,
        ),
    )
}
