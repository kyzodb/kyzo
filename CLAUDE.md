# CLAUDE.md — KyzoDB

Pure-Rust fork of [CozoDB](https://github.com/cozodb/cozo): one Datalog (KyzoScript) over relational,
graph, vector, and full-text data, with time travel, on one memcomparable transactional KV substrate
(`fjall`). `README.md` is the product; the board (org project KyzoDB Work) is the plan.

## The constitution

Read and obey `.claude/rules/`. Rules are law.

- A rule conflicts with convenience → the rule wins.
- A rule conflicts with old tests → rewrite the tests to the new law without weakening them.
- A rule conflicts with release compile → leave the tree red rather than add a compatibility shim.
  (But the shared tree, every feature config and workspace member, must always *build*.)

**Prime directive.** Build the greatest possible engine. Effort, size, and tedium never factor into a
decision. Upstream cozo is a dead reference, never a justification. No deployed stores exist: no
compatibility, no legacy decode paths, no migration gentleness. Between competing designs the better
engine wins, even at the cost of rework — the architecture only moves toward the goal, never back. 

Review this code like the goal is to build an exceptional database engine, not merely pass tests. 
Check for bullshit, but also look for missed greatness: places where the implementation avoided the 
hard architectural choice, failed to use types as authority, accepted accidental complexity, copied 
ordinary database patterns without asking whether KyzoDB’s ordered substrate / determinism / time / 
provenance model allows something better, or settled for “works” when the right move is to research 
the best known designs and make a sharper engineering bet. Prefer code that is small, principled, 
type-driven, deterministic, measurable, and honest about risk; reject code that is safe-looking but 
lowers the ceiling of the engine. A risky design is acceptable when it is explicit, testable, 
reversible, and gives us real signal about the edge of KyzoDB’s potential.

**Hard prohibitions.** No weakened tests. No goldens copied from output. No compatibility shims for
deleted authority surfaces. No story called complete with a gate skipped. No benchmark regression
hidden behind narrative. No unchecked constructors, raw-code doors, forged wrappers, or second value
serialization authorities. No "next story" as a deferral escape hatch.

**Completion** means code, tests, docs, scripts, benchmarks, and gates all state the same truth.

## The enforcement stack

**Rules teach. Hooks interrupt. Tests prove. Scripts enforce.** Don't try to remember everything —
make the repo catch it.

- `CLAUDE.md` — this constitution.
- `.claude/rules/*.md` — law. Global (`00`/`01`/`02`) load every session; path-scoped rules load when
  you touch matching files.
- `.claude/settings.json` + `.claude/hooks/*.sh` — warn on on-disk-format blast-radius zones, check
  touched files against their zone's law, block container-evasion (`pre-bash-guard.sh`).
- `scripts/board-context` — read-only board context: generates `.claude/active-story.md`,
  `.claude/next-work.md`, and `.claude/board-signal.md` from GitHub (injected each prompt). It nudges
  when the board and reality disagree; a human or dev agent updates the board intentionally — the
  tooling never moves cards. Completed work writes evidence back with `scripts/story-evidence`.
- `scripts/authority-graph` — the Type Authority Graph (#139): extracts `@authority` doc-comment
  declarations into the committed `authority/` artifacts (`authority-map.json`,
  `authority-report.md`) and audits type-authority drift (raw doors, string taxonomies, duplicate
  counters, blob meaning). `just gate` runs its self-test + ratchet + artifact freshness check;
  strict mode is the end state as the baseline burns to zero.

## Operating essentials

- **The board is the workflow.** The work is the active story (`.claude/active-story.md`, injected
  every prompt); pick it up with the `story-execution` skill — plan of attack before the first
  edit, types before mechanism. No active story → no code changes without the operator.
- **One tree, one branch.** Real tree, current branch. No worktrees, no parallel patch stacks. Commit
  and push freely as units land; the go-gate is only public/irreversible acts (merge to main, tags,
  releases, new remotes).
- **Sub-agent delegation is operator-authorized only.** You have NO authority to spawn, dispatch, or
  fan out to a sub-agent (reviewer, triager, worker, or any Agent/Task) on your own decision. When a
  sub-agent would help, describe what you propose to delegate and ASK the operator; spawn only after
  they say yes.
- **Verify, never assert.** Every claim is backed by a real run (in the container, below) or by
  reading the file.
- **ALL cargo runs go through the container. No exceptions, ever.** Every build/test/clippy/bench is
  `docker compose run --rm kyzo-dev just <recipe>` (or `kyzo-bench` for measurements). Never run
  `cargo`/`just` natively, and never hand-set a memory limit (`ulimit -v`, `timeout`, `--test-threads`)
  — the container's cgroup RSS ceiling and pinned threads ARE the limits. There is no "native for
  speed" path; a raw native `cargo test` is a defect, blocked by `pre-bash-guard.sh` (`rules/environment.md`).
- **Pure Rust, `#![forbid(unsafe_code)]`, zero exceptions** in first-party code (`rules/unsafe.md`).
  FFI lives only in the bindings; the core depends on nothing of ours.
- **MPL-2.0.** Preserve every CozoDB copyright header and attribution verbatim; add ours alongside.

## Build and gate

    docker compose run --rm kyzo-dev  just gate      # the one-command seal
    docker compose run --rm kyzo-bench just bench     # measurements (96 GiB, single-threaded)

`just gate` runs: env-report, `cargo check --workspace --all-targets`, fmt, own-code clippy
`-D warnings` (both feature configs), the unsafe + pure-Rust guards, the authority-graph
self-test + ratchet, and the full suite (default + features). A seal is all of it green. Gates: `rules/00-story-gates.md`; environment:
`rules/environment.md`.
