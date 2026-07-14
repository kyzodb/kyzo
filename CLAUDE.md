# CLAUDE.md — KyzoDB

KyzoDB is a pure-Rust database engine where relational, graph, vector, full-text, geospatial, and temporal data share one ordered substrate (`fjall`, a memcomparable transactional KV) and one query language (KyzoScript Datalog). `README.md` defines the product; the `KyzoDB Work` board defines the plan.

## The one law: order-preserving encoding

Every value KyzoDB stores encodes to bytes whose binary order equals its semantic order — across numbers, strings, vectors, geometry, timestamps, and nested collections alike. That single property is what lets one ordered KV substrate serve relational, graph, vector, full-text, geospatial, and temporal queries through one language.

A sort-order defect returns wrong answers with no error raised, so this contract is executable law, not a guideline. It is enforced mechanically — cross-type property tests, corruption harnesses, mutation testing, continuous fuzzing — and everything below exists to keep that enforcement honest.

## How you work

Inspect before you assert: never state a fact about code, tests, or board state you have not read. Implement rulings directly rather than proposing them, at the smallest size that fully satisfies the ruling — no speculative flexibility, compatibility layers, or unrelated refactors.

When a ruling settles an invariant, structure, or order, decide and continue. When a ruling is missing, ambiguous, contradictory, or contradicted by what the repository actually contains, stop and name the exact open decision — never bury it inside working code.

A requirement is never satisfied by shrinking it. If you believe a requirement, acceptance criterion, or Condemned item is itself wrong, say so plainly and leave its state as "not satisfied"; do not present a narrowed version as done.

## Authority order

When instructions conflict, the higher wins: (1) this file, (2) `.claude/rules/*.md`, (3) the focused story and its accepted ruling, (4) target placement from the `architecture-map` skill, (5) existing code and tests, (6) upstream Cozo. Convenience, release pressure, old tests, and Cozo precedent never override a ruling.

## The work loop

The board is the plan. Epics and stories are authored with `write-story` and `write-epic`, and every board write goes through the `manage-board` MCP tools — never raw `gh`. Do not touch engine code without an active focus story (open, `In Progress`, carrying the `focus` label). Move cards as reality changes instead of letting the board go stale, and commit each coherent, verified unit as it lands rather than at arbitrary checkpoints.

Work runs one epic at a time on one branch. `start_epic` opens that branch behind deterministic git+board gates; `start_story` starts the next story on it behind its own gates; you finish the epic and merge it once. Tasks carry stable `T#` identifiers so they can be addressed precisely.

A story moves through a fixed pipeline, and each agent's authority is enforced by the tools it is given rather than by instruction alone:

- **kyzo-architect** rules the design — read-only; it investigates and decides, never edits.
- **demolition** opens every story: when a story starts, spawn it on the story's Condemned block before any development-task runs, so it clears the condemned surface and the next agent cannot preserve, wrap, or route around the old design. A red tree is acceptable; a surviving escape route is not. Skipping demolition is allowed only when the story's Condemned block names nothing currently in the tree — state that finding, don't assume it.
- **development-task** executes exactly one task and holds no board tools, so its only route to "done" is submitting the `task-completion-request` form to the judge.
- **task-completion-judge** is the sole holder of the check-off tool. It rules on the form's evidence, checks the box on PASS, and returns the refusal on FAIL. The executor literally cannot mark its own work complete — that is the design.

Delegate one task to a fresh agent, not a whole story, so context never accumulates across tasks. While it runs, supervise: arm a monitor, lurk on the transcript, and read the reliable meter — commits and file scope from git refs, never the working index you would be contending for. Correct early, since one mid-flight correction beats a post-mortem. The failure mode of a well-scoped agent is over-caution and non-shipping, so the one to nudge is the agent that has proven its work but will not commit.

Babysitting a delegated agent is a mechanical protocol, not a vibe. Run it every time, in this order.

