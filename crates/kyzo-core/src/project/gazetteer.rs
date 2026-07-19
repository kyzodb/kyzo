/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * The gazetteer engine is wholly new KyzoDB work — it has no CozoDB
 * antecedent — but it is built to the same kernel doctrine as the ported
 * index operators (`fts_index.rs`, `hnsw.rs`, `minhash_lsh.rs`): pure
 * functions over the storage species, corruption typed rather than panicked
 * (law 5), and every span it returns truthful to the byte (the span law
 * extended from the parser to data).
 */

//! The gazetteer / dictionary entity-tagging engine: the *deterministic*
//! cousin of named-entity recognition.
//!
//! Where NER guesses which spans of text *might* be entities, a gazetteer
//! knows: the entities already live in the graph, their surface forms are a
//! stored relation, and tagging a document is exact string matching of those
//! forms against the text. The product is **text-to-graph as a relation** —
//! `tag(document)` yields `(entity_key, byte_start, byte_len, surface)` tuples
//! ready to `JOIN` the very relation the surface forms came from, so a found
//! mention composes with the rest of the graph like any other access path.
//! No search engine composes this way; that is the telos.
//!
//! ## The dictionary
//!
//! The dictionary is one stored relation, shape `[entity] -> [surfaces]`: one
//! key column naming the entity (any [`DataValue`] — an id, a string, a
//! compound), one non-key `List<String>` column of that entity's surface
//! forms. [`compile_dictionary`] scans it into a compiled
//! [`Gazetteer`] automaton. Many entities may share a surface form
//! ("Washington" the state, the city, the person); the engine keeps **all** of
//! them (see the overlap policy below), because dropping a candidate silently
//! is exactly the ambiguity a downstream join exists to resolve.
//!
//! The automaton is rebuilt deterministically from the relation on each
//! [`compile_dictionary`] — the cheapest correct thing. Persisting the built
//! automaton is a later on-disk-format decision (see the design notes); the
//! relation is the source of truth and is always sufficient to rebuild.
//!
//! ## Matching — leftmost-longest, all entities at the winning span
//!
//! Tagging uses an Aho-Corasick automaton in
//! [`MatchKind::LeftmostLongest`](aho_corasick::MatchKind::LeftmostLongest)
//! mode: the classic gazetteer overlap policy. At each position the *longest*
//! surface form that starts *earliest* wins, and matches do not overlap — so
//! in "New York City" the form "New York" (if present) claims `[0,8)` and the
//! contained "York" at `[4,8)` is suppressed. Every entity registered under
//! the *winning* surface form is emitted at that span; distinct surface forms
//! never tie on `(start, len)` (a same-length collision would mean
//! byte-identical forms, which the compiler folds into one), so the only
//! multiplicity at a span is genuine entity ambiguity, and the result is
//! fully deterministic. See the design notes for the alternatives
//! (leftmost-first, all-overlapping) and why leftmost-longest is the default.
//!
//! ## Case folding — ASCII only, because spans must stay truthful
//!
//! [`GazetteerConfig::case_insensitive`] folds **ASCII** case only
//! (`A-Z ≡ a-z`), via the automaton's native ASCII case-insensitivity. This
//! is deliberate, not a shortcut: ASCII folding is a length-preserving 1:1
//! byte map, so a match's `[start, end)` are still exact offsets into the
//! *original* document. Full-Unicode case folding is length-*changing*
//! (`U+0130 İ` lowercases to two code points), so matching on a folded copy
//! would return spans into the copy, not the document — a violation of the
//! span-truthfulness law. The FTS tokenizer's `Lowercase` filter *is*
//! full-Unicode, and correctly so: FTS matches *terms* and never owes an
//! offset back into the source. The gazetteer does, so it cannot borrow that
//! filter for its match path. Unicode-aware folding with an offset-remapping
//! layer is a documented extension seam, not a silent default.
//!
//! ## Laws
//!
//! - **Deterministic.** Same dictionary relation + same text ⇒ byte-identical
//!   tags, in canonical order `(start, entity_key)`. The compile step sorts
//!   its patterns and entity sets; the automaton's match order is
//!   position-deterministic; the output is sorted.
//! - **Corruption is typed, never a panic** (law 5). A dictionary row that
//!   does not decode as `[entity, List<String>]` — wrong arity, a non-list
//!   surfaces column, a non-string element — is [`IndexRowCorrupt`]. An empty
//!   surface form is the typed [`GazetteerEmptySurface`] (a zero-width pattern
//!   would match everywhere and is a definition error, refused at compile).
//! - **Spans are truthful.** Every returned `(start, len)` is a byte range
//!   into the document, in bounds and on `char` boundaries — the latter *by
//!   construction*: every surface form is a non-empty valid-UTF-8 string, so
//!   its first byte is a UTF-8 leading byte and can only align to a boundary,
//!   and its length carries that boundary to the end. A match inside a
//!   multibyte character is impossible to represent.
//!
//! ## Seams
//!
//! - **Exposure.** The natural first surface is a fixed rule
//!   `GazetteerTag(*docs[id, text], *dict[entity, surfaces])` returning
//!   `(id, entity, start, len, surface)` — see the design notes for the
//!   argument that a fixed rule (not an index operator) is the right first
//!   home.
//! - **Incremental dictionary updates** rebuild the automaton; the
//!   relation-is-truth stance makes that always correct if not always cheap
//!   (design notes cover the incremental path).

