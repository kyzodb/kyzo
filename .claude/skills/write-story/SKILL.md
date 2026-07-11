---
name: write-story
description: create, review, or revise kyzodb github issues using the kyzodb story contract. use when writing story names, descriptions, source citations, condemned blocks, ceiling checks, engineering choices, tasks, and definitions of done. enforce explicit database-engine commitments at the cited sources' full height instead of vague cleanup, fake certainty, deferred architecture, or low-ceiling work that merely passes.
---

# Write Story

A KyzoDB story is an executable engineering commitment. A valid story exposes the
value change, its sources, the condemned path, the ceiling check, the hard engineering
choice, and the evidence required to close the work.

Do not confuse "works" with "correct." A conservative implementation can be wrong if it
preserves accidental complexity, duplicate authority, or a low ceiling. An ambitious
engineering choice is valid even when it fails, when the failure produces real evidence
about KyzoDB's frontier — the Ceiling section is that doctrine made auditable.

Use direct technical language: mechanism, invariant, consequence, proof — never
dramatic quality claims. Every major or minor release gets its own story: its
deliverables are the release notes.

## GitHub and the board

- A story is a GitHub issue carrying exactly one classification label —
  `Feature`, `Bug`, `Performance`, `Security`, or `Demo` — as the GitHub label
  itself, never restated in the body (a body copy is a stored twin that goes
  stale on the first `reclassify_story`).
- Epic membership is the **parent issue** relation (the story is a sub-issue of
  its epic), never a body field.
- **Horizon lives in exactly one place: the parent epic's column** (`Now` /
  `Next` / `Later`). A story never carries its own horizon — no milestone, no
  body field, no label. A story's horizon is a derived read through its parent.
  Milestones do not exist on this board.
- All board writes go through the manage-board MCP tools, never raw `gh`.

## Story Schema

Use this exact markdown order.

```md
# <Story Name>

## Description

As a <actor>,
I want <capability, invariant, or decision>,
so that <state of value change>.

## Sources

- <atom/desideratum id> — <its asserted property, one line>
- ...every source this story serves; the planning graph is the registry.

## Condemned

Path: <old path, fallback, ambiguity, duplicate authority, compatibility path, escape hatch, accidental complexity, low-ceiling implementation, or deferred design this story rejects>

Reason: <why this path is unacceptable for correctness, determinism, authority, performance, security, demo credibility, or KyzoDB's ceiling>

Closure test: <how we know the condemned path is removed, bounded, or mechanically rejected>

## Ceiling

Maximum: <the full-height option the cited sources assert — quoted or derived from them, never invented downward>

Chosen: <this story's commitment>

Constraint: <exactly one of: "equal — chosen IS the maximum", or the named measured constraint that forces less, with where that measurement lives>

## Engineering Choice

Choice: <the hard technical commitment this story makes>

Choice type: <Representation | Authority Boundary | Execution Currency | Cache Invalidation | Storage Contract | Ordering Invariant | Admission Path | Evaluator Rule | Algorithm | Benchmark | Failure Path | Evidence Boundary>

Consequence: <what becomes possible, impossible, measurable, or enforceable because of this choice>

Evidence needed: <only for discovery, performance, demo, or evidence-bound stories; otherwise "None">

## Context

<Only what is needed to execute and review: mechanisms, constraints, tests, benchmarks, artifacts, failure modes, prior evidence.>

## Tasks

- [ ] <task that produces code, test coverage, benchmark evidence, committed artifact, or explicit decision artifact>

## Definition of Done

- [ ] <the value change is present>
- [ ] <the condemned path is removed, bounded, or mechanically rejected>
- [ ] <the engineering choice is implemented, measured, or decided by named evidence>
- [ ] <every Sources entry is satisfied here or explicitly re-homed to a named story>
- [ ] <the result is testable, measurable, or mechanically reviewable>
```

## Field Rules

| Field | Rule |
| --- | --- |
| `Story Name` | Name the domain and value-bearing mechanism, in Title Case. No dramatic quality words. |
| `Description` | `As / I want / so that`; the `so that` clause states a state of value change, not a generic benefit. |
| `Sources` | Every atom/desideratum this story serves, by id, with its one-line asserted property. These are the height reference the Ceiling is judged against. |
| `Condemned.Path` | A concrete rejected path. If the wrong path cannot be identified, the story is not sharp enough. |
| `Condemned.Reason` | Why preserving that path damages the engine or lowers KyzoDB's ceiling. |
| `Condemned.Closure test` | Makes the condemned path auditable at completion. |
| `Ceiling.Maximum` | The full-height option the Sources assert. Written from the sources, so falsifying it means falsifying them — side by side. |
| `Ceiling.Chosen` | The commitment. When it is less than Maximum without a real Constraint, the story is a counterfeit. |
| `Ceiling.Constraint` | `equal`, or a named measured fact (with its location) — never a feeling, a schedule, or "pragmatism". A legitimate retreat is a recorded measurement. |
| `Engineering Choice.Choice` | Chooses something. Restated uncertainty is not a decision. |
| `Engineering Choice.Choice type` | The closest listed type. |
| `Engineering Choice.Consequence` | What changes because the choice is made. |
| `Engineering Choice.Evidence needed` | May block the final choice only for discovery, measurement, demo-signal, or performance stories; otherwise `None`. |
| `Context` | Only execution/review context; carry forward obligations without pasting noise. |
| `Tasks` | Each produces code, tests, benchmark evidence, committed artifacts, or a decision artifact. |
| `Definition of Done` | Closes the value change, the condemned path, the engineering choice, and every source. |

## Banned Lexicon

Two mechanically greppable bans:

- **Mood verbs** — improve, harden, polish, finalize, clean up, ensure — banned
  in Tasks.
- **Escape-hatch phrases** — "for now", "initially", "fall back" / "fallback",
  "if needed", "if this proves too hard", "phase 2", "we can later",
  "optionally", "as a first step" — banned in **every section except inside the
  Condemned block, where they name the thing being killed**. Anywhere else,
  such a phrase IS a live escape hatch: the story is smuggling out the hard
  work it claims to commit to.

## Invalid Story Conditions

A story is invalid when any of these are true:

* the value change is vague
* the Sources are missing, or the commitment is lower than what the cited
  sources assert without a named measured Constraint in the Ceiling
* the condemned path is missing, abstract, or unauditable
* the Ceiling's Maximum is invented below what the sources assert — the
  counterfeit condition
* the engineering choice does not choose anything
* both paths remain alive without a named reason and closure boundary
* architecture is deferred without exact deciding evidence
* banned lexicon appears outside the Condemned block
* the Definition of Done cannot prove closure, or drops a source silently
* quality is performed through language instead of proven through mechanism
  and evidence
