/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! miette → LSP: spans to ranges, errors to diagnostics.
//!
//! LSP positions are UTF-16 code units — escape/codepoint diagnostics that
//! carry non-ASCII spans would render at the wrong column otherwise.
//! Positions clamp past-the-end; offset is the honest inverse with the
//! spec's own over-long-position rule. One [`Diagnostic`] per label across
//! the error tree; clamp, never crash; None, never guess.

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

/// Byte offset → LSP [`Position`]. LSP positions are UTF-16 code units
/// (`PositionEncodingKind::UTF16`, the default every client must support),
/// not bytes and not Unicode scalar values — a span past the first
/// non-ASCII character would render at the wrong column in the editor
/// otherwise, silently, for exactly the scripts most likely to exercise the
/// escape/codepoint diagnostics (`InvalidUtf8Error` / `InvalidEscapeSeqError`)
/// that carry non-ASCII spans.
pub(crate) struct LineIndex {
    /// Byte offset of the start of each line (line 0's start, always 0, is
    /// the first entry).
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        LineIndex { line_starts }
    }

    /// The [`Position`] for `byte_offset` into the same `text` this index was
    /// built from. Clamps past-the-end offsets to the document's end
    /// rather than panicking: a span is proven in-bounds against the
    /// *parser's* view of the text, but nothing re-proves that here, and a
    /// clamp is a wrong-looking diagnostic, never a crashed server.
    pub(crate) fn position(&self, text: &str, byte_offset: usize) -> Position {
        let byte_offset = byte_offset.min(text.len());
        let line = match self.line_starts.binary_search(&byte_offset) {
            Ok(exact) => exact,
            Err(insert_at) => insert_at.checked_sub(1).expect(
                "INVARIANT(LineIndexNonEmpty): line_starts starts at byte 0",
            ),
        };
        let line_start = self.line_starts[line];
        let character = u32::try_from(text[line_start..byte_offset].encode_utf16().count())
            .expect("INVARIANT(LspUtf16Column): UTF-16 column count fits u32");
        let line = u32::try_from(line).expect("INVARIANT(LspLine): line index fits u32");
        Position::new(line, character)
    }

    /// The inverse of [`LineIndex::position`]: the byte offset a [`Position`]
    /// (UTF-16 code units into its line) names. Walks the line's chars
    /// counting UTF-16 units rather than assuming 1 unit == 1 byte, for the
    /// same reason `position` above doesn't assume it either. Clamps a
    /// character past the line's real length to the line's end (the LSP
    /// spec's own rule for an over-long position, not an error).
    pub(crate) fn offset(&self, text: &str, position: Position) -> usize {
        let line = usize::try_from(position.line)
            .expect("INVARIANT(LspLine): u32 line fits usize");
        let Some(&line_start) = self.line_starts.get(line) else {
            return text.len();
        };
        let line_end = match self.line_starts.get(line + 1) {
            None => text.len(),
            Some(&next) => match next.checked_sub(1) {
                Some(end) => end.max(line_start),
                None => line_start,
            },
        };
        let line_text = &text[line_start..line_end.min(text.len())];
        let mut units = 0u32;
        for (byte_idx, ch) in line_text.char_indices() {
            if units >= position.character {
                return line_start + byte_idx;
            }
            units = match units.checked_add(u32::try_from(ch.len_utf16()).expect(
                "INVARIANT(LspUtf16Width): char UTF-16 width fits u32",
            )) {
                Some(n) => n,
                None => return line_start + line_text.len(),
            };
        }
        line_start + line_text.len()
    }
}

/// Unspanned ERROR diagnostic — refuse surfaces (clock, etc.) with no parser label.
pub(crate) fn unspanned_error(message: String) -> Diagnostic {
    Diagnostic {
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("kyzoscript".to_string()),
        message,
        ..Diagnostic::default()
    }
}

/// The identifier-shaped word touching `offset` in `text` — the token a
/// hover or completion request is "about" — plus its own byte range,
/// widening left and right from `offset` while the character is a
/// KyzoScript identifier character (`[A-Za-z0-9_]`; good enough for a
/// relation/aggregation name, which is all hover/completion resolve
/// against here). `None` if `offset` doesn't touch such a word at all
/// (whitespace, punctuation, end of document).
pub(crate) fn word_at(text: &str, offset: usize) -> Option<(&str, usize, usize)> {
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

/// One [`Diagnostic`] per label in `err`'s tree (its own labels, then every
/// `#[related]` cause and `diagnostic_source`, walked recursively — the
/// same shape `parse::fuzz_tests::walk_labels` proves every parser error
/// satisfies, just walked here from the public `Diagnostic` trait instead
/// of that crate-internal test helper). Every diagnostic shares one
/// message: the error's own `Display`, plus its `#[help]` text appended
/// when present, so the mechanical fix for a SQL-shaped mistake (or any
/// other designed hint) shows up in the editor, not just at the CLI. Falls
/// back to a single diagnostic at the document's start if the error tree
/// carries no label anywhere (defensive: every error the parse tier itself
/// raises is spanned, but the language door can also surface a non-parser
/// failure, e.g. the system clock, with no span to give).
pub(crate) fn diagnostics_from_report(
    err: &miette::Report,
    text: &str,
    index: &LineIndex,
) -> Vec<Diagnostic> {
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