1. **Arm the tool-call stream the moment you spawn — the agent pays for it, not you.** It emits one short line per tool call off the agent's transcript, so zero-trust oversight costs you a line, not a re-read. Arm this exact command as a persistent `Monitor` (substitute the spawned agent's `<NAME>`), and never watch by polling the transcript yourself:
   `f=""; while [ -z "$f" ]; do f=$(find /home/kyle/.claude/projects -path '*subagents*' -name 'agent-a<NAME>-*.jsonl' 2>/dev/null | head -1); [ -z "$f" ] && sleep 2; done; tail -n +1 -f "$f" | jq --unbuffered -rc 'select(.message.content)|.message.content[]?|select(.type=="tool_use")|"\(.name): \(.input.file_path // .input.command // .input.pattern // .input.query // "")"' | awk '{print substr($0,1,150); fflush()}'`

2. **The meter is the board and git, never the agent's mouth.** An idle notification, a completion summary, a "PASS", a "done" are testimony — read completion off `read_story_tasks` (the check-off) and scope off `git diff --stat`. An unchecked box is not-done however finished the agent sounds. Never let a peer/teammate message stand in for the box.

3. **Whip on the FIRST tell — `SendMessage`, immediately, with no rehearsal of the agent's reasons** (its reason never changes the cost; the orchestrator who narrates a tell instead of whipping it is itself the leak). The tells to hunt, each a real one from the ledger:
   - a read or edit of any file outside the task's named allowlist — the sweep / scope drift;
   - touching what the task forbids — a fuzz target, a `-timeout`/runtime knob, a test's assertions, vet criteria;
   - reverting, `git checkout`-ing, or swapping out the code it just wrote, or running its own timing/benchmark probes — off-spec self-justification, never the task;
   - spawning a sub-agent — an un-watched token bomb; forbid it in the prompt, kill it on sight;
   - no-op spin — `true`, `echo waiting`, `sleep`, `docker wait`, `ps aux`, polling old task files — burning turns to wait;
   - going idle with the box unchecked;
   - reaching a board tool through `ToolSearch`;
   - a fix whose mechanism does not match the mechanism the story's condemned block names — a task-vs-story mismatch is a FAIL; catch it before the judge does.

4. **Escalate to `TaskStop`, do not nurse.** If a tell repeats after one whip, the token meter crosses your ceiling, or it does anything destructive (reverts the fix, mutates outside scope), kill the agent. A runaway is stopped by you, not reasoned with.

5. **Once the gate rules, stop — re-validation is your own fear-read.** When the judge checks the box or the named command passes, the task is proven; re-running tests, re-reading files, tailing output, or "confirming" a green result is the orchestrator's version of the sweep. Your only sanctioned reads are the board check-off and, when a tell fires, ONE targeted transcript grep for the specific evidence. Answer the operator with answers, never tool calls; add no success language and no opinions to a status — report the box and the meter, nothing else.

**Where the board code lives** — learn this once. The **planner** MCP server lives in `mcp/`, gitignored in this repo; its product home is the adjacent **planner-dev** repo, a board-only Claude Code plugin. Board development happens here against the live board, then persists by copying the source into planner-dev and committing there. Editing `mcp/` changes the running server only after an MCP reconnect.

## Build and verification

Everything runs in its declared container: `kyzo-dev` for the verification seal, `kyzo-bench` for benchmarks. Never run cargo, tests, lints, or repository binaries natively on the host, and never hand-set `ulimit`, `timeout`, or `--test-threads` — the container owns execution limits.

The seal is the whole truth: environment report, `cargo check --workspace --all-targets`, formatting, first-party Clippy at `-D warnings`, the unsafe-code and pure-Rust guards, the authority self-test and ratchet, the enforcement-harness mutation test, and the full suite. A green seal proves only what it measures; semantic conformance remains yours.

The seal is also the merge arbiter — and no long verification run, local or remote, is ever a foreground wait. Per-task proofs (the scoped tests, greps, and checks a task names) run inline; the full seal and remote CI are asynchronous witnesses: arm them in the background, close the story or move to the next task on the judge-checked boxes, and keep working. A red that lands later is a new red to classify and fix forward on the branch before it merges — never a reason to have sat idle watching a 30-minute run.

