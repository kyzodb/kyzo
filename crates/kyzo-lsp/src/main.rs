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
//! through kyzo-model's public parse surface — the same `ParseError`/
//! `AggrNotFound`/`OptionNotConstantError`/… surface #73 redesigned — and
//! publishes the result as LSP `Diagnostic`s via [`translate`]: span becomes
//! `Range`, the error's `Display` plus its `#[help]` become `message`, and
//! the diagnostic `code` carries the same `parser::…` code the CLI shows.
//!
//! When formatting lands it must call kyzo-model's format door (the one
//! canonical pretty-printer), never a local one.
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
//! Scope: diagnostics-on-type, plus catalog-aware hover and completion.
//! `initialize`'s `initializationOptions.dbPath`, when the client supplies
//! one, opens a real on-disk `Engine` (the same `fjall` backend every other
//! entry point uses) so hover-over-a-relation and completion can answer
//! from the connected store's actual catalog (`::relations`/`::columns`,
//! the same sys-ops the CLI's `\d`-style introspection uses) — not a
//! separately-maintained shadow of it. Without a `dbPath`, completion is
//! empty (no hand-copied keyword/aggregation lists) and hover resolves
//! aggregations only through [`kyzo_model::program::parse_aggr`] — the same
//! admission door the parser uses — rather than a second vocabulary.
//!
//! Go-to-definition is not offered here: a second hand-rolled bracket grammar
//! was deleted; definition jumps must wait on a real parse-surface door that
//! can answer mid-edit documents, not a local mini-lexer.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, Write};

use kyzo::{Catalog, Engine, FjallStorage, new_fjall_storage};
use kyzo_model::program::parse_aggr;
use kyzo_model::{DataValue, ValidityTs};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, Diagnostic, Hover, HoverContents,
    HoverProviderCapability, InitializeResult, MarkupContent, MarkupKind, Position,
    PublishDiagnosticsParams, Range, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri,
};
use serde_json::{Value, json};

mod translate;
use translate::{LineIndex, diagnostics_from_report, word_at};

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

