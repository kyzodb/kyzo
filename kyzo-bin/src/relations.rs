/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Bulk relation export/import for the HTTP API and the REPL's `%import`.
//!
//! The CozoDB original (`cozo-core/src/runtime/db.rs::export_relations` /
//! `import_relations`) reached directly into a live transaction's storage —
//! `Db`'s internals in this port are crate-private (`runtime/db.rs`'s
//! `SessionTx` constructors are `pub(crate)`; there is no equivalent public
//! surface), so this is composed instead from the same public entry point
//! every other caller uses: [`Db::run_script`]. `::columns` gives the
//! column list, a plain `*rel{...}` scan gives the rows, and a `<-` mutation
//! with `$data` bound to the rows writes them back — the same query shape
//! the CozoDB original used for its `NamedRows::into_payload` convenience
//! method. This is less direct than a hand-rolled storage walk, but it is
//! the only route kyzo-core's runtime tier currently exposes, and it is
//! correct: every step is a real query the engine already proves sound.
//!
//! Every relation and column name here (an HTTP path segment for
//! `/export`, a JSON object key or header for `/import`) gets spliced into
//! composed KyzoScript via `format!`, so each is validated first
//! ([`validate_identifier`]) against the grammar's `compound_ident` shape
//! (`kyzoscript.pest`: dot-separated segments, each starting with a
//! letter/underscore and continuing with letters/digits/underscores — the
//! ASCII subset of `XID_START`/`XID_CONTINUE`). A caller reaching either
//! endpoint already has full script privilege (there is no separate
//! read-only capability — see `server/mod.rs`'s module doc), so this is
//! not privilege escalation either way; it closes off a name smuggling
//! extra script syntax (a stray `}`, a second statement, a different
//! target relation) into a query this module builds on the caller's
//! behalf.

use std::collections::BTreeMap;

use kyzo::{DataValue, Db, FjallStorage, NamedRows};
use miette::{Result, bail, miette};

/// Is `name` safe to splice into composed KyzoScript as a bare identifier
/// (a relation or column name)? Requires the ASCII subset of the grammar's
/// `compound_ident`: one or more `.`-separated, non-empty segments, each an
/// ASCII letter-or-underscore followed by ASCII letters/digits/underscores.
/// Rejects anything that could smuggle script syntax through — spaces,
/// braces, colons, quotes, `$`, `<-`, `~`, etc.
pub(crate) fn validate_identifier(name: &str) -> Result<()> {
    let is_valid_segment = |segment: &str| {
        let mut chars = segment.chars();
        matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if !name.is_empty() && name.split('.').all(is_valid_segment) {
        Ok(())
    } else {
        bail!("'{name}' is not a valid identifier")
    }
}

/// Column names of `relation`, key columns first (the same order
/// `::columns` reports and the same order a bare `*relation{cols}` pattern
/// binds them in).
fn columns_of(db: &Db<FjallStorage>, relation: &str) -> Result<Vec<String>> {
    validate_identifier(relation)?;
    let out = db.run_script(&format!("::columns {relation}"), BTreeMap::new())?;
    out.rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .next()
                .and_then(|v| match v {
                    DataValue::Str(s) => Some(s.to_string()),
                    _ => None,
                })
                .ok_or_else(|| miette!("'::columns {relation}' returned a malformed row"))
        })
        .collect()
}

/// Export the named relations as `{relation_name: NamedRows}`.
pub fn export_relations<I, T>(
    db: &Db<FjallStorage>,
    relations: I,
) -> Result<BTreeMap<String, NamedRows>>
where
    T: AsRef<str>,
    I: Iterator<Item = T>,
{
    let mut out = BTreeMap::new();
    for rel in relations {
        let rel = rel.as_ref();
        let cols = columns_of(db, rel)?;
        let cols_str = cols.join(", ");
        let query = format!("?[{cols_str}] := *{rel}{{{cols_str}}}");
        let rows = db.run_script(&query, BTreeMap::new())?;
        out.insert(rel.to_string(), rows);
    }
    Ok(out)
}

/// Import `{relation_name: NamedRows}` into the store. A name prefixed with
/// `-` deletes the given rows from that relation instead of writing them
/// (`:rm` instead of `:put` — the same convention the CozoDB original's
/// direct-storage `import_relations` used).
pub fn import_relations(db: &Db<FjallStorage>, data: BTreeMap<String, NamedRows>) -> Result<()> {
    for (relation_op, rows) in data {
        let (op, relation) = match relation_op.strip_prefix('-') {
            Some(rel) => (":rm", rel),
            None => (":put", relation_op.as_str()),
        };
        validate_identifier(relation)?;
        if rows.headers.is_empty() {
            continue;
        }
        // Column names come from the caller's JSON, the same as the
        // relation name, and are spliced into the same query — the same
        // grammar-shaped validation applies.
        for col in &rows.headers {
            validate_identifier(col)?;
        }
        let cols_str = rows.headers.join(", ");
        let query = format!("?[{cols_str}] <- $data {op} {relation} {{{cols_str}}}");
        let data_value = DataValue::List(
            rows.rows
                .into_iter()
                .map(DataValue::List)
                .collect(),
        );
        db.run_script(&query, BTreeMap::from([("data".to_string(), data_value)]))
            .map_err(|e| miette!("cannot import data for relation '{relation}': {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_identifier;

    #[test]
    fn accepts_plain_and_compound_identifiers() {
        assert!(validate_identifier("person").is_ok());
        assert!(validate_identifier("_temp").is_ok());
        assert!(validate_identifier("a1_b2").is_ok());
        assert!(validate_identifier("ns.person").is_ok());
        assert!(validate_identifier("a.b.c").is_ok());
    }

    #[test]
    fn rejects_empty_and_malformed_segments() {
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("1abc").is_err());
        assert!(validate_identifier("a..b").is_err());
        assert!(validate_identifier(".a").is_err());
        assert!(validate_identifier("a.").is_err());
    }

    #[test]
    fn rejects_script_injection_attempts() {
        // Each of these tries to smuggle extra script syntax through a
        // "relation name" — closing the pattern early, starting a second
        // statement, or escaping the identifier position entirely.
        for hostile in [
            "person} <- [[1]] :put other {x",
            "person {x: 1}",
            "person; ?[x] <- [[1]]",
            "person$evil",
            "person\nrm",
            "person ",
            " person",
            "*person",
            "~person",
            "person<-x",
        ] {
            assert!(
                validate_identifier(hostile).is_err(),
                "expected rejection for {hostile:?}"
            );
        }
    }
}