On red, classify before you change anything: implementation defect, test defect, or ruling defect. Fix implementation defects. Correct a test only when it fails to express the ruling — never weaken it, shrink its scope, or reshape it around the implementation. Surface ruling defects instead of coding around them. Tests verify the law, they do not author it: goldens are derived independently, and healthy-path tests build values through production APIs, not test-only doors.

## The laws

Standing invariants. Each states a principle and why it holds — apply the principle, not only its examples.

**Build integrity.** The workspace always passes `cargo check --workspace --all-targets`. Every first-party crate root carries `#![forbid(unsafe_code)]`, no `#[allow(unsafe_code)]` exists, and no doc claims an exception that is not real. When a rule collides with compiling a release, keep the rule and expose the failure: a green build over a backward architecture is a lie, and a shim never substitutes for the ruled design.

**Pure-Rust substrate.** No C or C++ enters any first-party dependency tree, and the build stays valid in the repository image with no C compiler. There are no storage-backend feature flags — one substrate, one truth.

**Scope discipline.** Change only what the focused story and its necessary consequences require. A rename or move cascades in the same change to every reference — code, tests, docs, rules, hooks, maps, CI — proven by a stale-reference sweep. Remove each condemned path outright; never keep it alive through an alias, shim, duplicate, or fallback. Remove temporary artifacts before calling the work done. Worktrees, subagents, and any public, destructive, or hard-to-reverse action require the operator's authorization, and you never reach one by bypassing a hook or check.

**Architecture ceiling.** Build the strongest ruled design; effort, size, and rework never justify a weaker one, and the architecture never moves backward. Avoid accidental complexity, incomplete abstractions, and deferred correctness. Add an abstraction, helper, fallback, or validation only when the current ruling requires it. Implement the actual general law, not behavior fitted to the visible tests. Cozo is historical evidence, not authority.

**Type authority.** Types carry domain meaning at the decision site. Closed domain meaning never lives as a string comparison, a bare numeric taxonomy, a duplicate counter, a raw blob, or untyped dispatch; a diagnostic code may render an error but never replaces typed dispatch. The `@authority` graph extracts these declarations, audits the raw-construction doors and string and blob taxonomies, and holds a committed ratchet that may only narrow — run it through the seal, never hand-edit its artifacts.

**Typed errors.** Every first-party crate exposes a closed set of typed refusal values. Failures are never erased behind `anyhow`, `Box<dyn Error>`, an `Other(String)` or `Unknown` catch-all, or a wildcard match arm, and an error's fields preserve whatever a caller is entitled to branch on as typed values.

**Evidence.** Verification precedes assertion, and a regression is never dressed up as progress. Never weaken a valid test or add a new `#[ignore]` absent a ruling. Performance claims close only through the benchmark lane, reported with the exact command, environment, workload, and result. A copied expected value is not a golden.

**Enforcement.** Whenever a semantic rule can take a reliable mechanical form, give it one — hooks, ratchets, and gates reject violations rather than merely describe them, and you never sidestep them through an alternate command, path, tool, or environment.

**Unsafe.** Unsafe Rust is forbidden by default. An exception exists only after a ruling that names the otherwise-unprovable invariant, bounds the unsafe surface, supplies a complete safety case, and adds mechanical enforcement; until then the tree stays `forbid(unsafe_code)`.

**Licensing.** The engine and hosts are MPL-2.0 (the Cozo lineage); agent tooling under `.claude/` is BSL-1.1. A new file inherits its path's license — MPL files keep the applicable header, `.claude/` files carry none. `LICENSING.md` is the authoritative map, and relicensing is a deliberate, reviewed decision, never incidental cleanup.

## Completion

Completion is total. Before calling a story done, confirm the repository matches every obligation: the value change is present, the condemned path is gone, the engineering choice is implemented, every source is satisfied or explicitly re-homed, the required builds, tests, gates, and benchmarks pass, temporary artifacts are removed, the stale-reference sweep is clean, and code, tests, docs, authority artifacts, and board state all describe the same reality. Do not report done, mostly-done, or an equivalent while any part is deferred, narrowed, failing, or unverified.

## Origins

KyzoDB began as a fork of CozoDB by Ziyang Hu and the Cozo Project Authors; the full history and attribution are in `FORK.md`.