/// Validate `text` and turn the result into the `Diagnostic`s to publish:
/// empty on success (clearing any previously-published diagnostics for
/// this document is just as important as reporting new ones), one or more
/// on failure. Speaks kyzo-model's public parse surface — the language
/// door — never an engine host façade.
fn validate(text: &str) -> Vec<Diagnostic> {
    let params = BTreeMap::<String, DataValue>::new();
    // Live session stamp for `@` / `@ NOW` — wall-clock micros, never the
    // from_raw(0) open-end sentinel the parse door forbids.
    let cur_vld = ValidityTs::from_raw(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(1),
    );
    match kyzo_model::parse::parse_script(text, &params, cur_vld) {
        Ok(_) => Vec::new(),
        Err(report) => diagnostics_from_report(&report, text, &LineIndex::new(text)),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Catalog-aware hover and completion. Relation/column facts come from
// `::relations`/`::columns` against the connected store. Aggregation hover
// admits names through `parse_aggr` (the model declaration door) — never a
// hand-copied name/doc table. Keyword/aggregation completion lists are gone;
// without a catalog, completion is empty rather than a second vocabulary.
// ─────────────────────────────────────────────────────────────────────────

/// Open the `Engine` an editor session's catalog features answer from, if the
/// client supplied one. `initializationOptions.dbPath` is the only source
/// consulted — no guessing from `rootUri`, since pointing an LSP session at
/// the wrong on-disk store (or silently creating one nobody asked for) is
/// a worse failure mode than "no catalog features this session."
fn open_catalog_db(initialize_params: &Value) -> Option<Engine<FjallStorage>> {
    let db_path = initialize_params
        .get("initializationOptions")?
        .get("dbPath")?
        .as_str()?;
    let storage = new_fjall_storage(db_path).ok()?;
    Engine::compose(storage, Catalog::new()).ok()
}

/// `::relations`' rows as `(name, arity)` pairs.
fn list_relations(db: &Engine<FjallStorage>) -> Vec<(String, i64)> {
    let Ok(rows) = db.run_script("::relations", Default::default()) else {
        return Vec::new();
    };
    rows.rows()
        .iter()
        .filter_map(|row| Some((row.first()?.get_str()?.to_string(), row.get(1)?.get_int()?)))
        .collect()
}

/// `::columns <name>`'s rows as `(column name, is_key)` pairs, or `None` if
/// `name` isn't a relation the store knows (a hover-worthy fact on its
/// own, but the caller decides what to do with "no such relation").
fn columns_for_relation(db: &Engine<FjallStorage>, name: &str) -> Option<Vec<(String, bool)>> {
    // `name` is the word the editor's cursor is touching, so it's already
    // identifier-shaped (`word_at`'s whole contract) -- never
    // interpolated from arbitrary text, but a relation name can still
    // collide with a reserved word (`create`, say); `::columns` itself is
    // the authority on whether that succeeds, not a lookalike check here.
    let script = format!("::columns {name}");
    let rows = db.run_script(&script, Default::default()).ok()?;
    Some(
        rows.rows()
            .iter()
            .filter_map(|row| Some((row.first()?.get_str()?.to_string(), row.get(1)?.get_bool()?)))
            .collect(),
    )
}

fn completion_items(db: Option<&Engine<FjallStorage>>) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    if let Some(db) = db {
        for (name, arity) in list_relations(db) {
            items.push(CompletionItem {
                label: name,
                kind: Some(CompletionItemKind::CLASS),
                detail: Some(format!("relation, arity {arity}")),
                ..Default::default()
            });
        }
    }
    items
}

/// A markdown hover for the word at `position` in `text`: the connected
/// store's columns if it names a relation, an aggregation admitted by
/// [`parse_aggr`] if it names one, or `None` (no hover) otherwise —
/// including "no catalog is connected", rather than guessing.
fn hover_at(db: Option<&Engine<FjallStorage>>, text: &str, position: Position) -> Option<Hover> {
    let index = LineIndex::new(text);
    let byte_offset = index.offset(text, position);
    let (word, start, end) = word_at(text, byte_offset)?;

    let markdown = match parse_aggr(word) {
        Ok(Some(aggr)) => {
            let kind = if aggr.is_meet() { "meet" } else { "normal" };
            format!("**{word}** (aggregation, {kind})")
        }
        Err(refuse) => format!("**{word}** (aggregation refused)\n\n{refuse}"),
        Ok(None) => {
            let columns = db.and_then(|db| columns_for_relation(db, word))?;
            let mut body = format!("**{word}** (relation)\n\n| column | key |\n|---|---|\n");
            for (col, is_key) in &columns {
                body.push_str(&format!(
                    "| {col} | {} |\n",
                    if *is_key { "yes" } else { "" }
                ));
            }
            body
        }
    };

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(Range::new(
            index.position(text, start),
            index.position(text, end),
        )),
    })
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
    // The connected store, if `initialize` named one -- `None` throughout
    // the session otherwise, which every catalog-backed handler already
    // treats as "degrade, don't fail".
    let mut db: Option<Engine<FjallStorage>> = None;

    loop {
        let Some(msg) = read_message(&mut reader)? else {
            return Ok(()); // client closed stdin without an `exit`: shut down quietly
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();

        match method {
            "initialize" => {
                if let Some(params) = msg.get("params") {
                    db = open_catalog_db(params);
                }
                let result = InitializeResult {
                    capabilities: ServerCapabilities {
                        text_document_sync: Some(TextDocumentSyncCapability::Kind(
                            TextDocumentSyncKind::FULL,
                        )),
                        hover_provider: Some(HoverProviderCapability::Simple(true)),
                        completion_provider: Some(CompletionOptions::default()),
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
            "textDocument/completion" => {
                if let Some(id) = id {
                    let items = completion_items(db.as_ref());
                    write_message(&mut writer, &response(id, serde_json::to_value(items)?))?;
                }
            }
            "textDocument/hover" => {
                if let Some(id) = id {
                    let hover = msg.get("params").and_then(|params| {
                        let uri = params["textDocument"]["uri"].as_str()?;
                        let text = open_docs.get(uri)?;
                        let position: Position =
                            serde_json::from_value(params["position"].clone()).ok()?;
                        hover_at(db.as_ref(), text, position)
                    });
                    write_message(&mut writer, &response(id, serde_json::to_value(hover)?))?;
                }
            }
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
