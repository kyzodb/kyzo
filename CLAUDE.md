# CLAUDE.md — KyzoDB

Pure-Rust fork of [CozoDB](https://github.com/cozodb/cozo): one Datalog (KyzoScript) over relational,
graph, vector, and full-text data, with time travel, on one memcomparable transactional KV substrate
(`fjall`). `README.md` is the product; the board (org project KyzoDB Migration) is the plan.

## The constitution

Read and obey `.claude/rules/`. Rules are law.

- A rule conflicts with convenience → the rule wins.
- A rule conflicts with old tests → migrate the tests without weakening them.
- A rule conflicts with release compile → leave the tree red rather than add a compatibility shim.
  (But the shared tree, every feature config and workspace member, must always *build*.)

**Prime directive.** Build the greatest possible engine. Effort, size, and tedium never factor into a
decision. Upstream cozo is a dead reference, never a justification. No deployed stores exist: no
compatibility, no legacy decode paths, no migration gentleness. Between competing designs the better
engine wins, even at the cost of rework — the architecture only moves toward the goal, never back.

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
- `.claude/settings.json` + `.claude/hooks/*.sh` — inject the active story, block gate-evasion, check
  touched files, gate completion.
- `.claude/active-story.md` — the one story in flight, injected each turn.

## Operating essentials

- **One tree, one branch.** Real tree, current branch. No worktrees, no parallel patch stacks. Commit
  and push freely as units land; the go-gate is only public/irreversible acts (merge to main, tags,
  releases, new remotes).
- **Verify, never assert.** Every claim is backed by a real `cargo` run or by reading the file.
- **Memory caps on every cargo run:** `(ulimit -v 12582912 && timeout 1800 cargo ...)`; mutants
  `(ulimit -v 8388608 && timeout 600 ...)`.
- **Pure Rust, `#![forbid(unsafe_code)]`, zero exceptions** in first-party code (`rules/unsafe.md`).
  FFI lives only in the bindings; the core depends on nothing of ours.
- **MPL-2.0.** Preserve every CozoDB copyright header and attribution verbatim; add ours alongside.

## Build and gate

    (ulimit -v 12582912 && timeout 1800 cargo build -p kyzo --release)
    (ulimit -v 12582912 && timeout 1800 cargo test  -p kyzo --release)

A seal requires the full suite green, `cargo clippy --release --all-targets -- -D warnings` clean on
own code in both feature configs, and `cargo fmt --check` clean. Gates: `rules/00-story-gates.md`.
