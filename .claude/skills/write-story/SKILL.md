---
name: write-story
description: create, review, or revise kyzodb github issues using the kyzodb story contract. use when writing story names, labels, descriptions, condemned blocks, engineering choices, tasks, and definitions of done. enforce explicit database-engine commitments instead of vague cleanup, fake certainty, deferred architecture, or low-ceiling work that merely passes.
---

# Write Story

## Instruction Block

A KyzoDB story is an executable engineering commitment.

A valid story must expose the value change, the condemned path, the hard engineering choice, and the evidence required to close the work.

Force the hard choice when enough information exists. Choose the representation, authority boundary, execution currency, cache invalidation rule, storage contract, ordering invariant, admission path, evaluator rule, algorithm, benchmark, failure path, or evidence boundary.

Do not hide behind flexibility, compatibility, parallel paths, deferred design, cleanup later, or “make it work for now.”

Do not confuse “works” with “correct.” A conservative implementation can be wrong if it preserves accidental complexity, duplicate authority, or a low ceiling. An ambitious engineering choice can be valid even if it fails, when the failure produces real evidence about KyzoDB’s frontier.

Use direct technical language. Replace dramatic quality claims with mechanism, invariant, consequence, and proof.

Every major or minor release gets its own story: its deliverables are the release notes.

## GitHub

- A story is a GitHub issue carrying exactly one of the five labels.
- Epic membership is the **parent issue** relation (the story is a sub-issue of its epic), never a body field.
- Milestone is time only: `Now`, `Next`, or `Later`.

## Story Schema

Use this exact markdown order.

```md
# <Story Name>

Label: <Feature | Bug | Performance | Security | Demo>
Milestone: <Now | Next | Later>

## Description

As a <actor>,
I want <capability, invariant, or decision>,
so that <state of value change>.

## Condemned

Path: <old path, fallback, ambiguity, duplicate authority, compatibility path, escape hatch, accidental complexity, low-ceiling implementation, or deferred design this story rejects>

Reason: <why this path is unacceptable for correctness, determinism, authority, performance, security, demo credibility, or KyzoDB’s ceiling>

Closure test: <how we know the condemned path is removed, bounded, or mechanically rejected>

## Engineering Choice

Choice: <the hard technical commitment this story makes>

Choice type: <Representation | Authority Boundary | Execution Currency | Cache Invalidation | Storage Contract | Ordering Invariant | Admission Path | Evaluator Rule | Algorithm | Benchmark | Failure Path | Evidence Boundary>

Consequence: <what becomes possible, impossible, measurable, or enforceable because of this choice>

Evidence needed: <only for discovery, performance, demo, or evidence-bound stories; otherwise write "None">

## Context

<Relevant engineering context needed to execute and review the story. Include mechanisms, constraints, tests, benchmarks, artifacts, failure modes, open questions, or prior evidence. Do not dump source text unless converting existing messy work.>

## Tasks

- [ ] <task that produces code, test coverage, benchmark evidence, committed artifact, or explicit decision artifact>

## Definition of Done

- [ ] <the value change is present>
- [ ] <the condemned path is removed, bounded, or mechanically rejected>
- [ ] <the engineering choice is implemented, measured, or decided by named evidence>
- [ ] <the result is testable, measurable, or mechanically reviewable>
````

## Field Rules

| Field                                | Rule                                                                                                                                                         |
| ------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `Story Name`                         | Name the domain and value-bearing mechanism. Avoid dramatic quality words.                                                                                   |
| `Label`                              | Must be exactly one of `Feature`, `Bug`, `Performance`, `Security`, or `Demo`.                                                                               |
| `Milestone`                          | Use the GitHub milestone name when the story belongs to an epic. Use `None` only when it does not.                                                           |
| `Description`                        | Must use `As / I want / so that`. The `so that` clause must state a state of value change, not a generic benefit.                                            |
| `Condemned.Path`                     | Must name a concrete rejected path. If the wrong path cannot be identified, the story is not sharp enough.                                                   |
| `Condemned.Reason`                   | Must explain why preserving that path damages the engine or lowers KyzoDB’s ceiling.                                                                         |
| `Condemned.Closure test`             | Must make the condemned path auditable at completion.                                                                                                        |
| `Engineering Choice.Choice`          | Must choose something. Do not restate uncertainty as a decision.                                                                                             |
| `Engineering Choice.Choice type`     | Must select the closest listed type.                                                                                                                         |
| `Engineering Choice.Consequence`     | Must state what changes because the choice is made.                                                                                                          |
| `Engineering Choice.Evidence needed` | May block the final choice only when the story is explicitly about discovery, measurement, demo signal, or performance evidence. Otherwise use `None`.       |
| `Context`                            | Include only context needed to execute and review the story. For cleanup/rewrite work, carry forward relevant engineering obligations without pasting noise. |
| `Tasks`                              | Each task must produce code, tests, benchmark evidence, committed artifacts, or a decision artifact.                                                         |
| `Definition of Done`                 | Must close the value change, condemned path, and engineering choice.                                                                                         |

## Invalid Story Conditions

A story is invalid when any of these are true:

* the value change is vague
* the condemned path is missing, abstract, or unauditable
* the engineering choice does not choose anything
* both paths remain alive without a named reason and closure boundary
* architecture is deferred without exact deciding evidence
* tasks use mood verbs such as improve, harden, polish, finalize, clean up, or ensure
* the Definition of Done cannot prove closure
* quality is performed through language instead of proven through mechanism and evidence