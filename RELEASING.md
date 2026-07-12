# Releasing

The operator runbook for cutting a KyzoDB release. Read [VERSIONING.md](VERSIONING.md) first — this
document is the mechanics, that one is the ruling. If the two ever disagree, VERSIONING.md wins and
this file is wrong and gets fixed.

## Preconditions (all of them, every time)

- [ ] `main` is green: the CI workflow's most recent run on `main`'s current tip concluded
      `success` (`gh run list --repo kyzodb/kyzo --branch main --limit 1 --json headSha,conclusion`).
- [ ] The story-wave being released is sealed: every story in the wave is `Done` on the board
      (`gh project item-list 1 --owner kyzodb`), not "mostly done" or "green except one flake."
- [ ] The board reflects reality: no story claimed by this release is still `In Progress`.
- [ ] `crates/kyzo-core/Cargo.toml`'s `version` (workspace-inherited) has been bumped per the SemVer rule in
      VERSIONING.md and that bump is itself a commit on `main`, ancestor of the tag you're about to
      cut.

None of these are the release workflow's job to double-check for you except the green-main proof
(the workflow re-verifies that one mechanically and refuses if it's false). The other three are
human judgment about the board; keep the board true before you tag.

## Cut the tag

```bash
# from a clean checkout of main, at the exact commit being released
git fetch origin main
git checkout origin/main
git tag -a v0.Y.Z -m "v0.Y.Z"
git push origin v0.Y.Z
```

Pushing the tag is the entire trigger. `.github/workflows/release.yml` takes it from there:
verifies the tag commit is a green, main-ancestor commit; re-runs the full gate hermetically from
that exact commit (never trusts the earlier CI run's artifacts); builds `kyzo-bin` release
binaries and checksums; drafts and publishes the GitHub Release; offers `cargo publish` behind a
manual approval gate. Any refusal at any stage means no Release is created — a partial, silently
incomplete release is exactly the failure mode this pipeline exists to prevent.

## The release notes template

The workflow drafts a Release with this structure; fill in what it cannot infer (the benchmark
section, mainly) before or as part of approving the `publish-crates` gate. Every section is
mandatory — an empty section states "none" or "unchanged," it is never omitted.

```markdown
## Changelog

<commits reachable from this tag, not reachable from the previous tag, that landed a sealed story —
generated from `git log <prev-tag>..<this-tag>`, one line per sealed story, board-issue-numbered>

## Gate summary

<pass/fail for every job in the hermetic gate re-run: pure-Rust, cargo-deny, MPL headers, fmt,
clippy, build, test — the same list `ci.yml`'s gate-summary job prints, re-run from scratch on the
tagged commit, not copied from the earlier CI run>

## FormatVersion

FormatVersion: <N> (<unchanged | bumped from M — migration stance: see VERSIONING.md>)

## Benchmark status

<criterion numbers relevant to this wave, WITH any regressions or losing runs against the prior
release or the standard yardsticks named plainly — see CLAUDE.md: "Benchmarks are instruments,
never gates." A release with a known regression still ships; it ships honestly.>

## Attribution

KyzoDB is a fork of CozoDB (Ziyang Hu and the Cozo Project Authors), MPL-2.0. See FORK.md for the
full lineage. This release changes nothing about that attribution.
```

## Preconditions the workflow enforces mechanically (and refuses loudly if false)

- Tag commit is an ancestor of `main`.
- The GitHub Actions run for CI on that exact commit SHA concluded `success` (checked via the API
  at release time — a stale local memory of "it was green last week" does not count).
- The hermetic gate re-run (fresh checkout, not reused from the original CI run) is fully green:
  build, test, fmt, clippy, pure-Rust gate, unsafe gate.

If any of these is false, the workflow's `verify-provenance` or `gate` job fails red and no
`kyzo-bin` artifacts are built and no GitHub Release is created. There is no partial-credit release.

## crates.io publication

`publish-crates` runs `cargo publish -p kyzo` gated on the `crates-io` GitHub Environment (manual
approval required — see the last-mile checklist in the story #83 PR for the one-time environment
and secret setup). If `CARGO_REGISTRY_TOKEN` is not configured, the job no-ops with a clear message
instead of failing red: publication readiness and the tag/Release pipeline are independent — a
missing crates.io credential never blocks a GitHub Release.

## Cadence

One release per sealed story-wave (see VERSIONING.md). Do not batch multiple sealed waves into one
release "to save a cut" — each wave's changelog entry should be traceable to exactly one tag.
