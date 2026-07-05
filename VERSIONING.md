# Versioning

This is the ruling, not a discussion. It governs every crate in this workspace and every artifact
built from it.

## The line is ours

KyzoDB versions on its own `0.Y.Z` line, and its first public release is **0.9.0**. It does not
track, mirror, or imply compatibility with upstream
[CozoDB](https://github.com/cozodb/cozo)'s version numbers.

Why: KyzoDB is a massively-diverged fork. The storage backend is different (fjall, not
RocksDB/SQLite), the on-disk format is different, the storage contract is rebuilt, and large parts of
the query engine were re-derived from first principles against an independent oracle. Reusing
upstream's numbers would claim a compatibility promise this project does not make. Lineage and credit
belong in [FORK.md](FORK.md) and the source headers, verbatim, not in the version string.

## SemVer 2.0, pre-1.0 semantics

Every crate follows [SemVer 2.0.0](https://semver.org/), with the pre-1.0 reading SemVer itself
specifies:

- **`0.Y` (minor) = breaking.** Any change to the public API, the query-language semantics, or
  on-disk compatibility that would be a major bump after 1.0 is a minor bump before it. Pre-1.0,
  breaking changes are expected.
- **`0.Y.Z` (patch) = backward-compatible** fixes and additions.

Depend on an exact version (`=0.9.0`) if you need stability across an upgrade you haven't reviewed.

## Why 0.9.0 and not 1.0.0

0.9.0 is an honest number. The engine is feature-complete for its scope and its correctness is
proven: serializable transactions, crash recovery, an oracle-verified query semantics, bitemporal
time travel, a shipped `::verify` self-check. But **1.0.0 is an earned commitment, not a feature
count**, and two of its conditions are not yet met:

- the **public API is not frozen**: it is young and still moving;
- **performance is not yet verified at scale** on current code against the public benchmarks.

1.0.0 ships only when the API is frozen (breaking changes then require `2.0.0`), performance is
measured and published against the standard yardsticks, and the project can make a production-
readiness statement it stands behind. Until then we stay on `0.Y.Z`.

## Three independent version numbers

These move on their own axes; do not infer one from another.

- **Crate SemVer** (`0.9.0`): the public Rust/language API surface.
- **FormatVersion** (an integer, currently **4**): the on-disk key/value format. Bumped *only* on a
  format change, and only with round-trip and ordering tests before and after. A crate minor bump
  does not imply a format change.
- **MSRV**: the minimum supported Rust version, declared in the workspace manifest.

## Releases come only from green main

A release is tagged only from a `main` commit whose CI is fully green, and only through the
tag-triggered release pipeline, which re-verifies the pure-Rust, unsafe, fmt/clippy, and
whole-workspace build/test gates hermetically from the tag checkout before publishing artifacts. A
number that did not come from that pipeline is not a KyzoDB release.
