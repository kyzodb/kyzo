---
name: kyzo-codegraph-investigations
description: Use when you need to actually understand a codebase that has a codegraph — its shape, its hotspots, where a pattern lives, what resembles what, what violates the architecture and where. A cookbook of combined queries (structure × meaning × zone × judgment in ONE statement) that no grep, LSP, or single-modality tool can express.
---

# Investigations: the combined-query cookbook

The unique power of this system is that the AST, the vectors, the architecture map, the claims,
and all of history live in **one store** — so one query can cross all of them at one snapshot.
Each recipe below is a question you would otherwise answer with a long tool loop (or not at
all), answered in one statement. Substitute `$p` with the project name throughout; add
`:limit` while exploring.

## Shape: learn a codebase in four queries

```
?[file, count(id)] := *ast{id, file, is_construct, valid_to}, is_construct == true, valid_to == ""
:order -count(id) :limit 15                       # where the mass is

?[role, count(id)] := *ast{id, role, is_construct, valid_to}, is_construct == true, valid_to == ""
                                                  # the codebase's texture: fn/type/impl ratios

?[zone, eligible, tainted] := *purity_zone{zone, at, eligible, tainted}, at == $latest
                                                  # health per architecture zone

?[flag, count(node)] := *structural_fact{node, flag}   # which premises the code exhibits, ranked
:order -count(node)
```

## Find by meaning, filter by structure (the signature move)

"Show me everything that *resembles* an unchecked public constructor — but only live,
non-test code, and tell me its zone":

```python
v = embedder.embed(["public constructor performing no validation"])[0]
```
```
?[file, symbol, zone, dist] :=
    ~construct_vproj:sim{node | query: vec($v), k: 25, bind_distance: dist},
    *ast{id: node, file, symbol, is_construct, in_test, valid_to},
    is_construct == true, in_test == false, valid_to == "",
    *placement{node, zone}
:order dist :limit 10
```

Vector databases can't do the join; LSPs can't do the similarity. This is one statement here.
Variants: swap the `*placement` clause for `zone == 'app/legacy'` (only the deprecated zone),
or add `not *claim{subject: cid, valid_to: ""}` after binding `cid` from ast — *resembles the
pattern but carries no claim yet* = the detection gap, quantified.

## Structure: recursion over the graph

Everything inside a construct, any depth (blast-radius shape). Seed `$root` with a node that
has children — a file's `source_file` root, a module, an impl:

```
under[n] := *edge{src: $root, dst: n, kind}, kind == 'child'
under[n] := under[m], *edge{src: m, dst: n, kind}, kind == 'child'
?[file, symbol, native_kind] := under[n], *ast{id: n, file, symbol, native_kind}
```

Types and every impl that serves them (the `impls` projection):

```
?[type_sym, impl_file, impl_line] :=
    *edge{src: i, dst: t, kind}, kind == 'impls',
    *ast{id: t, symbol: type_sym}, *ast{id: i, file: impl_file, line: impl_line}
```

## Duplication, exactly

Same full-body hash, different construct identity — literal copy-paste, no similarity
hand-waving (or query the pre-derived premise directly):

```
?[file, symbol, twin] := *structural_fact{node, flag, detail: twin}, flag == 'exact_duplicate',
                         *ast{id: node, file, symbol, valid_to}, valid_to == ""
```

## Architecture drift, named and counted

Constructs still living in deprecated zones, with what's wrong with each:

```
?[zone, file, symbol, concept] :=
    *zone_ctx{zone, map_status, valid_to}, map_status == 'deprecated', valid_to == "",
    *placement{node, zone},
    *ast{id: node, file, symbol, cid},
    *claim{subject: cid, concept, standing, valid_to: cvt}, standing == 'affirmed', cvt == ""
```

Where must that code go? The map already says: `?[src, dst] := *migration{src, dst}`.

The unmapped frontier — construct-bearing files the architecture map has no opinion about
(often the most interesting list in the whole repo):

```
?[file, count(id)] := *ast{id, file, is_construct, valid_to}, is_construct == true,
                      valid_to == "", not *placement{node: id}
:order -count(id)
```

## Judgment as data

The judged lane's history is queryable knowledge. Where has the model *confirmed* flaws
(assents with their reasons):

```
?[file, symbol, reason] := *examination{subject, privation, reason}, privation == true,
                           *ast{cid: subject, file, symbol, valid_to}, valid_to == ""
```

Assent rate per rule — which questions are worth asking (near 1.0: the miner can make it law;
near 0.0: the gate over-nominates). Aggregate args must be bound variables, so bind first:

```
?[rule, count(subject), sum(y)] := *examination{rule, subject, privation}, y = to_int(privation)
```

## Cross-project note

Everything above composes further: any recipe can be re-run **as of an instant** (see
`kyzo-codegraph-accountability` for the time patterns) or restricted by any other clause.
When a question feels like it needs two tools, it's one query with one more join — write the
join.
