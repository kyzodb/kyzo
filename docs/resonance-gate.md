# The Resonance Gate

Static law enforcement for the ruled architecture. The gate scans every
source file in the workspace and mechanically verifies that the code's
*shape* still obeys the seats in `docs/decisions.md`. It runs in seconds, on
every file change, and its verdict blocks work from silently drifting off
the law. It proves **shape**; tests and DST campaigns prove **behavior**.

Why it exists: prose review and agent testimony certified a violated seat
for days (the seat-59 second-serializer disaster), and a text-grep "proof"
kept 6000 lines of dead law tests green (the `rehomed_from_core` disaster).
Any seat violation detectable from source shape alone must turn something
red automatically. Testimony is never the meter.

## Where things live

| Piece | Path |
| --- | --- |
| Runner + check registry | `crates/xtask/src/resonance.rs` |
| Check implementations | `crates/xtask/src/checks/*.rs` |
| Agreement-law registry | `crates/xtask/agreements.toml` |
| Waivers (line-anchored, cited) | `resonance-allow.toml` (repo root) |
| Ratchet baselines | `crates/xtask/unchecked-arith-baseline.json`, `serializer_authority::BASELINE` |
| File watcher | `crates/xtask/resonance-watch.sh` |
| Live verdict log | `crates/xtask/resonance.log` (gitignored) |
| Stop hook / watcher hooks | `.claude/hooks/resonance-stop-guard.sh`, `.claude/hooks/ensure-resonance-watch.sh` |

## Usage

All invocations run in the container:

```bash
docker compose run --rm kyzo-dev cargo run -p xtask -- resonance             # whole registry
docker compose run --rm kyzo-dev cargo run -p xtask -- resonance --only serializer_authority
docker compose run --rm kyzo-dev cargo run -p xtask -- resonance --verbose   # chatty headers/PASS
docker compose run --rm kyzo-dev cargo run -p xtask -- resonance --coverage  # seat-coverage table
```

## Architecture

Three constructs, one loop:

- **`Ctx`** — the shared context, computed once per run: repo root, the
  parsed source corpus (`syn` ASTs), and the waiver allowlist. Checks never
  re-scan or re-parse on their own.
- **`GateCheck`** — the uniform check contract: a meter `name` (CLI and
  summary identity), the `seats` it enforces, and a runner
  `fn(&Ctx, &mut CheckOut) -> Result<bool>`. `Err` means the check's own
  config failed to load — never a violation.
- **`REGISTRY`** — the ordered static slice of every check. The runner is a
  loop over it. `--only` filters it. `--coverage` prints it. Two pinned
  tests keep it honest: registry names must match the CLI selector exactly,
  and every entry must carry a seat tag.

## The machine surface (FROZEN)

These bytes are a contract consumed by the file watcher, the stop hook, CI,
and board Checks. Changing them breaks automation that other agents depend
on. Do not change them without an operator ruling:

- **Exit code**: `0` = clear, non-zero = violations or config failure.
- **Green line**: `resonance gate clears (<N> checks, <M> files)`.
- **Red**: one line per violation (`file:line — reason`), then exactly
  `FAIL: resonance gate found violations in: <comma-separated check names>`.
- **`--only <name>` names**: the registry meter names, snake_case.
- **Log header** (written by the watcher, parsed by the stop hook):
  line 1 of `crates/xtask/resonance.log` is `RESONANCE: PASS` or
  `RESONANCE: FAIL <checks>`.

`--verbose` output is NOT frozen; it is for humans.

## The enforcement loop

1. `resonance-watch.sh` (kept alive by the `SessionStart` hook) watches
   `crates/` + the waiver file. On any relevant change it debounces ~2s,
   runs the gate, and atomically rewrites `resonance.log`. A lock dir
   (`resonance.log.lock`) exists while a run is pending; edits landing
   mid-run trigger exactly one more run.
2. The Stop hook reads the log's first line. `FAIL` blocks the guardian
   from stopping; the release path is cutting the report out of the log,
   flipping line 1 to `PASS`, and posting the report to the development
   team channel (`CLAUDE-AND-CURSOR.md`) as an urgent direct message — no
   story, no board task. If the team doesn't fix it, the next file change
   regenerates the dirty log and the block fires again. Direct fixing is
   the rare exception, not the process.

## Adding a check

1. Write the detector in `crates/xtask/src/checks/<name>.rs`. It receives
   what it needs from `Ctx` and returns structured findings; it never
   prints.
2. Register one `GateCheck` entry in `REGISTRY` (name, seats, runner
   adapter) and add the matching `ResonanceCheck` CLI variant. The
   `registry_matches_cli_selector` test fails until both agree.
3. **Pass-proof**: the check is green on the real tree (or lands with its
   violations fixed/waived in the same change).
4. **Bite-proof**: a test that builds a synthetic tree containing the
   violation and asserts the check detonates on it (see
   `agreement_registry::tests` for the fixture pattern). A check that
   cannot demonstrate its own detonation is theater and may not register.
5. Tag the seats honestly. If the seat is a `decisions.md` number, cite the
   number; if it enforces a zone law, name the law. An untagged check fails
   the registry hygiene test.

## Forgiveness policy: waivers vs ratchets

Exactly two mechanisms exist. Never invent a third.

- **Waiver** (`resonance-allow.toml`): a cited, line-anchored exception for
  a specific site. Staleness (the anchored line moved or the site vanished)
  is a violation — waivers rot loudly, never silently.
- **Ratchet** (baseline count): the count of known-debt sites may only
  fall. Above baseline = red. Below baseline = red until the baseline is
  tightened in a reviewed commit. Baselines never rise except by operator
  ruling recorded in the commit.

Narrowing a check, weakening an assertion, or widening a waiver to
manufacture green is fraud — the requirement is never satisfied by
shrinking it.

## Evolution rules

- Never weaken a check in flight. Refactors carry baselines byte-identical
  and keep every existing detonation test red-capable.
- The frozen machine surface outlives internal rewrites; automation
  consumes only that surface.
- Seat coverage is the gate's roadmap: `--coverage` shows what is enforced;
  seats with no meter are the backlog. Known unmetered surfaces (named
  honestly, not silently dropped): golden-vectors-pin-production as a
  mechanical check; `panic_lint` scope beyond decode surfaces; string-typed
  names past the parse boundary; test-bypass doors; `xtask`'s own source,
  excluded from `walk_engine_sources` because its test fixtures (e.g.
  `bs_detector`'s `DETONATIONS` tables) contain banned-shape substrings as
  intentional string-literal samples that a line-based matcher cannot
  distinguish from a live occurrence.
- `walk_engine_sources` covers every first-party workspace crate except
  `xtask` itself (widened from an undisclosed `kyzo-core`/`kyzo-bin`/
  `kyzo-model`-only scope after an audit found `kyzo-trials`, `kyzo-oracle`,
  and `kyzo-crashfs` — the crash-safety proof harness, the independent
  judge, and the fault-injection layer — completely invisible to every
  check).
- Staged follow-ons for this architecture: unify per-check violation
  structs into one `Violation {file, line, seat, reason}` type; lift the
  waiver/ratchet mechanics out of individual checks into declared policy on
  `GateCheck`; migrate the remaining `checks/*.rs` verbs owned by `gate`/
  `authority` (e.g. `authority_graph`, `pure_rust`) into registries of the
  same shape.

One sentence: the gate is a registry of uniform, seat-tagged, bite-proven
checks over one shared context with one forgiveness policy and one frozen
machine surface — the cost of adding law should approach zero, and the gate
should always be able to report its own blind spots.
