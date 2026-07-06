# Task #36 — 0.9.0 release recovery + bug ledger (PARKED)

Branch story-62-deeper-time, HEAD e92703d PUSHED (0 local-only) at time of
capture. This predates the #118/#119 work; reconcile against current main
before acting.

## Bug ledger (all found in the earlier session)
- apply_pending corruption — FIXED (a686800).
- div/mod + float-mod-by-zero — FIXED (e92703d).
- UNBOUNDED-RECURSION hang (#68): fix = default `derived_tuple_ceiling` in
  build_budget/eval.rs — DETERMINISTIC, run-everything-catch-runaway; deadline
  stays opt-in for determinism; NOT a static refuse. (Was mid-flight via agent
  a022aa5f — verify landed.)
- MATH-DOMAIN silent-NaN (sqrt(-1)/ln/asin/acos/acosh/atanh -> null): fix =
  functions.rs typed domain error. (Was mid-flight via agent a3b6a6a30 — verify
  landed.)
- same_generation = NOT a bug (transient).

## Uncommitted drafts (were on disk for maintainer review)
- VERSIONING.md + CONTRIBUTING.md — ready.
- README.md — polished BUT needs RHETORIC REWORK. Ruling: lead with VALUE +
  robustness ("cannot crash it, always typed errors"), NOT correctness (=table
  stakes). README `DbInstance::new`/`run_default` example was UNVERIFIED — verify
  it runs.

## Milestones
Engine 0.9.0 (release) -> Engine v1.0 (perf #68/#82 + hardening #96-#101) ->
Benchmarks. #62 CLOSED (feature done; >=10x bench -> #28 post-tag).

## Release path
land bug-fix agents -> cleanup #21 (see task-21 file) -> merge branch to main
(reconcile main's 3 release-eng commits: VERSIONING/CHANGELOG/release.yml) ->
README value-first rework + verify examples -> tag v0.9.0 (NEEDS EXPLICIT GO)
-> bench post-tag closes #82.

New test infra already pushed: adversarial_robustness.rs, integration suite
32860f0, debug_asserts + coverage + overflow-profile.

NOTE: a milestone name ("Engine 0.9.0") is not the version tag; one release per
sealed story-wave, from green main only. Tag needs maintainer GO.
