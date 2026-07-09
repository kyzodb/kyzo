# CLAUDE.md — KyzoDB

A pure-Rust engine where relational, graph, vector, text, and time are one substrate — and the 
database ships its own adversary. One Datalog (KyzoScript) over relational, graph, vector, and 
full-text data, with time travel, on one memcomparable transactional KV substrate (`fjall`). 

`README.md` is the product; the board (org project KyzoDB Work) is the plan.

**KyzoDB is built on a single foundational contract: every value the database can hold — numbers,** 
**strings, vectors, geometry, timestamps, nested collections — encodes to bytes whose binary sort** 
**order is identical to the value's semantic order.** This memcomparable encoding is the engine's 
heart. Because one total order governs every type, one ordered key-value substrate (a pure-Rust LSM 
tree, with no C or C++ anywhere in the build) can serve relational queries, graph traversal, vector 
similarity, full-text, and geospatial search through a single Datalog query language. The ordering 
contract is not a convention; it is executable law, enforced by exhaustive cross-type property tests, 
corruption harnesses, and continuous fuzzing, because in an ordered store a sort-order defect doesn't 
raise an error — it silently returns wrong query results. KyzoDB treats that layer with corresponding 
severity.


## The enforcement stack

Rules teach. Hooks interrupt. Tests prove. Scripts enforce.

- `CLAUDE.md` — this constitution.
- `.claude/rules/*.md` — law: `global.md`, path-scoped `zone-*.md` for the target architecture,
  and `deprecated-*.md` carrying per-file migration guidance for the legacy tree.
- `.claude/skills/architecture-map/SKILL.md` — the target-state placement authority.
- `.claude/settings.json` + `.claude/hooks/*.sh` — inject work context and construction doctrine
  each prompt, block container-evasion (`pre-bash-guard.sh`), block unsafe-policy violations on
  edit (`post-edit-guard.sh`), gate engine edits on a focus story (`focus-gate.sh`).
- `.claude/hooks/inject-work-context.sh` — reads the board (project `KyzoDB Work` #1); the focus
  set is every open story In Progress with the `focus` label, injected in full each prompt.
  `.claude/skills/manage-board/manage-board.py` (the manage-board skill) is the ONLY board writer.
- `scripts/authority-graph.py` — the Type Authority Graph: extracts `@authority` doc-comment
  declarations into the committed `authority/` artifacts (`authority-map.json`,
  `authority-report.md`) and audits type-authority drift (raw doors, string taxonomies, duplicate
  counters, blob meaning). `just gate` runs its self-test + ratchet + artifact freshness check.

## The board

The work is the focus set: open stories In Progress carrying the `focus` label, injected every
prompt. Stories and epics are written with the `write-story`/`write-epic` skills and landed with
`manage-board`.

## Build and gate

    docker compose run --rm kyzo-dev  just gate      # the one-command seal
    docker compose run --rm kyzo-bench just bench     # measurements (96 GiB, single-threaded)

`just gate` runs: env-report, `cargo check --workspace --all-targets`, fmt, own-code clippy
`-D warnings`, the unsafe + pure-Rust guards, the authority-graph self-test + ratchet, and the
full suite.


## Global Rules

1. **The tree always builds** — `cargo check --workspace --all-targets` is binary.
2. **`#![forbid(unsafe_code)]` present in every first-party crate root; zero `#[allow(unsafe_code)]` anywhere; no doc claims a nonexistent unsafe exception** — attribute grep + doc-pattern scan, all mechanical.
3. **No C/C++ in any first-party dependency tree; no storage-backend feature flags** — `cargo tree` scan, Cargo.toml parse, and the Dockerfile-without-a-C-compiler makes violation a build failure.
4. **All cargo through the container; no native cargo/just; no hand-set limits (`ulimit -v`, `timeout`, `--test-threads`); binaries run only via `just run`** — command-pattern interception in the Bash hook, deterministic.
5. **The gate covers every first-party crate** — justfile content check.
6. **`manage-board` is the only board writer** — raw `gh` board-write command patterns are blockable in the hook.
7. **No focus story → no engine code changes** — `focus-gate.sh` denies Edits under `kyzo-*` paths unless an open story carries the `focus` label.
8. **No worktrees without operator approval; public/irreversible git acts gated** — `git worktree`, `git tag`, `push` to main are interceptable command patterns behind an operator-set flag.
9. **No sub-agent spawn without operator authorization** — a PreToolUse hook blocks the spawn unless an operator-controlled flag exists.
10. **MPL headers preserved verbatim on edited files** — header-block diff check.
11. **New `#[ignore]` flagged on sight** — grep on the diff (the ledger's adequacy is the other bucket).
12. **The authority ratchets** — raw-door count, `@authority` coverage, string-taxonomy and duplicate-counter audits, allowlist entries only narrowing: deterministic against the committed baseline.
13. **The mutation-test of the harness runs** — executing it is mechanical; it either passes or doesn't.
14. **The rule wins** — over convenience; over old tests (rewrite them to the new law, never weaker); over release compile (the tree sits red before a compatibility shim exists — but every member and feature config must always build). Detecting a weakened rewrite is semantic judgment: yours.
15. **Prime directive** — build the greatest possible engine: effort, size, and tedium never enter a decision; the better design wins even at the cost of rework; the architecture never moves backward; upstream cozo is never a justification.
16. **The review standard** — missed greatness, avoided hard choices, accidental complexity, ceiling-lowering: all judgment.
17. **Types-are-authority at the decision site** — a grep finds string-compares and bare integers as *candidates*, but "does membership control dispatch," "is this identity," and the four-question classification are judgment; only the ratchet halves (rule #12) are mechanical.
18. **Never weaken a test; the three-way failure triage; goldens independently derived; healthy paths construct through production** — weakness, correct triage, and a golden's provenance are invisible to a script; a copied golden is byte-identical to a derived one.
19. **Verify-never-assert; no regression behind narrative; perf claims close on the bench lane** — whether a claim is backed, and whether narrative is hiding a number, is semantic.
20. **Completion is total; a seal means the same truth everywhere** — the gate's greenness is mechanical, but "docs, code, and stories state the same truth" is meaning-comparison.
21. **Move cards the moment reality changes** — a script can't know what reality is.
22. **Commit as units land** — "a unit" is judgment.
23. **The if-ever unsafe protocol's substance** — the named unprovable invariant, the safety case quality: judgment (its trigger is caught in bucket 1).
24. **Enforcement-is-mechanical itself** — the meta-duty to keep converting judgment rules into mechanical checks (hooks, ratchets, gates) wherever a deterministic form exists.

## Origins

KyzoDB began as a fork of [CozoDB](https://github.com/cozodb/cozo) by Ziyang Hu and the Cozo Project
Authors, whose design proved the one-substrate thesis was worth betting on; the full story and
attribution live in [FORK.md](FORK.md).