use aho_corasick::{AhoCorasick, MatchKind};
use miette::{Diagnostic, Result, bail};
use smartstring::{LazyCompact, SmartString};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use kyzo_model::value::DataValue;
use crate::engines::{IndexCorruptReason, IndexRowCorrupt};
use crate::runtime::relation::RelationHandle;
use crate::storage::ReadTx;

// ---------------------------------------------------------------------------
// Config.
// ---------------------------------------------------------------------------

/// How a [`Gazetteer`] folds case while matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct GazetteerConfig {
    /// Fold ASCII case (`A-Z ≡ a-z`) when matching. Length-preserving, so
    /// spans stay exact; **not** full-Unicode folding — see the module docs
    /// for why the gazetteer's span law forbids the length-changing kind.
    pub(crate) case_insensitive: bool,
}

// ---------------------------------------------------------------------------
// Typed errors.
// ---------------------------------------------------------------------------

/// A dictionary entity carried an empty surface form. A zero-width pattern
/// matches at every position and cannot yield a truthful span, so it is a
/// definition error refused at compile time (typed, like the FTS extractor's
/// type error — the analogous "this definition cannot mean anything" case).
#[derive(Debug, Error, Diagnostic)]
#[error("gazetteer dictionary entity {entity} has an empty surface form")]
#[diagnostic(code(index::gazetteer::empty_surface))]
#[diagnostic(help(
    "every surface form must be a non-empty string; an empty form would match \
     at every position and has no truthful span"
))]
pub(crate) struct GazetteerEmptySurface {
    pub(crate) entity: String,
}

/// The Aho-Corasick automaton could not be built from the dictionary's
/// surface forms (e.g. the combined pattern set exceeds the automaton's size
/// limits). The dictionary relation is intact and can be pruned and recompiled.
#[derive(Debug, Error, Diagnostic)]
#[error("gazetteer automaton build failed")]
#[diagnostic(code(index::gazetteer::build_failed))]
pub(crate) struct GazetteerBuildFailed;

// ---------------------------------------------------------------------------
// The dictionary relation's schema.
// ---------------------------------------------------------------------------

/// The canonical dictionary shape: one key column `entity` of the given type,
/// one non-key `surfaces` column of `List<String>`. The lifecycle tier can
/// mint the relation from this; [`compile_dictionary`] expects exactly this
/// arity (1 key + 1 non-key). Multi-column entity keys are a documented
/// later generalization.
pub(crate) fn gazetteer_dict_metadata(entity_type: ColType) -> StoredRelationMetadata {
    StoredRelationMetadata {
        keys: vec![ColumnDef {
            name: SmartString::from("entity"),
            typing: NullableColType::required(entity_type),
            default_gen: None,
        }],
        non_keys: vec![ColumnDef {
            name: SmartString::from("surfaces"),
            typing: NullableColType::required(ColType::List {
                eltype: Box::new(NullableColType::required(ColType::String)),
                len: None,
            }),
            default_gen: None,
        }],
    }
}

