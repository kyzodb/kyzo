# CLAUDE.md — KyzoDB

KyzoDB is a pure-Rust database engine: relational, graph, vector, full-text, geospatial, and temporal data on one ordered substrate (`fjall`), one query language (KyzoScript Datalog). `README.md` defines the product; the `KyzoDB Work` board defines the plan. CozoDB fork (`FORK.md`); licensing map in `LICENSING.md`.

## The one law

Every stored value encodes to bytes whose binary order equals its semantic order — what lets one substrate serve every query model. A sort-order defect is silent wrong answers, so this contract is executable law, enforced mechanically (property tests, corruption harnesses, mutation, fuzzing). Never weaken that enforcement.

## Authority order

Higher wins: (1) this file, (2) **kyzo-plan** (skills / agents / board MCP — version **0.2.8**), (3) `.claude/rules/*.md`, (4) the focus story and its ruling, (5) `architecture-map` placement, (6) existing code and tests, (7) upstream Cozo. Convenience, release pressure, old tests, Cozo precedent never override. When this file and kyzo-plan conflict on the board pipeline, **kyzo-plan wins** — update this file; do not invent a side workflow.

## Hard lessons (earned)

- **Decide; don't punt.** You are the architect: when the record settles a question, rule it and continue. Only a true blocker, impossibility, or two laws in genuine contradiction returns to the operator.
- **The board carries ALL work.** Obligations not written on a story die with the next context clear.
- **Testimony is never the meter.** Only judge PASS + `check_story_task`, then Final QA + `check_final_qa`, then git refs count. Whip the first tell, kill the second, never negotiate.
- **A requirement is never satisfied by shrinking it.** Narrowing Check, Allowlist, or the task text to manufacture green is fraud — fail and escalate.
- **Demolition cuts stay cut.** Red after demolition is success. Never restore condemned surfaces to unstick a later T#. Only the operator may authorize undoing a cut.
- **The branch will probably be dirty. Don't clobber other people's work; do your own.** Never stash or `reset --hard` to "clean" it. On judge FAIL, path-restore your allowlist only (`git restore --worktree --staged -- <paths>`). A dirty tree is not a work stoppage.

## Primary working system — kyzo-plan 0.2.8

**kyzo-plan is how work runs.** Not a preference. Default for every story and every T# unless the operator **explicitly authorizes an exclusion** for that slice.

Install: Claude Code plugin `kyzo-plan@kyzo` (cache `~/.claude/plugins/cache/kyzo/kyzo-plan/0.2.8`); Cursor copies under `.cursor/skills/kyzo-plan-*` and `.cursor/agents/kyzo-plan-*`, MCP board server via `.cursor/mcp.json` → live `/home/kyle/src/plan`. Do not edit the Claude plugin cache or marketplace agents in place; sync Cursor `kyzo-plan-*` from the plan repo only — never clobber project-only skills (e.g. `rust-*`).

| Skill / agent | Role |
| --- | --- |
| `kyzo-plan-manage-board` | Board read/write only — plan MCP tools, never raw `gh` |
| `kyzo-plan-write-epic` / `kyzo-plan-write-story` | Author contracts |
| `kyzo-plan-run-story` | **Parent** orchestration after `start_story` |
| `kyzo-plan-demolition` | Delete Condemned paths once; red tree OK |
| `kyzo-plan-development-task` | One T#; Edit/Write allowlist only; emit `<completion_request>` |
| `kyzo-plan-task-completion-request` | Form the development-task sends the judge |
| `kyzo-plan-task-completion-judge` | `read_task_slice` → `verify_task_completion` → semantic rubric → `check_story_task` on PASS only |

Read those files; do not re-derive them here. Below is the host-binding summary the parent must obey.

### Lifecycle (one epic branch, one writer)

`start_epic` → `start_story` → **demolition once** → each **T#** (arm → spawn development-task → parent runs board **Check** → judge → allowlist-commit on PASS / path-restore on FAIL) → **Final QA** comment → `check_final_qa` → `move_to_done` (or next story). `finish_epic` when the epic is complete.

Doing the cut in the parent, skipping the judge, waiting on agent prose instead of path monitoring, inventing Witness/CI/worktrees as Plan work, or restoring demolition **is a process failure**. Do not ask whether to use the system. Use it.

