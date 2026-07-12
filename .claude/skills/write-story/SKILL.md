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

## The story is the executor's entire authority

A KyzoDB story is executed by an agent that has exactly two doors: **execute the
story exactly, or stop and name the blocker to the orchestrator before doing
anything else.** There is no third "figure it out myself" door — that door is
re-derivation, and re-derivation is avoidance wearing the costume of diligence.
Thinking lives upstream: in this story and in the orchestrator. The agent is an
actuator. See the `spec-authority` skill for the full operating model.

Everything that follows exists to make the story executable **without
re-derivation.** If executing it requires the agent to re-discover a root cause
the author already knew, re-confirm a decision already made, or hunt the
codebase for where a fix goes, the story is under-written — and the cost of that
gap is not abstract.

### The failure this prevents: the token-burn catastrophe

An under-written story once cost **≈307 million tokens for a ~250-line change.**
The story had already diagnosed every fix and named every file. The agent
ignored that and re-derived it anyway: it `sed`- and `cat`-dumped whole source
files and 500 lines of vendored crate source into a context that grew to 920K
tokens and never got cleared, then re-paid that near-full context across **632
API calls.** The diff was tiny; the exploration to re-reach it was enormous and
resident. The waste looked like diligence — motion through the codebase that
substitutes for applying the fix already handed over.

A well-written story is the primary defense against this. It does not describe a
destination and invite the agent to find the path; it **points at the path.**
Three research-grounded properties, each of which directly starves the failure:

1. **Point at a reference.** Name the concrete artifact the agent works against
   — the exact file, module, pattern, or spec — so it goes straight there
   instead of exploring. "Compare against `crates/…/canonical.rs`," not "find
   where the codec lives." Naming the reference is the specific cure for
   over-exploration.
2. **Name the verification gate.** State the exact command or named check that
   proves done — as a Definition-of-Done item that names the real gate (e.g.
   the container seal command), so completion is verified, not self-attested,
   and a failed gate triggers report-not-workaround. Do not invent a gate you
   cannot confirm is real; if the true gate is unknown, mark it `[OPEN]`.
3. **Mark decided vs open.** Everything the story decides is immutable and the
   agent executes it; anything genuinely still to be designed is marked `[OPEN]`
   with who decides, and the agent escalates it — it never resolves an open
   question by improvising code. An unmarked open question is an invitation to
   improvise, which is the failure by another name.

Do not manufacture false specificity to satisfy this. For forward-looking design
work whose code does not yet exist, the "reference" is the spec and the cited
frontier atoms, and the genuinely-open sub-decisions are marked `[OPEN]`, not
invented. False precision — a named file that is wrong — is worse than honest
openness, because it sends the agent to the wrong place with confidence.

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

Use this exact markdown order. The body opens with the parent-epic cross-link
and a one-sentence lede, then the **human-narrative zone** (Description through
Engineering Choice), then the **executor-contract zone** (Context, Tasks,
Definition of Done). Condemned is a `> [!WARNING]` callout, Ceiling is a table,
Context is wrapped in `<details>`, and each task carries an append-only `T#`
identifier; the remaining field labels are bold `**Label:**` markers that are
both visual skeleton and parser anchors.

```md
# <Story Name>

**Epic:** #<parent-epic-number>

<one plain-language sentence — the lede — introducing this story for a human reader>

## Description

As a <actor>,
I want <capability, invariant, or decision>,
so that <state of value change>.

## Sources

- **<source authority>:** <its asserted property, one line>
- ...every source this story serves; the planning graph is the registry.

## Condemned

> [!WARNING]
> **Path:** <old path, fallback, ambiguity, duplicate authority, compatibility path, escape hatch, accidental complexity, low-ceiling implementation, or deferred design this story rejects>
>
> **Reason:** <why this path is unacceptable for correctness, determinism, authority, performance, security, demo credibility, or KyzoDB's ceiling>
>
> **Closure test:** <how we know the condemned path is removed, bounded, or mechanically rejected>

## Ceiling

| Maximum | Chosen | Constraint |
| --- | --- | --- |
| <the full-height option the cited sources assert — never invented downward> | <this story's commitment> | <"equal — chosen IS the maximum", or the named measured constraint that forces less, with where that measurement lives> |

## Engineering Choice

**Choice:** <the hard technical commitment this story makes; when it decomposes into parts, write them as a real numbered list, never an inline (1)(2)(3) chain>

**Choice type:** <Representation | Authority Boundary | Execution Currency | Cache Invalidation | Storage Contract | Ordering Invariant | Admission Path | Evaluator Rule | Algorithm | Benchmark | Failure Path | Evidence Boundary>

**Consequence:** <what becomes possible, impossible, measurable, or enforceable because of this choice>

**Evidence needed:** <only for discovery, performance, demo, or evidence-bound stories; otherwise "None">

## Context

<details>
<summary>Execution context</summary>

<Only what is needed to execute and review: mechanisms, constraints, tests, benchmarks, artifacts, failure modes, prior evidence. Point at references by exact name — the file, module, pattern, or spec the executor compares against — so it reads the named slice, never the codebase. Mark any genuinely-undecided sub-decision `[OPEN]` with who decides.>

</details>

## Tasks

- [ ] T1 — <task that produces code, test coverage, benchmark evidence, committed artifact, or explicit decision artifact>

## Definition of Done

- [ ] <the value change is present>
- [ ] <the condemned path is removed, bounded, or mechanically rejected>
- [ ] <the engineering choice is implemented, measured, or decided by named evidence>
- [ ] <every Sources entry is satisfied here or explicitly re-homed to a named story>
- [ ] <the result is provable by the named verification gate — the exact command or check, e.g. the container seal — not self-attested>
```

