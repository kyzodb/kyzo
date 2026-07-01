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

    cargo build -p kyzo --release        # core
    cargo test  -p kyzo --release        # core tests
    cargo build --workspace              # whole-workspace (the honest scope)
    cargo tree  -p kyzo -e normal,build  # dependency-graph claims (incl. build deps)

## Reporting

- Quote the actual command and its result (or the failing tail of it). Never summarize a failure as a
  pass or a partial as a whole.
- If a build/test was skipped, the report says it was skipped and why.
- Interpreting a noisy or failing run is the `cargo-diagnostics-triager` agent's job; dispatch it
  rather than guessing.
