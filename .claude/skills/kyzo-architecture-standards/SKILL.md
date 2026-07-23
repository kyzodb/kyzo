---
name: kyzo-architecture-standards
description: The engineering standard all Kyzo work is judged against — max purity, type-driven, ontology-first. Load before judging any code, design, seal, story, or waiver against the bar; before choosing between engineering approaches; and whenever deciding if work is "good enough," whether a fallback is acceptable, or what quality floor applies. Not a coding how-to (rust-* skills) and not a workflow.
---

# Kyzo Architecture Standards

The standard is one thing: max purity. Every document, rule, and example here is evidence of the standard, never its boundary. "No written rule forbids it" is not a defense — you know what max purity means for the case in front of you, and that knowledge is the rule.

## The Standard

We build pure type-driven development to max purity. The program is an ontology and a graph of types: model what exists, give each distinct meaning one canonical type, make illegal states unrepresentable at construction — not caught in review, not policed at runtime, not documented as a caveat.

`docs/decisions.md` and `docs/STORAGE-ARCH-STRIPPED.md` are the FLOOR for engineering quality, not the ceiling. The floor is those documents themselves, not any summary of them: read them. Work below their caliber fails the standard.

Always strive for the best, hardest answer, even when it may not work in the long run and forces a fallback. Picking the fallback first is always incorrect, even when the fallback is ultimately the solution.

## Judging

Judge distance from the ideal construction of the truth in question, not distance from the current code. If a type could make the illegal state unrepresentable, that construction is the answer; anything less is a violation. A claim is met only when its enforcement exists — a type, a test, a campaign. Prose is not enforcement. "Fixing this is laborious" is never a defense; "fixing this is architecturally wrong, because X" is the only valid counter, and X must be specific and falsifiable.

## Changes

This file is law for every judge that loads it and is write-locked like the waiver ledger: only the operator changes it. Propose changes to the operator; do not edit.