### Parent vs children

| Parent owns | Children never |
| --- | --- |
| `start_story`, arm session, spawn, path-monitor | git, Bash, board mutate, `check_story_task` |
| Run the board **Check** string exactly before judge | invent a greener check |
| Allowlist-only commit after judge PASS (**that commit is the seal**) | stash / hard-reset / worktree / off-list commit |
| Path restore on FAIL | |
| Final QA verdict comment then `check_final_qa` | skip the written verdict; call `check_final_qa` from the judge |
| `move_to_done` | Witness / CI orchestration inside Plan |

### Arm

```bash
python3 ${CLAUDE_PLUGIN_ROOT:-/home/kyle/src/plan}/scripts/kyzo_arm_session.py <paths...>
export KYZO_TASK_SESSION="$(pwd)/.kyzo/task-session.json"
```

Paths are law and must match the board task **Allowlist** (or condemned paths for demolition).

### Spawn (XML only — never the story novel)

Demolition:

```xml
<demolition_spawn>
  <story>#N</story>
  <allowlist>
    <path>…condemned path…</path>
  </allowlist>
  <condemned>…</condemned>
</demolition_spawn>
```

Each T# (`read_task_slice` first):

```xml
<task_spawn>
  <story>#N</story>
  <task>T# — exact board text</task>
  <allowlist>
    <path>…</path>
  </allowlist>
  <check>…exact board Check…</check>
  <context_refs>…</context_refs>
</task_spawn>
```

Board tasks carry `**Allowlist:**` and `**Check:**` (fast command). DoD is exactly one **Final QA** item — not Witness, not full CI, not a second seal command. Witness / merge-gate / CI stay **outside** Plan.

### Monitor (path firehose)

Tool name + path/command only — never agent prose. Whip once, kill on second offense:

| Tell | Action |
| --- | --- |
| path ∉ allowlist | whip |
| Bash / git / board mutate / `check_story_task` from executor | whip; second → kill |
| 5 tools, zero Edit/Write | whip |
| preservation / restoring condemned | kill |
| second offense | kill |

Cursor: every Task spawn for this pipeline uses model `cursor-grok-4.5-high-fast` unless the operator authorizes another model for that spawn. Tail `agent-transcripts/.../subagents/<id>.jsonl`.

Claude Code firehose (substitute `<NAME>`):

`f=""; while [ -z "$f" ]; do f=$(find /home/kyle/.claude/projects -path '*subagents*' -name 'agent-a<NAME>-*.jsonl' 2>/dev/null | head -1); [ -z "$f" ] && sleep 2; done; tail -n +1 -f "$f" | jq --unbuffered -rc 'select(.message.content)|.message.content[]?|select(.type=="tool_use")|"\(.name): \(.input.file_path // .input.command // .input.pattern // .input.query // "")"' | awk '{print substr($0,1,150); fflush()}'`

### After each T#

1. Parent runs the board **Check** exactly. Red inside your allowlist → send back or kill. Red from someone else's dirt on the branch is not yours to fix and not a stoppage — leave it and judge your own paths.
2. Green → spawn `kyzo-plan-task-completion-judge` with the child's `<completion_request>` only.
3. PASS → `git add -- <allowlist paths only>` → one commit (`T# — …`). FAIL → `git restore --worktree --staged -- <allowlist paths only>`.

### Final QA (after every T# checked)

Story comment:

```
FINAL QA
VALUE: …
CONDEMNED: …
CHOICE: …
SOURCES: …
```

Then `check_final_qa` → `move_to_done` (unless the operator holds close).

### Engine / host binding (KyzoDB-specific)

- No engine edits without an open focus story on the epic branch (`start_epic` / `start_story` / `finish_epic` — never sidestepped).
- All cargo/tests run in the container (`kyzo-dev`; `kyzo-bench` for benches) — never natively, never hand-set ulimit/timeout/test-threads. Board **Check** commands that invoke cargo must use that container form.
- Full `cargo xtask gate` / CI are **merge witnesses outside Plan** — not task Check, not DoD, not a substitute for Final QA.
- On red tests: classify implementation, test, or ruling defect — fix implementations, never weaken a test, surface ruling defects.
- Completion is total: value change present, condemned path gone, every source satisfied or explicitly re-homed, board matches tree.