`T#` identifiers are append-only: assigned once, never renumbered when a task is
inserted or removed; a new task takes the next unused integer. They are the
handle the task-completion-judge checks off, so every task line carries one.

## Rendering for humans

A story serves three readers at once: the operator building shared
understanding, the agent bound by its commitments, and the parser enforcing
its shape. Formatting is not decoration — it is the shared-understanding
layer, and the same structure serves all three: a bold field label is a
visual anchor, a parse marker, and an addressable obligation; a numbered
commitment is scannable and citable in correction ("pre-commitment 2 says
…"); a `<details>` block gives the human a skimmable surface and the agent
the exhaustive inventory without forking the truth.

A story body is read far more often than it is written; shape it for the
scanning eye, not the parser's convenience:

- **Lists over chains** — any enumeration living inline as `(1)… (2)… (3)…`
  becomes a markdown numbered list. Field values may span lines; use that.
- **Structure the Context** — it is the longest section and must not be a wall:
  `###` sub-heads per topic, a table when the evidence is tabular (job → cause
  → fix), and a `<details>` block for bulk inventories the reader expands on
  demand.
- **Backtick every identifier** — paths, commands, crate names, flags, RUSTSEC
  ids. A bare path in prose is invisible; `crates/xtask` is an anchor.
- **One clause per task** — a checkbox packing three actions hides partial
  progress. Split compound tasks unless their halves are inseparable evidence.
- Never bold anything except the schema's field labels and true emphasis;
  decoration that isn't structure is noise.

The new-schema elements each carry a specific legibility job:

- **Two zones** — the human-narrative zone (lede → Engineering Choice) reads as
  prose for the operator; the executor-contract zone (Context, Tasks, Definition
  of Done) reads as machine spec. Same fidelity, kept apart so neither degrades
  the other.
- **Condemned is a `> [!WARNING]` callout** — the killed path renders as an
  alarm the eye cannot skip.
- **Ceiling is a table** — Maximum | Chosen | Constraint side by side, so a
  Chosen below Maximum without a named Constraint is visible at a glance.
- **`<details>` wraps Context** — the longest section collapses, giving the
  human a skimmable surface and the agent the full inventory without forking the
  truth.
- **Cross-link the graph** — the `**Epic:** #N` line makes the parent navigable
  inline; GitHub renders it as a live link.
- **Lede first** — one plain sentence at the very top a stranger reads before
  any structure.

The board is public — outside engineers and investors read it cold. The show
is utility: a story a stranger can scan in thirty seconds and an agent cannot
escape IS the demonstration of the planning system. Never add content aimed
at an audience; polish the working artifact until it demonstrates itself.

## The story feeds a three-agent pipeline

A story is consumed by three agents in sequence, and specific fields are their
handles — write each field for its consumer:

- **The demolition agent** reads the **Condemned** block to clear the
  implementation surface before construction. Write Condemned specific enough to
  act on: name the concrete files, symbols, adapters, tests, and call paths to
  remove, not an abstract "the old approach." A vague Condemned block leaves the
  demolition agent nothing it can safely delete.
- **The development-task agent** executes one task at a time; every task carries
  a `T#` identifier, the handle it and the judge address it by.
- **The task-completion-judge** checks a task off only against the **Definition
  of Done** item that names the exact verification-gate command. One DoD item
  must name that gate, or the judge has nothing to verify completion against.

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
| `Context` | Only execution/review context; carry forward obligations without pasting noise. Every reference is named by exact path/module/pattern/spec so the executor reads the named slice, never explores. Every genuinely-undecided sub-decision is marked `[OPEN]` with who decides. |
| `Tasks` | Each produces code, tests, benchmark evidence, committed artifacts, or a decision artifact. |
| `Definition of Done` | Closes the value change, the condemned path, the engineering choice, and every source, and one item names the exact verification gate that proves closure. If you cannot name how done is checked, the story is not sharp enough to execute. |

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
* **executing it requires re-derivation** — the agent would have to
  re-discover a root cause, re-confirm a made decision, or hunt the codebase
  for where a fix goes, because the story names no reference to work against
* **its Definition of Done names no verification gate** — no command or check
  proves it done, so completion could only be self-attested
* **it leaves a live open question unmarked** — a genuinely-undecided
  sub-decision is written as if decided, or omitted, inviting the agent to
  resolve it by improvising code instead of escalating it as `[OPEN]`
* **it manufactures false specificity** — a named file, line, or fix that is
  not actually true, sending the executor to the wrong place with confidence