// ---------------------------------------------------------------------------
// Compiled automaton.
// ---------------------------------------------------------------------------

/// A dictionary compiled into a matchable automaton. Built by
/// [`compile_dictionary`]; matched by [`Gazetteer::tag`]. Immutable once
/// built — an incremental dictionary change recompiles (see the design
/// notes).
#[derive(Debug)]
pub(crate) struct Gazetteer {
    /// `None` when the dictionary contributed no surface forms (empty
    /// dictionary): the automaton matches nothing. Kept optional so
    /// correctness never rides on the automaton library's zero-pattern
    /// behavior.
    automaton: Option<AhoCorasick>,
    /// Per pattern id (Aho-Corasick assigns ids by build order = the sorted
    /// pattern order), the entities registered under that surface form, sorted
    /// and deduplicated. Parallel to the pattern list the automaton was built
    /// from.
    entities_by_pattern: Vec<Vec<DataValue>>,
    /// Whether matching folds ASCII case; retained for introspection and to
    /// document what the automaton was built to do.
    #[allow(dead_code)]
    case_insensitive: bool,
}

/// One tagged mention: an entity found at a byte span of the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tag {
    /// The dictionary entity key — ready to join the source relation.
    pub(crate) entity: DataValue,
    /// Byte offset of the mention's start in the document. On a `char`
    /// boundary, in bounds.
    pub(crate) start: usize,
    /// Byte length of the mention. `start + len` is on a `char` boundary, in
    /// bounds.
    pub(crate) len: usize,
    /// The exact document slice that matched (`&text[start..start+len]`) —
    /// the original-cased text, which under ASCII folding may differ in case
    /// from the dictionary form.
    pub(crate) surface: SmartString<LazyCompact>,
}

/// Compile a dictionary relation `[entity, List<String> surfaces]` into a
/// matchable [`Gazetteer`].
///
/// Deterministic: surface forms are collected into a sorted map and entities
/// into sorted sets, so the pattern order (hence Aho-Corasick pattern ids) and
/// the per-pattern entity lists are a pure function of the relation's
/// contents, independent of scan or hash order.
///
/// When `config.case_insensitive`, surface forms are keyed by their
/// ASCII-lowercased bytes so case-variant forms ("Cat"/"cat") collapse into
/// one pattern with the union of their entities — matching the automaton's own
/// ASCII folding, so there are never two automaton-equivalent patterns to tie
/// between.
///
/// Errors (typed, never a panic): a row whose arity or column types are not
/// `[entity, List<String>]` is [`IndexRowCorrupt`]; an empty surface form is
/// [`GazetteerEmptySurface`]; an automaton that will not build is
/// [`GazetteerBuildFailed`].
pub(crate) fn compile_dictionary(
    tx: &impl ReadTx,
    dict: &RelationHandle,
    config: GazetteerConfig,
) -> Result<Gazetteer> {
    // surface-key -> the set of entities to emit for that pattern. The key is
    // the raw form (exact) or its ASCII-lowercase (folded); the automaton is
    // built from these keys.
    let mut collector: BTreeMap<SmartString<LazyCompact>, BTreeSet<DataValue>> = BTreeMap::new();

    for row in dict.scan_all(tx) {
        let row = row?;
        if row.len() != 2 {
            bail!(IndexRowCorrupt::new(
                &dict.name,
                row.as_slice(),
                IndexCorruptReason::WrongColumnCount {
                    found: row.len(),
                    expected: 2,
                },
            ));
        }
        let entity = row[0].clone();
        let surfaces = row[1].get_slice().ok_or_else(|| {
            IndexRowCorrupt::new(
                &dict.name,
                row.as_slice(),
                IndexCorruptReason::GazetteerSurfacesNotList,
            )
        })?;
        for s in surfaces {
            let s = s.get_str().ok_or_else(|| {
                IndexRowCorrupt::new(
                    &dict.name,
                    row.as_slice(),
                    IndexCorruptReason::GazetteerSurfaceNotString,
                )
            })?;
            if s.is_empty() {
                bail!(GazetteerEmptySurface {
                    entity: format!("{entity:?}"),
                });
            }
            let key = if config.case_insensitive {
                SmartString::from(s.to_ascii_lowercase())
            } else {
                SmartString::from(s)
            };
            collector.entry(key).or_default().insert(entity.clone());
        }
    }

    let patterns: Vec<SmartString<LazyCompact>> = collector.keys().cloned().collect();
    let entities_by_pattern: Vec<Vec<DataValue>> = collector
        .into_values()
        .map(|set| set.into_iter().collect())
        .collect();

    let automaton = if patterns.is_empty() {
        None
    } else {
        let ac = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .ascii_case_insensitive(config.case_insensitive)
            .build(&patterns)
            .map_err(|_| GazetteerBuildFailed)?;
        Some(ac)
    };

    Ok(Gazetteer {
        automaton,
        entities_by_pattern,
        case_insensitive: config.case_insensitive,
    })
}

