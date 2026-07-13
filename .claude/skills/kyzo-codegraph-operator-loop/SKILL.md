---
name: kyzo-codegraph-operator-loop
description: Use when running the codegraph loop itself — ingesting, updating after edits, reading reports, working the proposals desk, editing doctrine or the architecture map, or invoking the judge/miner/rewitness doors. Teaches what each report field means, which actions cost model bits or re-parses, and the settle discipline an agent must never violate.
---

# Running the loop

You are operating a measuring instrument. The prime directive: **the instrument's honesty is
worth more than any single result** — never force a number, never settle a proposal without
authorization, never write to the store except through the doors.

## The round

After any code change: `codegraph_update`. It is a typed diff — unchanged files cost nothing,
and the round ends in a measurement. Read the report in this order:

1. `purity` + `purity_direction` — the headline. `▲` toward the target, `▼` away, `·`
   unchanged, `∅` first measurement, **`≠` = the LAW changed, so do not compare this score to
   the previous one as if the code moved.**
2. `files_touched / removed`, `constructs_added / modified / removed` — the diff's shape.
   Sanity-check it against what you actually edited; a surprise here is a finding.
3. `claims_superseded / claims_added` — judgments that fell with their evidence or landed on
   new code. Interrogate additions via `codegraph_claims` before reacting.
4. `suspects`, and on the purity event `examined` — open questions the gates have nominated.
5. `debt / debt_covered` — unexpanded vs expanded macro surface (Rust). `expand_failures` is a
   recorded degrade, never silence.
6. `stale_generation` — records parsed by an older tool release. Only the deliberate
   `codegraph_rewitness` door cures this (full re-parse + re-embed: expensive by design —
   surface it to the human, don't fire it casually).

A doctrine or map edit needs **no code change**: run update with the new law; it re-places and
re-fires from stored state with zero re-parsing, and honestly reads `≠`.

## What costs what

| action | cost |
| --- | --- |
| update, unchanged files | ~free (hash gate) |
| update, touched files | parse + embed only the new constructs |
| doctrine/map edit + update | derivation only — zero parsing |
| `codegraph_judge` | REAL model bits: ≤ `judge_budget` per round, one bit per question, cached forever per content; deferred backlog is picked up next round |
| `codegraph_mine` | one model call, only when examination pressure ≥ `miner_trigger` |
| `codegraph_rewitness` | full re-parse + re-embed of the project — deliberate, operator-priced |

## The desk, and the settle discipline

`codegraph_proposals` shows everything awaiting judgment: judged/vector claims with the
question asked, the evidence span, the model's reason, and the rule's real assent rate; mined
rules with their gates and premises. The desk shows numbers, never recommendations — and so do
you, unless authorized:

- **Never affirm or reject a claim, settle a rule, or promote a rule into the overlay without
  explicit human instruction or a standing delegation covering that action.** Settles enter
  the score and record `by` — whoever settles is accountable by name. When delegated, put the
  delegation in `by`/`reason` (e.g. `by: "agent:claude-for-kyle"`, reason states the basis).
- A settle re-measures immediately — expect the number to move at the door, and report it.
- Rejecting is as valuable as affirming: it is recorded, it tunes assent rates, and it feeds
  the miner truthful pressure. Don't rubber-stamp yes.
- A mined rule fires nothing until settled AND promoted (`codegraph_promote_rule` writes it
  into the project's overlay file — a real file edit the human owns).

## Editing the law

The overlay (`CODEGRAPH_OVERLAY` JSON) and the map (`CODEGRAPH_MAP` Mermaid) are the human's
files: propose diffs, don't silently rewrite. Mechanics that will save you a refused load:

- Overlay records replace shipped base records **whole by id**; `tombstones` disable base
  rules; detector config replaces whole, never patches.
- Every deprecated zone MUST carry a `migrates to` edge and a `reason:` line — dangling
  deprecation is unrepresentable and the reader refuses with the offence named.
- The loader refuses law that can never fire: gates no adapter emits (check `*vocabulary`),
  zone scopes covering nothing on the map, thresholds beyond the snippet bound. A refusal
  names every offender — read it, fix the law, don't fight the door.
- Loop switches live on detector config: `judge_residuals`, `judge_budget`,
  `mine_when_triggered`, `miner_trigger` — data, not flags.

## Hard rails

- `codegraph_purge` is schema-level destruction of EVERYTHING at the KyzoDB URL. Only on an
  explicit, just-given human instruction naming the project; when in doubt, refuse and ask.
- Never point tests or experiments at a production KyzoDB URL; integration work belongs on a
  throwaway instance.
- Never `:put`/`:rm` codegraph relations directly — every record carries its asserter, and a
  hand-written row is a forged witness. Reads: unlimited. Writes: doors only.
- Report what the instrument says, including the parts that look bad — `suspects` high,
  `debt` nonzero, `▼`. A flattering summary that omits the companions is a misreading.
