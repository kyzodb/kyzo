---
name: kyzo-architecture-standards
description: The engineering standard all Kyzo work is judged against — max purity, type-driven, ontology-first. Load before judging any code, design, seal, story, or waiver against the bar; before choosing between engineering approaches; and whenever deciding if work is "good enough," whether a fallback is acceptable, or what quality floor applies. Not a coding how-to (rust-* skills) and not a workflow.
---

# Kyzo Architecture Standards

The standard is one thing: max purity. Every document, rule, and example below is evidence of the standard, never its boundary. "No written rule forbids it" is not a defense — if you know what max purity means for the case in front of you, and you do, that knowledge is the rule.

## The Standard

We build pure type-driven development to max purity. The program is an ontology and a graph of types: model what exists, give each distinct meaning one canonical type, and make illegal states unrepresentable at construction — not caught in review, not policed at runtime, not documented as a caveat.

`docs/decisions.md` and `docs/STORAGE-ARCH-STRIPPED.md` are the FLOOR for engineering quality, not the ceiling. Their caliber is the minimum: claims tagged honestly (Unconstructible / Refused / Unexposed — inflating one to another is fraud), every refusal typed and named, every hard claim backed by an executable proof, every constructor consuming evidence rather than validating candidates. Work that could not stand next to those documents does not meet the standard.

Always take the best, hardest answer first, even when it may not survive and will force a fallback later. Picking the fallback first is always incorrect — even when the fallback turns out to be the final solution — because arriving at it through a failed attempt at the ideal produces a ruling with known costs, while starting there produces a surrender with unknown ones.

## Judging Against the Standard

Judge distance from the ideal design, not distance from the current code. The question is never "is this better than what was there" — it is "is this what a max-purity construction of this truth looks like."

A purity question is answered by construction: if a type could make the illegal state unrepresentable, that construction is the answer, and anything less is a gap with a name. A claim is met only when its enforcement exists — a type, a test, a campaign. Prose is not enforcement.

Effort is not a defense. "Fixing this is laborious" never justifies a shortfall; "fixing this is architecturally wrong, because X" is the only valid counter, and X must be specific and falsifiable.

## Extending This Skill

This skill is the seat for engineering standards. Project-specific floors, exemplar documents, and additional rulings belong here — add them under this section rather than scattering them across prompts, so every judge reads one standard.