impl Gazetteer {
    /// Tag a document: find every dictionary mention under the leftmost-longest
    /// overlap policy, emitting one [`Tag`] per entity registered at each
    /// winning span. Infallible and deterministic — the automaton is already
    /// built and validated, and every span is in bounds and on a `char`
    /// boundary by construction. Result is in canonical order
    /// `(start, entity)`.
    pub(crate) fn tag(&self, text: &str) -> Vec<Tag> {
        let Some(ac) = &self.automaton else {
            return Vec::new();
        };
        let mut out: Vec<Tag> = Vec::new();
        for m in ac.find_iter(text) {
            let start = m.start();
            let end = m.end();
            // `start`/`end` bound a matched surface form: a non-empty valid
            // UTF-8 pattern found in valid UTF-8 text, so both are `char`
            // boundaries and the slice cannot split a character.
            let surface = SmartString::<LazyCompact>::from(&text[start..end]);
            for entity in &self.entities_by_pattern[m.pattern().as_usize()] {
                out.push(Tag {
                    entity: entity.clone(),
                    start,
                    len: end - start,
                    surface: surface.clone(),
                });
            }
        }
        // find_iter already yields ascending, non-overlapping starts; the
        // per-span entity lists are already sorted. Sorting makes the
        // (start, entity) canonical order explicit and robust to either.
        out.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.entity.cmp(&b.entity)));
        out
    }

    /// The number of distinct surface-form patterns the automaton carries.
    /// Zero for an empty dictionary.
    #[allow(dead_code)]
    pub(crate) fn pattern_count(&self) -> usize {
        self.entities_by_pattern.len()
    }
}

