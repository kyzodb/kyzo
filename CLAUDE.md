# CLAUDE.md — KyzoDB

KyzoDB is a pure-Rust database engine: relational, graph, vector, full-text, geospatial, and temporal data on one ordered substrate (`fjall`), one query language (KyzoScript Datalog). `README.md` defines the product; the `KyzoDB Work` board defines the plan. CozoDB fork (`FORK.md`); licensing map in `LICENSING.md`.

## The one law

Every stored value encodes to bytes whose binary order equals its semantic order — what lets one substrate serve every query model. A sort-order defect is silent wrong answers, so this contract is executable law, enforced mechanically (property tests, corruption harnesses, mutation, fuzzing). Never weaken that enforcement.

## Authority order

Higher wins: (1) this file, (2) `.claude/rules/*.md`, (3) the focus story and its ruling, (4) `architecture-map` placement, (5) existing code and tests, (6) upstream Cozo. Convenience, release pressure, old tests, and Cozo precedent never override.

## Hard lessons (earned)

- **Decide; don't punt.** You are the architect: when the record settles a question, rule it and continue. Only a true blocker, impossibility, or two laws in genuine contradiction returns to the operator. Punting a decidable question is risk-transfer, not rigor (this deleted the architect agent — 6005370).
- **The board carries ALL work.** Obligations not written on a story die with the next context clear: a DoD item with no task gets a task on sight; armed background witnesses get recorded on the story.
- **Testimony is never the meter.** Executors sound finished and argue confidently; only the judge-checked box and git refs count. Whip the first tell, kill the second, never negotiate.
- **A requirement is never satisfied by shrinking it.** If it's wrong, say so and leave it unsatisfied — never present a narrowed version as done.

## The work loop

The board is the plan. Every board write goes through the manage-board MCP tools, never raw `gh`. One epic at a time on one branch (`start_epic`/`start_story`/`finish_epic` gates — never sidestepped). No engine edits without an open focus story. Move cards as reality changes; commit each verified unit.

The story pipeline (authority lives in tool grants):

1. **demolition** first — cuts the whole Condemned block; a red tree is by design (green is a `main` invariant, reconciled at merge), and shrinking the cut to stay green is the forbidden compromise. Skipping is legal only on a stated, evidence-backed clean-surface finding.
2. **development-task** — one fresh agent per T#, spawned with the full contract pasted verbatim + a file allowlist. No board tools; its only exit is the completion form.
3. **task-completion-judge** — alone checks the box.

**Babysit every spawn.** Arm this Monitor immediately (substitute `<NAME>`):

`f=""; while [ -z "$f" ]; do f=$(find /home/kyle/.claude/projects -path '*subagents*' -name 'agent-a<NAME>-*.jsonl' 2>/dev/null | head -1); [ -z "$f" ] && sleep 2; done; tail -n +1 -f "$f" | jq --unbuffered -rc 'select(.message.content)|.message.content[]?|select(.type=="tool_use")|"\(.name): \(.input.file_path // .input.command // .input.pattern // .input.query // "")"' | awk '{print substr($0,1,150); fflush()}'`

Whip via SendMessage on the FIRST tell — off-allowlist read/edit, forbidden target, self-reverts, spawning a sub-agent, no-op spin, idle with the box unchecked, board tools via ToolSearch, mechanism mismatch — never rehearsing its reasons. Repeat tell or anything destructive: `TaskStop`. Once the judge or the named command rules, stop — re-validation is your own fear-read.

## Verification

All cargo/tests run in the container (`kyzo-dev`; `kyzo-bench` for benches) — never natively, never hand-set ulimit/timeout/test-threads. Per-task proofs run inline; the full seal (`cargo xtask gate`) and CI are async witnesses: arm in background, close the story on judge-checked boxes, keep working. The seal is the merge arbiter for the one epic merge. On red, classify before changing anything — implementation, test, or ruling defect; fix implementations, never weaken a test, surface ruling defects.

Completion is total: value change present, condemned path gone, every source satisfied or explicitly re-homed, gates green, stale-reference sweep clean, board and tree describing one reality.

Board MCP server: `mcp/` (gitignored); home = planner-dev repo (copy source there to persist); reconnect MCP to load edits.
