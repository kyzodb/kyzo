---
name: verify-with-build
description: The verify-with-build/test discipline. Use whenever making any claim about what compiles, what passes, what a change does, or what the dependency graph contains. Every such claim must be backed by a real cargo build/test/run or by reading the actual file.
---

# Verify with build/test

Every claim about the code is backed by evidence produced in this session, or it is not made.

## The rule

- **A claim about compilation** requires a real `cargo build` (exact package/flags named) run now, not
  remembered from earlier.
- **A claim about behaviour or correctness** requires a real `cargo test` or an actual run whose output
  is quoted.
- **A claim about a file's contents** requires reading that file in this session.
- **A claim about the dependency graph** requires `cargo tree` / the lockfile, whole-workspace. A
  pure-Rust claim checked only against `kyzo-core` while silently excluding the bindings is the
  canonical sabotage on this project: scope every claim explicitly or label it partial.
- If a claim cannot be verified (toolchain missing, code not yet compiling), **say so plainly** and
  state what was checked instead. An unverifiable claim stated as fact is worse than no claim.

## Standard verification commands

Official gates run in the pinned container (real cgroup RSS cap, not `ulimit -v`):

    docker compose run --rm kyzo-dev  just gate        # the seal (check, fmt, clippy, unsafe, pure-rust, tests)
    docker compose run --rm kyzo-dev  just env-report  # environment fingerprint for the report
    docker compose run --rm kyzo-bench just bench       # benchmarks (96 GiB, single-threaded)

Individual recipes (`just check` / `test` / `test-features` / `clippy` / `memcheck`) run the same
commands natively for speed. Do NOT wrap cargo in `ulimit -v` — it caps virtual address space, which
Rust over-reserves, manufacturing fake OOMs; the container's `mem_limit` is the honest cap (see
`.claude/rules/environment.md`). Every gate report states native-vs-containerized. Mutation runs:
`(timeout 600 cargo mutants ...)`.


## Exit codes, not pipe output
A command piped into grep/tail reports the LAST pipe stage's exit code: `cargo clippy | tail` looks
green while clippy failed. Check the command's own exit code explicitly (e.g. `${pipestatus[1]}`) for
every gating claim. A green that is not exit-code-verified is not a green — this exact failure has
already produced a false commit on this project.

## A passing suite proves nothing about itself
- **Mutation-proof the tests**: a test suite's guarantee is demonstrated by
  breaking the code it claims to protect and watching it fail — then
  reverting. A green suite that also stays green under the bug is the bug.
  (Proven here twice: an under-reporting meet op and a set-intersection
  computing union both sailed through a "complete" law suite.)
- **Laws pin lawfulness; only value oracles pin the operation.** Property
  tests (idempotent, associative, round-trips) admit whole families of
  wrong implementations that are also lawful. Every operation needs at
  least one concrete input→output assertion.

## Reporting

- Quote the actual command and its result (or the failing tail of it). Never summarize a failure as a
  pass or a partial as a whole.
- If a build/test was skipped, the report says it was skipped and why.
- Interpreting a noisy or failing run is the `cargo-diagnostics-triager` agent's job; dispatch it
  rather than guessing.
