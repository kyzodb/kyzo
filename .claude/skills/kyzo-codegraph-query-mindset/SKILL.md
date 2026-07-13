---
name: kyzo-codegraph-query-mindset
description: Use when working in a codebase that has a codegraph (KyzoDB code graph) available and you are about to grep, find, read files, or walk directories to answer a question about the code. Teaches the mindset shift and the KyzoScript (Datalog) crash course specialized to codegraph's relations — one query frequently replaces a whole grep-read-grep loop, and answers questions grep cannot ask at all.
---

# Query the graph, don't grep the tree

The repository you are in has been parsed into a KyzoDB graph: every construct typed,
content-addressed, embedded, placed against the architecture map, and judged against the
project's doctrine. You are very good at Datalog — you just don't reach for it. Reach for it.

## The mindset shift

| You were about to… | One query instead |
| --- | --- |
| grep for a function name | `*ast{symbol: 'parse', …}` — typed hits, no false positives from comments |
| grep, open file, grep again | a join — the "open and look" step IS the join |
| walk directories to map a module | aggregate over `*ast` by `file`/`role` |
| read a file to see if it's test code | `in_test` is a column |
| ask "where else does this pattern occur" | vector search over `construct_vproj`, joined back to `*ast` |
| ask "what's inside this thing, recursively" | two-line recursive rule over `*edge` |
| ask "is this file in the deprecated zone" | `*placement` join — the map is data |

Grep answers "where does this string appear." The graph answers *"which live, non-test
constructs of role X, in zone Y, carrying premise Z, resemble concept W"* — questions grep
cannot even express. When your question has more than one clause, it's a query.

## How to connect

The endpoint is `$CODEGRAPH_KYZO_URL` (e.g. `http://127.0.0.1:9070/text-query`).

```bash
curl -s "$CODEGRAPH_KYZO_URL" -H 'content-type: application/json' \
  -d '{"script": "?[file, symbol, line] := *ast{file, symbol, line, is_construct, valid_to}, is_construct == true, valid_to == \"\" :limit 20", "params": {}}'
```

```python
from codegraph import store
store.q(url, "?[symbol] := *ast{symbol, project, valid_to}, project == $p, valid_to == ''",
        {"p": "myproject"})["rows"]
```

For the common prepared views (claims with evidence spans, purity, the proposals desk), prefer
the MCP tools (`codegraph_claims`, `codegraph_purity`, `codegraph_proposals`). Raw KyzoScript is
for everything they didn't anticipate — which is the point of this skill.

## KyzoScript in five minutes (codegraph dialect)

**A query** binds columns from stored relations and filters:

```
?[file, symbol] := *ast{file, symbol, role, is_construct, valid_to},
                   role == 'function', is_construct == true, valid_to == ""
```

- `*ast{col, col2: var}` — bare name binds a variable of that name; `col: var` renames;
  `col: 'literal'` filters in place.
- Conditions are just more clauses: `line > 100`, `starts_with(file, 'src/')`,
  `is_in(kind, ['added','removed'])`.
- Params: `$p` in the script, supplied in `params`.
- Tails: `:limit 50`, `:order -line` (descending), `:order line`.
- Aggregation in the head: `?[file, count(id)] := *ast{id, file, valid_to}, valid_to == ""`.
- Negation: `not *claim{subject: c, valid_to: ""}` — "c carries no live claim".

**LIVENESS — the one convention you must never forget.** Versioned relations (`ast`,
`source_file`, `claim`, `rule`, `concept`, `zone_ctx`) keep every version forever;
`valid_to == ""` selects the live one. A query without that filter reads all of history
(sometimes that's exactly what you want — see the accountability skill).

**Rules compose, and recursion is native.** Everything under a construct, any depth:

```
under[n] := *edge{src: $root, dst: n, kind}, kind == 'child'
under[n] := under[m], *edge{src: m, dst: n, kind}, kind == 'child'
?[symbol, native_kind] := under[n], *ast{id: n, symbol, native_kind}
```

The second `under` rule feeds on itself. This shape — seed, extend, harvest — answers
containment, reachability, and blast-radius questions in three lines.

**Vector search is a relation.** Get an embedding, then the HNSW index binds like any table:

```python
from codegraph import embedder
v = embedder.embed(["a public constructor that validates nothing"])[0]
store.q(url, """
?[file, symbol, dist] := ~construct_vproj:sim{node | query: vec($v), k: 10, bind_distance: dist},
                         *ast{id: node, file, symbol, valid_to}, valid_to == ""
:order dist
""", {"v": v})
```

That join back to `*ast` is the move: the semantic hit becomes structural rows you can filter
by zone, role, test-ness, claims — anything. See `kyzo-codegraph-investigations` for the
combined-query recipes that make this system unlike anything else you have used.

## Self-discovery — when unsure, ask the store

```
::relations          — every relation present
::columns ast        — the exact columns of one relation
```

Column lists and vocabularies live in `kyzo-codegraph-schema`. Never guess a column name;
discover or look it up — a wrong name is a refusal, never a silent wrong answer.

## Habits of a good graph citizen

- Filter by `project == $p` when the store might host more than one project.
- `:limit` exploratory queries; the ast relation can hold ~100k rows.
- Ids are typed by prefix: `a:` node, `k:` construct/subject cid, `cl:` claim, `r:` rule,
  `k:bad:`/`k:good:` concepts, `law:` doctrine digest. If you have an id, its prefix tells you
  which relation to join.
- Reads are always safe. Writes go through the codegraph doors only — never `:put`/`:rm`
  codegraph relations by hand; you would be forging records the system treats as witnessed.
