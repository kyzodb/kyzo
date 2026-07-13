---
name: kyzo-codegraph-schema
description: Use when writing any raw KyzoScript query against a codegraph store and you need exact relation names, columns, id forms, enum vocabularies, or the liveness/time conventions. The authoritative field reference — a wrong column name refuses; this is where the right ones live.
---

# The codegraph schema

Every relation, exactly as derived from the typed records. Versioned relations (marked ⏱)
carry `status, valid_from, valid_to`; live rows have `valid_to == ""`. Envelope columns
(`provenance, authority, status, valid_from, valid_to`) are omitted below for brevity but are
always queryable on ⏱ relations.

## The code

| relation | key | columns (beyond envelope) |
| --- | --- | --- |
| `project` | name | `name, languages` |
| `source_file` ⏱ | project, file, valid_from | `project, file, language, file_hash` |
| `ast` ⏱ | id, valid_from | `id, project, file, language, native_kind, role, symbol, line, end_line, byte_start, cid, content_sha, prefix, is_construct, in_test, generation` |
| `edge` | project, src, dst, kind | `project, src, dst, kind` — kind ∈ `child`, `impls` |
| `change_event` | project, cid, at | `project, cid, at, kind, file, symbol, role, from_sha, to_sha` — kind ∈ `added`, `modified`, `removed` |

Notes: `role` is the language-neutral kind (`function`, `type`, `impl`, `module`, …);
`native_kind` is the tree-sitter kind (`function_item`, `enum_item`, …). `cid` is the
position-independent construct identity (stable across edits); `id` is version-specific.
`prefix` is a normalized snippet (≤180 chars) — display/matching material, never identity.
`content_sha` hashes the full body.

## The judgment

| relation | key | columns |
| --- | --- | --- |
| `claim` ⏱ | id, valid_from | `id, project, subject, concept, rule, rule_from, evidence_node, evidence_sha, zone, tier, standing, reason, similarity, premises` |
| `structural_fact` | project, node, flag, detail | `project, node, flag, witness, detail` — witness ∈ `adapter`, `derivation`, `placement`, `expansion` |
| `examination` | project, rule, subject, sha, at | `project, rule, subject, sha, privation, reason, judge, at` |
| `adjudication` | project, claim, at | `project, claim, at, standing, authority, by, reason` |
| `rule_adjudication` | project, rule, at | `project, rule, at, standing, authority, by, reason` |

Vocabularies: `tier` ∈ `demonstrated, judged, vector, emergent`. `standing` ∈
`proposed, affirmed, rejected`. `authority` ∈ `parser, embedder, model, engine, operator`.
A claim's `subject` is a cid (`k:…`); `evidence_node` is a node id (`a:…`) whose content is
pinned by `evidence_sha`. `rule_from` is the VERSION of the rule that fired.
`examination.privation` — true = the judge said yes (the flaw is present); the
(rule, subject, sha) triple is the never-ask-twice cache key.

## The law

| relation | key | columns |
| --- | --- | --- |
| `concept` ⏱ | id, valid_from | `project, id, term, polarity, description, standing` — polarity ∈ `good`, `bad` |
| `rule` ⏱ | id, valid_from | `id, project, zones, standing, premises, tier, gates, concept, question, judge, threshold, escalation, breadth, kyzoscript` — union: demonstrated rules use `gates+concept`; judged add `question+judge`; vector use `threshold/escalation/breadth+judge` |
| `zone_ctx` ⏱ | project, zone, valid_from | `project, codebase, zone, path, truth, map_status, reason, target` — map_status ∈ `current, deprecated, target` |
| `migration` | project, src, dst | derived from deprecated zones' migrates-to edges |
| `vocabulary` | source, flag | every flag the running tool can emit, per source language + `derived` |
| `placement` | project, node | `project, node, zone` — a rebuildable projection; UNPLACED = absent row |

`gates`, `zones`, `premises` are JSON-encoded lists in their columns. Zone identity is
codebase-qualified (`app/legacy`); `path` is the tree prefix placement matches.

## The measurement

| relation | key | columns |
| --- | --- | --- |
| `purity` | project, at | `project, at, doctrine_digest, score, constructs, bad, good, suspects, examined, debt, debt_covered` |
| `purity_zone` | project, at, zone | `project, at, zone, doctrine_digest, eligible, tainted` |

`doctrine_digest` (`law:…`) names the exact composed law that took the measurement — rows with
different digests were taken by different instruments. `debt` = macro sites without expansion
coverage; `debt_covered` = with.

## The vectors

| relation | key | columns |
| --- | --- | --- |
| `construct_vproj` | project, node | `project, node, emb, model` — HNSW index `construct_vproj:sim` |
| `concept_vproj` | concept, model | `concept, model, emb` — HNSW index `concept_vproj:sim` |

Query form: `~construct_vproj:sim{node | query: vec($v), k: 10, bind_distance: dist}`.
Similarity ≈ `1 - dist` (cosine). `model` stamps the embedder identity — vectors from
different models never silently mix.

## Id prefixes

`a:` ast node (version-specific) · `k:` construct cid / claim subject · `cl:` claim ·
`r:` rule · `k:bad:` / `k:good:` concept · `law:` doctrine digest ·
`exam:` premise reference on mined rules.

## Time, in one box

- **Live view:** `valid_to == ""`.
- **As of instant $t:** `valid_from <= $t, (valid_to == "" || valid_to > $t)` — timestamps are
  ISO-8601 strings and compare lexicographically.
- `status` tells you HOW a version closed: `superseded` (replaced by a newer truth) vs
  `invalidated` (its file left the tree). Nothing is ever deleted.
