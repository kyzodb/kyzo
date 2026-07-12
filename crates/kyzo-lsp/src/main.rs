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
//! Scope: diagnostics-on-type, plus catalog-aware hover and completion.
//! `initialize`'s `initializationOptions.dbPath`, when the client supplies
//! one, opens a real on-disk `Db` (the same `fjall` backend every other
//! entry point uses) so hover-over-a-relation and completion can answer
//! from the connected store's actual catalog (`::relations`/`::columns`,
//! the same sys-ops the CLI's `\d`-style introspection uses) — not a
//! separately-maintained shadow of it. Without a `dbPath`, both features
//! degrade to their catalog-free form (keyword/aggregation completion,
//! aggregation-only hover) rather than failing.
//!
//! Go-to-definition jumps from a rule reference to that rule's own head
//! within the SAME document (a stored relation's "definition" is catalog
//! data with no source location to jump to, so `*rel` names never resolve
//! here — only bare, unsigiled `name[...]` references do). It is
//! deliberately lexical (a bracket-matching scan for `ident[…]` shapes,
//! not a real parse) rather than AST-based: the document being edited is
//! often mid-keystroke and does not parse at all, and a feature that only
//! works on valid documents reads as broken to an editor user.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use kyzo::{Db, FjallStorage, new_fjall_storage};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, Diagnostic, DiagnosticSeverity, Hover,
    HoverContents, HoverProviderCapability, InitializeResult, MarkupContent, MarkupKind,
    NumberOrString, OneOf, Position, PublishDiagnosticsParams, Range, ServerCapabilities,
    ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
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

    /// The inverse of [`LineIndex::position`]: the byte offset a `Position`
    /// (UTF-16 code units into its line) names. Walks the line's chars
    /// counting UTF-16 units rather than assuming 1 unit == 1 byte, for the
    /// same reason `position` above doesn't assume it either. Clamps a
    /// character past the line's real length to the line's end (the LSP
    /// spec's own rule for an over-long position, not an error).
    fn offset(&self, text: &str, position: Position) -> usize {
        let line = position.line as usize;
        let Some(&line_start) = self.line_starts.get(line) else {
            return text.len();
        };
        let line_end = self
            .line_starts
            .get(line + 1)
            .map_or(text.len(), |&next| next.saturating_sub(1).max(line_start));
        let line_text = &text[line_start..line_end.min(text.len())];
        let mut units = 0u32;
        for (byte_idx, ch) in line_text.char_indices() {
            if units >= position.character {
                return line_start + byte_idx;
            }
            units += ch.len_utf16() as u32;
        }
        line_start + line_text.len()
    }
}

