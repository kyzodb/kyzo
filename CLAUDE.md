# CLAUDE.md — KyzoDB

KyzoDB is a pure-Rust database engine where relational, graph, vector, full-text, geospatial, and temporal data share one ordered substrate and one query language.

One KyzoScript Datalog query operates over one memcomparable transactional KV substrate: `fjall`.

`README.md` defines the product.
The `KyzoDB Work` organization project defines the plan.

## Foundational contract

Every value KyzoDB can store encodes to bytes whose binary order equals its semantic order.

This applies across numbers, strings, vectors, geometry, timestamps, nested collections, and every other value kind.

That contract allows one ordered KV substrate to support relational queries, graph traversal, vector similarity, full-text search, geospatial search, and time travel through one language.

A sort-order defect in an ordered store may return incorrect results without raising an error. Therefore the encoding contract is executable law, enforced through cross-type property tests, corruption harnesses, mutation testing, and continuous fuzzing.

## Operating model

Read the focus stories, applicable rules, relevant code, and enforcement artifacts before changing the tree. Never claim facts about code you have not inspected.

Implement ruled work directly. Do not merely suggest changes.

Use the smallest implementation that completely satisfies the ruling. Do not introduce speculative flexibility, compatibility behavior, unrelated refactors, or abstractions for hypothetical future work.

When a ruling determines the invariant, behavior, structure, or dependency order, make the implementation decision and continue.

When a ruling is missing, ambiguous, contradictory, or disproved by repository reality, stop and surface the exact unresolved design decision. Do not hide a design decision inside working code.

## Authority order

When instructions conflict, apply this order:

1. This constitution.
2. Applicable `.claude/rules/*.md`.
3. The focused story and its accepted architecture ruling.
4. Target-state placement from `architecture-map`.
5. Existing implementation and tests.
6. Upstream Cozo behavior.

Existing code, old tests, convenience, release pressure, and upstream precedent do not override a ruling.

## Enforcement stack

Rules teach. Hooks interrupt. Tests prove. Gates enforce.

* `CLAUDE.md` — repository constitution.
* `.claude/rules/zone-*.md` — target architecture by path.
* `.claude/rules/deprecated-*.md` — migration law for legacy files.
* `.claude/skills/architecture-map/SKILL.md` — target-state placement authority.
* `.claude/settings.json` and `.claude/hooks/*.sh` — context injection and command/edit enforcement.
* `.claude/hooks/inject-work-context.sh` — injects every open `In Progress` story carrying the `focus` label.
* `.claude/hooks/pre-bash-guard.sh` — blocks container evasion and prohibited commands.
* `.claude/hooks/post-edit-guard.sh` — blocks unsafe-policy and edit-time violations.
* `.claude/hooks/focus-gate.sh` — denies engine edits without a focus story.
* The authority graph — extracts `@authority` declarations, emits committed authority artifacts, and audits authority drift.
* The repository seal — the full verification stack, executed only in the dev container.

The `manage-board` skill through the Kyzo MCP server is the only board writer.

## Work authority

The active work set is every open board story that:

* is `In Progress`; and
* carries the `focus` label.

Stories and epics are created with `write-story` and `write-epic`. Board state changes go through `manage-board`.

Do not change engine code without an active focus story.

Move cards when repository reality changes. Do not leave board state knowingly stale.

Commit complete units as they land. A commit must represent one coherent, verified change rather than an arbitrary checkpoint.

Delegated work is supervised work. While a subagent runs, lurk on its transcript and spot-check its choices mid-flight — behavior observed in action is holistic evidence of the whole, and one early correction replaces an exhaustive post-review. Judge strictly and correct immediately; leniency early is paid for later, twice.

At delegation time, arm recurring check timers in the same motion as the spawn — a monitor ticking observable progress signals every few minutes. The lurk happens on schedule, never on memory; a stalled signal is itself the alarm. Watch the reliable meter: committed artifacts (branch commits, file scope, pathspec discipline) read from git refs, not the working index — never contend with a running agent for the index lock. Transcript size and mtime are the cheap liveness signal (small and recently-written is a lean, live agent); do not conclude bloat or stall from a monitor that cannot read the file — verify with the instant meter before killing. The demonstrated failure of a well-scoped agent is over-caution and non-shipping, not recklessness: an agent that has proven its work but will not commit is the thing to nudge; guard against the stall as hard as against the runaway.

