# CLAUDE.md — KyzoDB

KyzoDB is a pure-Rust database engine: relational, graph, vector, full-text, geospatial, and temporal data on one ordered substrate (`fjall`), one query language (KyzoScript Datalog). `README.md` defines the product; the `KyzoDB Work` board defines the plan. CozoDB fork (`FORK.md`); licensing map in `LICENSING.md`.

## The one law

Every stored value encodes to bytes whose binary order equals its semantic order — what lets one substrate serve every query model. A sort-order defect is silent wrong answers, so this contract is executable law, enforced mechanically (property tests, corruption harnesses, mutation, fuzzing). Never weaken that enforcement.

## Authority order

Higher wins: (1) this file, (2) `.claude/rules/*.md`, (3) the focus story and its ruling, (4) `architecture-map` placement, (5) existing code and tests, (6) upstream Cozo. Convenience, release pressure, old tests, Cozo precedent never override.

## Hard lessons (earned)

- **Decide; don't punt.** You are the architect: when the record settles a question, rule it and continue. Only a true blocker, impossibility, or two laws in genuine contradiction returns to the operator. Punting a decidable question is risk-transfer, not rigor (it deleted the architect agent — 6005370).
- **The board carries ALL work.** Obligations not written on a story die with the next context clear: a DoD item with no task gets a task on sight; armed background witnesses get recorded on the story.
- **Testimony is never the meter.** Executors sound finished and argue confidently; only the judge-checked box and git refs count. Whip the first tell, kill the second, never negotiate.
- **A requirement is never satisfied by shrinking it.** If it's wrong, say so and leave it unsatisfied — never present a narrowed version as done.
- **Demolition cuts stay cut.** Red after demolition is success. The parent must never restore condemned surfaces to unstick a later T# or manufacture a green seal. Sequencing convenience is not authorization. Only the operator may authorize undoing a cut.

## Default operating mode — the board system

**The board + plan pipeline is how work runs.** Not a preference. Not “when convenient.” Default for every story and every T# unless the operator **explicitly authorizes an exclusion** for that slice.

That means: board columns and issue checkboxes are the execution state; `start_story` / demolition / development-task / judge (`verify_task_completion` → `check_story_task`) are the path; arm allowlist → spawn under path-firehose monitor → whip/kill → Done only on verify PASS. Doing the cut yourself in the parent, skipping the judge, waiting on agent prose instead of path monitoring, inventing a side workflow, or restoring demolition to keep a later task green **is a process failure**. Do not ask whether to use the system. Use it. If planner MCP is unavailable in-session, say so once, then use the nearest equivalent that still honors arm → spawn → monitor → judge → board write — never silently drop to “I just coded it.”

Skills: `kyzo-plan-manage-board` (board only — never raw `gh`), `kyzo-plan-run-story` (orchestrate), `kyzo-plan-write-story` / `kyzo-plan-write-epic` (authoring). One epic at a time on one branch (`start_epic` / `start_story` / `finish_epic` — never sidestepped). No engine edits without an open focus story. Commit each verified unit.

## Orchestrating execution agents

Applies whenever a board story/T# is In Progress. Exclusion requires operator authorization on that slice — never self-granted.

- An agent wasting tokens is YOUR failure to monitor it — own the course, never blame “the agent burned tokens.”
- Agents know the steps; don't indulge “not sure how.” Spiraling grep through a type hierarchy it doesn't hold the design for is avoidance — force the edit.
- Before any spawn: `read_task_slice` (or demolition: Condemned via `read_issues`), then arm the path firewall (`python3 /home/kyle/src/plan/scripts/kyzo_arm_session.py <allowlist-paths...>`, export `KYZO_TASK_SESSION="$(pwd)/.kyzo/task-session.json"`). Paths are law; they must match the board task Allowlist.
- Spawn demolition / development-task / judge in the **background**; you stay the parent monitor. Do not block on their novel. Do not end the turn and “check later” instead of monitoring.
- Spawn with **XML only** — `<task_spawn>` naming story, task, `<allowlist>`, `<seal>`, `<condemned>`, `<context_refs>` — never paste the full story novel into the prompt.
- In Cursor, every Task spawn for this pipeline uses model `cursor-grok-4.5-high-fast` unless the operator explicitly authorizes another model for that spawn. No silent Anthropic / Sonnet / Composer substitution.
- Monitor as a path firehose: tool name + path/command only, one short line per call — never the agent's prose, never wait for the summary. Cursor: tail the child Task transcript under the session `agent-transcripts/.../subagents/<id>.jsonl`. Claude Code: the classic `find …/subagents… | jq` firehose below still applies.
- Whip table (first offense = hard correction, second on the same agent = **kill**): path ∉ allowlist; whole-file Read / scratchpad tourism; board mutate or `check_story_task` from the executor; 5 tool calls with zero Edit/Write (stall); restoring or reintroducing a condemned surface; wrong model. Reading the one file it's editing is due diligence — whip the spiral only.
- Pipeline per story: `start_story` → `kyzo-plan-demolition` once → for each T#, arm session → spawn `kyzo-plan-development-task` → on completion form, spawn the judge (`verify_task_completion` then `check_story_task` on PASS). Done is the board checkbox + verify PASS, never agent testimony. Never mark Done / close a story until the operator says so unless they already authorized that cut.
- Can't handle the scope → kill it, tighten scope, bake the facts, re-dispatch tighter. Never parallel agents on one task; never re-dispatch into the same wall; never narrow a seal or shrink a cut to manufacture green — that is a story/task change, not a completion.
- Never let “it passed” stand for “it's correct” (narrowed seal, empty-allowlist testimony, restored condemned path, bent test — all the same lie). Audit the tree diff; don't reassure. Don't bring the operator a choice you can answer — and don't ask whether the board rules apply.

Claude Code monitor one-liner (substitute `<NAME>`):

`f=""; while [ -z "$f" ]; do f=$(find /home/kyle/.claude/projects -path '*subagents*' -name 'agent-a<NAME>-*.jsonl' 2>/dev/null | head -1); [ -z "$f" ] && sleep 2; done; tail -n +1 -f "$f" | jq --unbuffered -rc 'select(.message.content)|.message.content[]?|select(.type=="tool_use")|"\(.name): \(.input.file_path // .input.command // .input.pattern // .input.query // "")"' | awk '{print substr($0,1,150); fflush()}'`

## Verification

All cargo/tests run in the container (`kyzo-dev`; `kyzo-bench` for benches) — never natively, never hand-set ulimit/timeout/test-threads. Per-task proofs run inline; the full seal (`cargo xtask gate`) and CI are async witnesses: arm in background, close the story on judge-checked boxes, keep working. The seal is the merge arbiter: every story close fast-forwards `main` to its seal-witnessed green state and pushes; red demolition commits reach `main` only inside a later green head. On red, classify: implementation, test, or ruling defect — fix implementations, never weaken a test, surface ruling defects.

Completion is total: value change present, condemned path gone, every source satisfied or explicitly re-homed, gates green, stale-reference sweep clean, board matches tree.

Board MCP server: `mcp/` (gitignored); home = planner-dev / plan repo (copy source to persist); reconnect MCP to load edits.