/// The identifier-shaped word touching `offset` in `text` — the token a
/// hover or completion request is "about" — plus its own byte range,
/// widening left and right from `offset` while the character is a
/// KyzoScript identifier character (`[A-Za-z0-9_]`; good enough for a
/// relation/aggregation name, which is all hover/completion resolve
/// against here). `None` if `offset` doesn't touch such a word at all
/// (whitespace, punctuation, end of document).
fn word_at(text: &str, offset: usize) -> Option<(&str, usize, usize)> {
    fn is_word_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_'
    }
    let offset = offset.min(text.len());
    let mut start = offset;
    while start > 0 {
        let prev = text[..start].chars().next_back()?;
        if !is_word_char(prev) {
            break;
        }
        start -= prev.len_utf8();
    }
    let mut end = offset;
    while end < text.len() {
        let next = text[end..].chars().next()?;
        if !is_word_char(next) {
            break;
        }
        end += next.len_utf8();
    }
    if start == end {
        None
    } else {
        Some((&text[start..end], start, end))
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
// Catalog-aware hover and completion. Every relation/column fact here comes
// from actually running `::relations`/`::columns` against the connected
// store through the same public `Db::run_script` every other caller uses —
// no shadow catalog kept in sync by hand.
// ─────────────────────────────────────────────────────────────────────────

/// The built-in aggregations, for completion and hover when no catalog (or
/// no matching relation) applies. Mirrors `parse::query::COMMON_AGGR_NAMES`
/// in spirit (that list is crate-internal, so this one can't just reuse
/// it) — a drift between the two would weaken a hint or a completion
/// suggestion, never misreport what the engine actually accepts, since
/// `parse_aggr` (`data/aggr.rs`) alone decides that.
const AGGREGATIONS: &[(&str, &str)] = &[
    ("count", "the number of rows in the group"),
    ("count_unique", "the number of distinct values in the group"),
    ("sum", "the sum of the group's values"),
    ("product", "the product of the group's values"),
    ("mean", "the arithmetic mean of the group's values"),
    ("variance", "the sample variance of the group's values"),
    (
        "std_dev",
        "the sample standard deviation of the group's values",
    ),
    ("min", "the smallest value in the group"),
    ("max", "the largest value in the group"),
    ("unique", "the distinct values in the group, as a list"),
    (
        "collect",
        "every value in the group, as a list, in derivation order",
    ),
    (
        "group_count",
        "counts per distinct value, as a list of [value, count] pairs",
    ),
    ("union", "the union of the group's set-valued values"),
    (
        "intersection",
        "the intersection of the group's set-valued values",
    ),
    (
        "choice",
        "one value from the group, chosen deterministically",
    ),
    ("choice_rand", "one value from the group, chosen at random"),
    ("shortest", "the shortest value (by length) in the group"),
    ("min_cost", "the value with the smallest associated cost"),
    ("bit_and", "the bitwise AND of the group's integer values"),
    ("bit_or", "the bitwise OR of the group's integer values"),
    ("bit_xor", "the bitwise XOR of the group's integer values"),
    ("latest_by", "the value associated with the largest key"),
    ("smallest_by", "the value associated with the smallest key"),
];

/// The imperative and relation-op keywords, for completion. Not every
/// grammar keyword (`kyzoscript.pest` has ~30 of these) — the ones a
/// newcomer actually reaches for while typing.
const KEYWORDS: &[&str] = &[
    "not",
    "or",
    "and",
    "in",
    ":create",
    ":put",
    ":insert",
    ":update",
    ":rm",
    ":replace",
    ":ensure",
    ":ensure_not",
    ":limit",
    ":offset",
    ":sort",
    ":order",
    ":timeout",
    ":sleep",
    "%if",
    "%then",
    "%else",
    "%end",
    "%loop",
    "%break",
    "%continue",
    "%return",
];

/// Open the `Db` an editor session's catalog features answer from, if the
/// client supplied one. `initializationOptions.dbPath` is the only source
/// consulted — no guessing from `rootUri`, since pointing an LSP session at
/// the wrong on-disk store (or silently creating one nobody asked for) is
/// a worse failure mode than "no catalog features this session."
fn open_catalog_db(initialize_params: &Value) -> Option<Db<FjallStorage>> {
    let db_path = initialize_params
        .get("initializationOptions")?
        .get("dbPath")?
        .as_str()?;
    let storage = new_fjall_storage(db_path).ok()?;
    Db::new(storage).ok()
}

/// `::relations`' rows as `(name, arity)` pairs.
fn list_relations(db: &Db<FjallStorage>) -> Vec<(String, i64)> {
    let Ok(rows) = db.run_script("::relations", Default::default()) else {
        return Vec::new();
    };
    rows.rows
        .iter()
        .filter_map(|row| Some((row.first()?.get_str()?.to_string(), row.get(1)?.get_int()?)))
        .collect()
}

/// `::columns <name>`'s rows as `(column name, is_key)` pairs, or `None` if
/// `name` isn't a relation the store knows (a hover-worthy fact on its
/// own, but the caller decides what to do with "no such relation").
fn columns_for_relation(db: &Db<FjallStorage>, name: &str) -> Option<Vec<(String, bool)>> {
    // `name` is the word the editor's cursor is touching, so it's already
    // identifier-shaped (`word_at`'s whole contract) -- never
    // interpolated from arbitrary text, but a relation name can still
    // collide with a reserved word (`create`, say); `::columns` itself is
    // the authority on whether that succeeds, not a lookalike check here.
    let script = format!("::columns {name}");
    let rows = db.run_script(&script, Default::default()).ok()?;
    Some(
        rows.rows
            .iter()
            .filter_map(|row| Some((row.first()?.get_str()?.to_string(), row.get(1)?.get_bool()?)))
            .collect(),
    )
}

fn completion_items(db: Option<&Db<FjallStorage>>) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    for (name, doc) in AGGREGATIONS {
        items.push(CompletionItem {
            label: (*name).to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some("aggregation".to_string()),
            documentation: Some(lsp_types::Documentation::String((*doc).to_string())),
            ..Default::default()
        });
    }
    for keyword in KEYWORDS {
        items.push(CompletionItem {
            label: (*keyword).to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
    }
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
/// store's columns if it names a relation, an aggregation's one-line
/// description if it names one of those, or `None` (no hover) otherwise —
/// including "no catalog is connected", rather than guessing.
fn hover_at(db: Option<&Db<FjallStorage>>, text: &str, position: Position) -> Option<Hover> {
    let index = LineIndex::new(text);
    let byte_offset = index.offset(text, position);
    let (word, start, end) = word_at(text, byte_offset)?;

    let markdown = if let Some((_, doc)) = AGGREGATIONS.iter().find(|(name, _)| *name == word) {
        format!("**{word}** (aggregation)\n\n{doc}")
    } else {
        let columns = db.and_then(|db| columns_for_relation(db, word))?;
        let mut body = format!("**{word}** (relation)\n\n| column | key |\n|---|---|\n");
        for (col, is_key) in &columns {
            body.push_str(&format!(
                "| {col} | {} |\n",
                if *is_key { "yes" } else { "" }
            ));
        }
        body
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
// Go-to-definition: a rule reference in the SAME document jumps to that
// rule's own head. This is deliberately lexical, not AST-based, and that
// is the right call here, not a shortcut: the document being edited is
// frequently mid-keystroke and does not parse at all, and a
// go-to-definition that only works on valid documents reads as broken to
// an editor user. The grammar fact this leans on is narrow and stable —
// every rule head and every rule application is the shape
// `ident ~ "[" ~ … ~ "]"` (`rule_head`/`rule_apply` in kyzoscript.pest) —
// so a scan that finds `ident[…]` shapes and asks "is `:=`/`<-`/`<~`
// immediately next?" identifies exactly the same definitions the real
// parser would, without needing one. It does not resolve a relation name
// (`*rel`, catalog data with no source location) or anything inside a
// `$param`/`~search:index` sigil — `word_at`'s own scope already excludes
// those tokens.
// ─────────────────────────────────────────────────────────────────────────

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The byte offset of the bracket matching the `[` at `open`, treating
/// nested `[`/`{`/`(` and skipping over string and comment content so a
/// `]`/`#`/`"` inside one can't be mistaken for structure — the same class
/// of mini-lexer `parse::reject_excessive_nesting` uses on the engine side
/// for the identical reason (this file has no access to that crate-
/// internal scanner, so it earns its own narrow copy of the same idea).
/// `None` on unbalanced brackets or an unterminated string/comment: a
/// document mid-edit is exactly where those happen, and "no definition
/// found" is the right answer, not a guess.
fn matching_bracket(text: &str, open: usize) -> Option<usize> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    // `open`'s own position in `chars`, so the scan starts at the char
    // right after it.
    let open_idx = chars.iter().position(|&(i, _)| i == open)?;
    let mut stack = vec![']'];
    let mut i = open_idx + 1;
    while i < chars.len() {
        let (at, c) = chars[i];
        match c {
            '#' => {
                while i < chars.len() && chars[i].1 != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1).map(|&(_, c)| c) == Some('*') => {
                i += 2;
                loop {
                    if i >= chars.len() {
                        return None; // unterminated block comment
                    }
                    if chars[i].1 == '*' && chars.get(i + 1).map(|&(_, c)| c) == Some('/') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            '"' | '\'' => {
                let quote = c;
                i += 1;
                loop {
                    let Some(&(_, c)) = chars.get(i) else {
                        return None; // unterminated string
                    };
                    i += 1;
                    if c == '\\' {
                        i += 1; // an escaped char, whatever it is, isn't the closing quote
                    } else if c == quote {
                        break;
                    }
                }
                continue;
            }
            '[' => stack.push(']'),
            '{' => stack.push('}'),
            '(' => stack.push(')'),
            ']' | '}' | ')' => {
                if stack.last() == Some(&c) {
                    stack.pop();
                    if stack.is_empty() {
                        return Some(at);
                    }
                } else {
                    return None; // mismatched bracket: don't guess
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Every rule NAME's own definition site in `text`: an identifier
/// immediately (whitespace aside) followed by a bracketed head whose
/// matching close is immediately (whitespace aside) followed by `:=`,
/// `<-`, or `<~`. Keyed by name, spanned at the identifier itself (not the
/// whole head) — where a go-to-definition jump should land the cursor.
fn rule_definitions(text: &str) -> HashMap<String, (usize, usize)> {
    /// The offset of the first non-whitespace char at or after `from`.
    fn skip_whitespace(text: &str, from: usize) -> usize {
        from + (text[from..].len() - text[from..].trim_start().len())
    }

    let mut defs = HashMap::new();
    let mut chars = text.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if !(c.is_ascii_alphabetic() || c == '_') {
            chars.next();
            continue;
        }
        // A sigil right before this identifier means it names a relation
        // (`*rel`), a search index (`~rel:idx`), or a parameter (`$name`),
        // never a rule -- not a definition site by construction.
        let sigiled = text[..start]
            .chars()
            .next_back()
            .is_some_and(|prev| matches!(prev, '*' | '~' | '$'));
        let mut end = start;
        while let Some(&(i, c)) = chars.peek() {
            if !is_word_char(c) {
                break;
            }
            end = i + c.len_utf8();
            chars.next();
        }
        if !sigiled {
            let open = skip_whitespace(text, end);
            if text[open..].starts_with('[')
                && let Some(close) = matching_bracket(text, open)
            {
                let after_head = skip_whitespace(text, close + 1);
                if text[after_head..].starts_with(":=")
                    || text[after_head..].starts_with("<-")
                    || text[after_head..].starts_with("<~")
                {
                    defs.entry(text[start..end].to_string())
                        .or_insert((start, end));
                }
            }
        }
    }
    defs
}

/// A [`Location`] for the definition of the rule named by the word at
/// `position`, if any: the word must itself be a bare (unsigiled)
/// identifier immediately followed by `[` (a rule reference's own shape),
/// and that name must have a definition somewhere in `text`.
fn definition_at(uri: &Uri, text: &str, position: Position) -> Option<lsp_types::Location> {
    let index = LineIndex::new(text);
    let byte_offset = index.offset(text, position);
    let (word, start, _) = word_at(text, byte_offset)?;
    let sigiled = start > 0
        && text[..start]
            .chars()
            .next_back()
            .is_some_and(|prev| matches!(prev, '*' | '~' | '$'));
    if sigiled {
        return None;
    }
    let (def_start, def_end) = *rule_definitions(text).get(word)?;
    Some(lsp_types::Location::new(
        uri.clone(),
        Range::new(
            index.position(text, def_start),
            index.position(text, def_end),
        ),
    ))
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
    let mut db: Option<Db<FjallStorage>> = None;

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
                        definition_provider: Some(OneOf::Left(true)),
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
            "textDocument/definition" => {
                if let Some(id) = id {
                    let location = msg.get("params").and_then(|params| {
                        let uri_str = params["textDocument"]["uri"].as_str()?;
                        let text = open_docs.get(uri_str)?;
                        let position: Position =
                            serde_json::from_value(params["position"].clone()).ok()?;
                        let uri: Uri = uri_str.parse().ok()?;
                        definition_at(&uri, text, position)
                    });
                    write_message(&mut writer, &response(id, serde_json::to_value(location)?))?;
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