Delegate one task at a time, not a whole story, to a fresh per-task agent that executes it and dies — context never accumulates across tasks, and each committed task is a mandatory checkpoint where the orchestrator verifies scope, bounds, and build before spawning the next. The agent's contract is two doors: execute the task exactly, or escalate a blocker in one line. When an agent escalates a genuine defect in the story itself — a mis-scoped condemned path, an unbuildable requirement — the fix is to repair the story and re-rule, not to force the agent through the wrong instruction. A story the orchestrator over-scoped is trimmed to the real work, in the open, when reality proves the excess.

## Build and verification

Run Cargo, binaries, tests, gates, and benchmarks only through their declared containers: `kyzo-dev` for verification, `kyzo-bench` for benchmarks. The tree declares the current seal and bench entry points; invoke those and nothing else.

The seal includes:

* environment reporting;
* `cargo check --workspace --all-targets`;
* formatting;
* first-party Clippy with `-D warnings`;
* unsafe-code guards;
* pure-Rust dependency guards;
* authority self-test, ratchet, and artifact freshness;
* mutation testing of the enforcement harness;
* the full test suite.

Do not run native build, test, or lint tooling on the host.
Do not hand-set `ulimit`, `timeout`, `--test-threads`, or equivalent execution limits.
Run repository binaries only through the declared container entry points.

## Failure triage

Before changing code in response to red, classify the failure:

1. implementation defect;
2. test defect;
3. ruling defect.

Fix implementation defects.

Correct tests only when they fail to express the ruling. Never weaken a valid test, reduce its scope, delete meaningful coverage, or rewrite it around the implementation.

Surface ruling defects instead of working around them.

Tests verify the law; they do not create it. Goldens must be independently derived. Healthy-path tests must construct values through production APIs rather than privileged test-only doors.

## Requirements are not negotiable down

Never propose weakening, deleting, narrowing, or reinterpreting a requirement, acceptance criterion, Condemned item, or engineering contract as an acceptable substitute for implementing it. If you believe the requirement itself is incorrect, state that separately, but classify the current state as "requirement not satisfied." Do not present modifying the requirement as completion of the work.

## Global laws

### Build integrity

1. The entire workspace always builds under `cargo check --workspace --all-targets`.
2. Every first-party crate root contains `#![forbid(unsafe_code)]`.
3. No `#[allow(unsafe_code)]` exists.
4. Documentation must not claim an unsafe exception that does not exist.
5. Every first-party crate and feature configuration remains covered by the gate.
6. A compatibility shim is not an acceptable substitute for the ruled architecture.

When a rule conflicts with release compilation, preserve the rule and expose the failure. Do not make the repository green by moving the architecture backward.

### Pure-Rust substrate

7. No C or C++ may enter any first-party dependency tree.
8. Do not add storage-backend feature flags.
9. The build must remain valid in the repository Docker image without a C compiler.

### Scope and execution

10. Change only what the focused story and its necessary consequences require.
11. Name and path changes cascade across code, tests, documentation, rules, hooks, maps, baselines, CI, and every other reference in the same change unless the ruling explicitly limits the cascade.
12. Prove a rename or move with a stale-reference sweep.
13. Remove every condemned path named by the story.
14. Do not preserve condemned behavior through aliases, shims, duplicate paths, fallback dispatch, or hidden compatibility.
15. Remove temporary scripts, files, and iteration artifacts before completion.
16. Do not create worktrees without operator approval.
17. Do not spawn subagents without operator authorization.
18. Public, destructive, shared, or difficult-to-reverse actions require their declared operator authorization. Never bypass hooks or checks to perform them.

### Architecture quality

19. Build the strongest ruled engine. Effort, size, repetition, and rework do not justify a weaker design.
20. The architecture does not move backward.
21. Upstream Cozo is historical evidence, not design authority.
22. Avoid accidental complexity, ceiling-lowering compromises, incomplete abstractions, and deferred correctness.
23. Do not add abstractions, helpers, configurability, validation, fallbacks, or defensive branches unless the current ruling requires them.
24. Implement the actual general law, not behavior specialized to visible tests.
25. Continue through long tasks and context compaction. Context pressure is not a completion condition. Preserve progress in repository state and resume from evidence.

### Type authority