// ---------------------------------------------------------------------------
// Tests: the engine's executable law.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::program::InputRelationHandle;
    use kyzo_model::SourceSpan;
    use kyzo_model::program::symbol::Symbol;
    use crate::runtime::relation::{KeyspaceKind, RelationHandle, create_relation};
    use crate::storage::fjall::new_fjall_storage;
    use crate::storage::{Storage, WriteTx};

    // -- fixture construction -------------------------------------------------

    fn input_handle(name: &str, metadata: StoredRelationMetadata) -> InputRelationHandle {
        let key_bindings = metadata
            .keys
            .iter()
            .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
            .collect();
        let dep_bindings = metadata
            .non_keys
            .iter()
            .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
            .collect();
        InputRelationHandle {
            name: Symbol::new(name, SourceSpan(0, 0)),
            metadata,
            key_bindings,
            dep_bindings,
            span: SourceSpan(0, 0),
        }
    }

    /// Build a dictionary relation from `(entity_id, &[surface_form])` rows and
    /// compile it. Entity keys are `Int`s here for brevity; the engine treats
    /// them as opaque `DataValue`s.
    fn compile(
        db: &impl Storage,
        rows: &[(i64, &[&str])],
        config: GazetteerConfig,
    ) -> (RelationHandle, Gazetteer) {
        let meta = gazetteer_dict_metadata(ColType::Int);
        let mut tx = db.write_tx().unwrap();
        let dict =
            create_relation(&mut tx, input_handle("dict", meta), KeyspaceKind::Facts).unwrap();
        for (entity, surfaces) in rows {
            let surface_list =
                DataValue::List(surfaces.iter().map(|s| DataValue::from(*s)).collect());
            let row = vec![DataValue::from(*entity), surface_list];
            dict.put_fact(
                &mut tx,
                &row,
                kyzo_model::value::ValidityTs::from_raw(0),
                SourceSpan(0, 0),
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let rtx = db.read_tx().unwrap();
        let g = compile_dictionary(&rtx, &dict, config).unwrap();
        (dict, g)
    }

    /// A compact, comparable view of a tag: `(entity_id, start, len, surface)`.
    fn view(tags: &[Tag]) -> Vec<(i64, usize, usize, String)> {
        tags.iter()
            .map(|t| {
                (
                    t.entity.get_int().unwrap(),
                    t.start,
                    t.len,
                    t.surface.to_string(),
                )
            })
            .collect()
    }

    // -- the naive reference oracle -------------------------------------------

    /// Obviously-correct leftmost-longest tagger: scan every char boundary,
    /// take the longest surface form matching at that position, emit every
    /// entity of that (case-adjusted) length, jump past it. `O(n·m)`, only for
    /// tests — [`Gazetteer::tag`] must agree with it byte for byte.
    fn naive_tag(
        pairs: &[(i64, &str)],
        text: &str,
        case_insensitive: bool,
    ) -> Vec<(i64, usize, usize, String)> {
        let eq = |a: &[u8], b: &[u8]| {
            if case_insensitive {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        };
        let bytes = text.as_bytes();
        let mut out: Vec<(i64, usize, usize, String)> = Vec::new();
        let mut start = 0usize;
        while start < text.len() {
            if !text.is_char_boundary(start) {
                start += 1;
                continue;
            }
            // Longest surface length that matches at `start`.
            let mut best_len = 0usize;
            for (_, surf) in pairs {
                let sb = surf.as_bytes();
                let end = start + sb.len();
                if end <= text.len()
                    && text.is_char_boundary(end)
                    && eq(&bytes[start..end], sb)
                    && sb.len() > best_len
                {
                    best_len = sb.len();
                }
            }
            if best_len == 0 {
                // advance one whole char
                start += 1;
                while start < text.len() && !text.is_char_boundary(start) {
                    start += 1;
                }
                continue;
            }
            let end = start + best_len;
            let mut entities: BTreeSet<i64> = BTreeSet::new();
            for (e, surf) in pairs {
                if surf.len() == best_len && eq(&bytes[start..end], surf.as_bytes()) {
                    entities.insert(*e);
                }
            }
            for e in entities {
                out.push((e, start, best_len, text[start..end].to_string()));
            }
            start = end;
        }
        out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// Flatten `(entity, &[surface])` rows to `(entity, surface)` pairs for the
    /// oracle.
    fn pairs<'a>(rows: &'a [(i64, &'a [&'a str])]) -> Vec<(i64, &'a str)> {
        rows.iter()
            .flat_map(|(e, ss)| ss.iter().map(move |s| (*e, *s)))
            .collect()
    }

    // -- laws -----------------------------------------------------------------

    #[test]
    fn matches_naive_reference_across_documents() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows: &[(i64, &[&str])] = &[
            (1, &["New York"]),
            (2, &["York"]),
            (3, &["New York City"]),
            (4, &["cat"]),
            (5, &["category"]),
        ];
        let (_dict, g) = compile(&db, rows, GazetteerConfig::default());
        let p = pairs(rows);
        for text in [
            "I love New York City in the fall",
            "the cat sat in the category",
            "York and New York and New York City",
            "no entities here at all",
            "New Yorkers are not New York",
            "",
            "catcatcat",
        ] {
            assert_eq!(
                view(&g.tag(text)),
                naive_tag(&p, text, false),
                "mismatch on {text:?}"
            );
        }
    }

    #[test]
    fn leftmost_longest_overlap_policy() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        // "New York" (len 8) contains "York" (len 4); "New York City" (13)
        // contains both.
        let rows: &[(i64, &[&str])] =
            &[(1, &["New York"]), (2, &["York"]), (3, &["New York City"])];
        let (_dict, g) = compile(&db, rows, GazetteerConfig::default());

        // Longest wins from the leftmost start: the whole "New York City".
        let tags = view(&g.tag("New York City"));
        assert_eq!(tags, vec![(3, 0, 13, "New York City".to_string())]);

        // Without the trailing "City", "New York" (8) beats contained "York".
        let tags = view(&g.tag("New York today"));
        assert_eq!(tags, vec![(1, 0, 8, "New York".to_string())]);

        // Bare "York" still tags on its own.
        let tags = view(&g.tag("York alone"));
        assert_eq!(tags, vec![(2, 0, 4, "York".to_string())]);
    }

    #[test]
    fn shared_surface_form_emits_every_entity() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        // Three distinct entities share the surface "Washington".
        let rows: &[(i64, &[&str])] = &[
            (10, &["Washington"]),
            (20, &["Washington"]),
            (30, &["Washington"]),
        ];
        let (_dict, g) = compile(&db, rows, GazetteerConfig::default());
        let tags = view(&g.tag("Washington"));
        assert_eq!(
            tags,
            vec![
                (10, 0, 10, "Washington".to_string()),
                (20, 0, 10, "Washington".to_string()),
                (30, 0, 10, "Washington".to_string()),
            ],
            "an ambiguous surface yields all its entities at the one span"
        );
    }

    #[test]
    fn adjacent_and_repeated_matches() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows: &[(i64, &[&str])] = &[(1, &["ab"]), (2, &["cd"])];
        let (_dict, g) = compile(&db, rows, GazetteerConfig::default());
        // Adjacent, no separator: two back-to-back matches.
        let tags = view(&g.tag("abcd"));
        assert_eq!(
            tags,
            vec![(1, 0, 2, "ab".to_string()), (2, 2, 2, "cd".to_string()),]
        );
        // Repeated same form at several offsets.
        let tags = view(&g.tag("ab ab ab"));
        assert_eq!(
            tags,
            vec![
                (1, 0, 2, "ab".to_string()),
                (1, 3, 2, "ab".to_string()),
                (1, 6, 2, "ab".to_string()),
            ]
        );
    }

    #[test]
    fn case_insensitive_folds_ascii_and_keeps_spans() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows: &[(i64, &[&str])] = &[(1, &["Rust"]), (2, &["cozo"])];
        let cfg = GazetteerConfig {
            case_insensitive: true,
        };
        let (_dict, g) = compile(&db, rows, cfg);
        let p = pairs(rows);
        let text = "RUST and rust and Cozo and COZO";
        // Agrees with the case-insensitive oracle.
        assert_eq!(view(&g.tag(text)), naive_tag(&p, text, true));
        // And the surface returned is the DOCUMENT's casing, not the form's.
        let tags = view(&g.tag("RUST"));
        assert_eq!(tags, vec![(1, 0, 4, "RUST".to_string())]);
    }

    #[test]
    fn case_variant_forms_collapse_to_one_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        // "Cat" and "cat" as forms of two different entities: under folding
        // they are one pattern carrying both entities.
        let rows: &[(i64, &[&str])] = &[(1, &["Cat"]), (2, &["cat"])];
        let cfg = GazetteerConfig {
            case_insensitive: true,
        };
        let (_dict, g) = compile(&db, rows, cfg);
        assert_eq!(g.pattern_count(), 1, "case-variant forms fold to one");
        let tags = view(&g.tag("a CAT here"));
        assert_eq!(
            tags,
            vec![(1, 2, 3, "CAT".to_string()), (2, 2, 3, "CAT".to_string()),],
            "both entities emit at the one folded match"
        );
    }

    #[test]
    fn exact_mode_is_case_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows: &[(i64, &[&str])] = &[(1, &["Rust"])];
        let (_dict, g) = compile(&db, rows, GazetteerConfig::default());
        assert!(g.tag("i love rust").is_empty(), "lowercase does not match");
        assert_eq!(
            view(&g.tag("i love Rust")),
            vec![(1, 7, 4, "Rust".to_string())]
        );
    }

    // -- unicode --------------------------------------------------------------

    #[test]
    fn unicode_multibyte_spans_are_byte_offsets_on_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        // "café" is 5 bytes (é = 2 bytes); "naïve" is 6 bytes (ï = 2).
        let rows: &[(i64, &[&str])] = &[(1, &["café"]), (2, &["naïve"]), (3, &["東京"])];
        let (_dict, g) = compile(&db, rows, GazetteerConfig::default());
        let p = pairs(rows);

        let text = "a café and a naïve 東京 tour";
        assert_eq!(view(&g.tag(text)), naive_tag(&p, text, false));

        // Spot the byte offsets explicitly: "café" starts at byte 2, spans 5.
        let tags = g.tag(text);
        let cafe = tags
            .iter()
            .find(|t| t.entity == DataValue::from(1i64))
            .unwrap();
        assert_eq!((cafe.start, cafe.len), (2, 5));
        assert_eq!(&text[cafe.start..cafe.start + cafe.len], "café");
        // "東京" is two 3-byte chars = 6 bytes.
        let tokyo = tags
            .iter()
            .find(|t| t.entity == DataValue::from(3i64))
            .unwrap();
        assert_eq!(tokyo.len, 6);
        assert_eq!(&text[tokyo.start..tokyo.start + tokyo.len], "東京");
    }

    #[test]
    fn no_match_inside_a_multibyte_char() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        // 'é' is bytes [0xC3, 0xA9]. A form can never be the byte 0xA9 alone
        // (that is not valid UTF-8, so it is not a representable surface). The
        // form "é" matches the whole char, on boundaries.
        let rows: &[(i64, &[&str])] = &[(1, &["é"])];
        let (_dict, g) = compile(&db, rows, GazetteerConfig::default());
        // "café" contains an é at bytes [3,5); the tag lands exactly there.
        let tags = view(&g.tag("café"));
        assert_eq!(tags, vec![(1, 3, 2, "é".to_string())]);
    }

    #[test]
    fn zalgo_and_rtl_agree_with_oracle() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        // Zalgo (combining marks), RTL (Arabic/Hebrew), and an emoji ZWJ
        // sequence — all valid UTF-8, all handled as opaque bytes.
        let rows: &[(i64, &[&str])] = &[
            (1, &["a\u{0301}\u{0300}"]), // 'a' + two combining accents
            (2, &["مرحبا"]),             // "hello" in Arabic (RTL)
            (3, &["שלום"]),              // "peace" in Hebrew (RTL)
            (4, &["👩\u{200d}💻"]),      // woman technologist (ZWJ emoji)
        ];
        let (_dict, g) = compile(&db, rows, GazetteerConfig::default());
        let p = pairs(rows);
        for text in [
            "say مرحبا to a\u{0301}\u{0300} and שלום",
            "the 👩\u{200d}💻 codes",
            "a\u{0301}\u{0300}a\u{0301}\u{0300}",
        ] {
            assert_eq!(
                view(&g.tag(text)),
                naive_tag(&p, text, false),
                "mismatch on {text:?}"
            );
        }
    }

    // -- edge cases -----------------------------------------------------------

    #[test]
    fn empty_dictionary_tags_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let (_dict, g) = compile(&db, &[], GazetteerConfig::default());
        assert_eq!(g.pattern_count(), 0);
        assert!(g.tag("any text at all").is_empty());
        assert!(g.tag("").is_empty());
    }

    #[test]
    fn empty_document_tags_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows: &[(i64, &[&str])] = &[(1, &["anything"])];
        let (_dict, g) = compile(&db, rows, GazetteerConfig::default());
        assert!(g.tag("").is_empty());
    }

    // -- determinism ----------------------------------------------------------

    #[test]
    fn determinism_run_twice_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows: &[(i64, &[&str])] = &[
            (3, &["gamma", "g"]),
            (1, &["alpha", "a"]),
            (2, &["beta", "b", "alpha"]), // shares "alpha" with entity 1
        ];
        let text = "alpha beta gamma a b g alpha";
        // One stored relation; two independent compiles of it must agree.
        let (dict, g1) = compile(&db, rows, GazetteerConfig::default());
        let rtx = db.read_tx().unwrap();
        let g2 = compile_dictionary(&rtx, &dict, GazetteerConfig::default()).unwrap();
        let t1 = g1.tag(text);
        let t2 = g2.tag(text);
        assert_eq!(t1, t2, "same dictionary + text ⇒ identical tags");
        // And a second tag() call on the same automaton is identical.
        assert_eq!(g1.tag(text), t1);
    }

    // -- corruption is typed, never a panic -----------------------------------

    #[test]
    fn empty_surface_form_is_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let meta = gazetteer_dict_metadata(ColType::Int);
        let mut tx = db.write_tx().unwrap();
        let dict =
            create_relation(&mut tx, input_handle("dict", meta), KeyspaceKind::Facts).unwrap();
        let row = vec![
            DataValue::from(1i64),
            DataValue::List(vec![DataValue::from("ok"), DataValue::from("")]),
        ];
        dict.put_fact(
            &mut tx,
            &row,
            kyzo_model::value::ValidityTs::from_raw(0),
            SourceSpan(0, 0),
        )
        .unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let err = compile_dictionary(&rtx, &dict, GazetteerConfig::default())
            .expect_err("empty surface must error");
        assert!(
            err.downcast_ref::<GazetteerEmptySurface>().is_some(),
            "typed empty-surface error, got: {err:?}"
        );
    }

    #[test]
    fn corrupt_surfaces_column_is_typed_error_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows: &[(i64, &[&str])] = &[(1, &["fine"])];
        let (dict, _g) = compile(&db, rows, GazetteerConfig::default());

        // Overwrite the row's value with a non-list surfaces column (an Int
        // where a List<String> is required). The stored bytes decode as a
        // valid tuple, but the wrong shape for the dictionary.
        let mut tx = db.write_tx().unwrap();
        let bad = vec![DataValue::from(1i64), DataValue::from(999i64)];
        dict.put_fact(
            &mut tx,
            &bad,
            kyzo_model::value::ValidityTs::from_raw(1),
            SourceSpan(0, 0),
        )
        .unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let err = compile_dictionary(&rtx, &dict, GazetteerConfig::default())
            .expect_err("non-list surfaces must error, not panic");
        assert!(
            err.downcast_ref::<IndexRowCorrupt>().is_some(),
            "typed corruption error, got: {err:?}"
        );
    }

    #[test]
    fn non_string_surface_element_is_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows: &[(i64, &[&str])] = &[(1, &["fine"])];
        let (dict, _g) = compile(&db, rows, GazetteerConfig::default());

        // A surfaces list whose element is an Int, not a String.
        let mut tx = db.write_tx().unwrap();
        let bad = vec![
            DataValue::from(1i64),
            DataValue::List(vec![DataValue::from(42i64)]),
        ];
        dict.put_fact(
            &mut tx,
            &bad,
            kyzo_model::value::ValidityTs::from_raw(1),
            SourceSpan(0, 0),
        )
        .unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let err = compile_dictionary(&rtx, &dict, GazetteerConfig::default())
            .expect_err("non-string surface element must error, not panic");
        assert!(
            err.downcast_ref::<IndexRowCorrupt>().is_some(),
            "typed corruption error, got: {err:?}"
        );
    }
}