26. Types carry domain authority at the decision site.
27. Do not represent closed domain meaning as string comparisons, bare numeric taxonomies, duplicate counters, raw blobs, or untyped dispatch.
28. `@authority` declarations must remain complete and accurate.
29. Raw construction doors, string taxonomies, duplicate counters, and blob meaning may not exceed the committed authority ratchet.
30. Allowlist changes may only narrow existing exceptions unless a new ruling explicitly authorizes otherwise.
31. A diagnostic code may render an error variant but must never replace typed dispatch.

### Errors

32. Every first-party crate exposes closed typed refusal values.
33. Do not erase first-party failures behind `anyhow`, `Box<dyn Error>`, or equivalent shipped interfaces.
34. Do not add `Other(String)`, `Unknown`, or catch-all error variants.
35. Do not use wildcard arms over first-party error enums.
36. Error fields must preserve information a caller is permitted to branch on as typed values.

### Tests and evidence

37. Never weaken a valid test.
38. New `#[ignore]` annotations are violations unless explicitly ruled.
39. The enforcement harness mutation test must run and pass.
40. Verification precedes assertion.
41. Do not conceal a regression behind explanation, qualification, or progress narrative.
42. Performance claims close only through the benchmark lane.
43. A measurement report includes the executed command, environment, workload, relevant configuration, and observed result.
44. A copied expected value is not an independently derived golden.

### Enforcement

45. Convert semantic rules into deterministic checks whenever a reliable mechanical form becomes possible.
46. Hooks, ratchets, tests, and gates must reject violations rather than merely document them.
47. Do not bypass enforcement with alternate commands, paths, tools, environment changes, or equivalent behavior.
48. `manage-board` is the only board writer. Do not mutate project state through raw `gh` commands or another path.

### Licensing

49. Preserve MPL headers verbatim in MPL-covered files.
50. Do not place MPL headers in BSL-covered `.claude/` files.
51. The engine and hosts remain MPL-2.0 unless a deliberate, reviewed zone ruling changes them.
52. Agent tooling under `.claude/` is BSL-1.1.
53. `LICENSING.md` is the authoritative path-to-license map.
54. Relicensing is a deliberate legal and architectural decision, never incidental cleanup.

### Unsafe protocol

55. Unsafe Rust is prohibited by default.
56. Any proposed exception requires a separate ruling that names the otherwise unprovable invariant, constrains the unsafe surface, supplies a complete safety case, and adds mechanical enforcement.
57. Until that process completes, the tree remains `forbid(unsafe_code)`.

## Authority graph

The authority graph extracts `@authority` declarations into:

* `authority/authority-map.json`;
* `authority/authority-report.md`.

It audits:

* raw construction doors;
* missing authority coverage;
* string-controlled taxonomies;
* duplicate generation or identity counters;
* domain meaning hidden in blobs;
* ratchet and allowlist drift;
* committed artifact freshness.

Run the authority graph through the repository seal. Do not manually edit generated authority artifacts.

## Completion

Completion is total.

Before claiming a story is done:

1. compare the repository state against every story obligation;
2. confirm all ruled behavior is implemented;
3. confirm required constructors and tests exist;
4. confirm condemned paths and behavior are absent;
5. run the required builds, tests, gates, and benchmarks;
6. remove temporary artifacts;
7. sweep for stale names, paths, documentation, rules, maps, baselines, and CI references;
8. confirm code, tests, documentation, authority artifacts, and board state describe the same reality;
9. run the required completion integrity skill.

Do not report `done`, `complete`, `mostly done`, or an equivalent claim while any required work is deferred, narrowed, failing, unverified, or represented inconsistently.

A green gate proves only what the gate measures. You remain responsible for semantic conformance.

## Licensing map

The engine and hosts are MPL-2.0 because of the Cozo lineage and repository policy. Agent tooling under `.claude/` is BSL-1.1.

A new file inherits the license of its path:

* MPL-covered files preserve the applicable MPL header.
* `.claude/` files carry no MPL header.

Original engine work may currently remain MPL by repository policy rather than copyright necessity. Any later BSL relicensing occurs only by deliberate subsystem ruling after derivative and original zones are separated and legal review is complete.

## Origins

KyzoDB began as a fork of CozoDB by Ziyang Hu and the Cozo Project Authors. Cozo demonstrated that the one-substrate architecture was worth pursuing.

The full history and attribution are in `FORK.md`.
