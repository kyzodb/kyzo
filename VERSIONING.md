# Versioning

This is the ruling, not a discussion. It governs every crate in this workspace and every artifact
built from it.

## The line is ours

KyzoDB versions on its own `0.Y.Z` line, starting at `0.1.0`. It does not track, mirror, or imply
compatibility with upstream [CozoDB](https://github.com/cozodb/cozo)'s version numbers.

Why: KyzoDB is a massively-diverged fork. The storage backend is different (fjall, not
RocksDB/SQLite), the on-disk format is different, the storage contract is rebuilt, and large parts
of the query engine have been re-derived from first principles against an independent oracle.
Reusing upstream's version numbers would claim a compatibility promise — "this behaves like Cozo
vX.Y.Z" — that this project does not make and will not keep. Lineage and credit belong in
[FORK.md](FORK.md) and the source headers, verbatim, forever. They do not belong in the version
string.

## SemVer 2.0, pre-1.0 semantics

Every crate in this workspace follows [SemVer 2.0.0](https://semver.org/), with the pre-1.0 reading
SemVer itself specifies:

- **`0.Y` (minor) = breaking.** Any change to public API surface, query-language semantics, or
  on-disk compatibility that would be a major bump post-1.0 is a minor bump pre-1.0. There is no
  quiet-breaking-patch category.
- **`0.Y.Z` (patch) = compatible.** Bug fixes, new capabilities that don't change existing behavior,
  performance work, and documentation land as patches.
- There is no `0.Y.Z-something` pre-release channel yet. If one is needed (a release candidate ahead
  of a risky cut), it is named explicitly in the release notes when it happens, not assumed.

## `v1.0.0` is a milestone, not a date

`v1.0.0` is reserved for the completion of the [Engine v1.0
milestone](https://github.com/orgs/kyzodb/projects/1): the full engine, standing, adversarially
proven, with every guardrail in this repo's `CLAUDE.md` load-bearing. It is cut when that milestone
closes, whenever that is. No calendar pressure moves this number.

## The three independent axes

Three numbers describe a KyzoDB release, and they do not move together. Conflating any two of them
is the mistake this section exists to prevent.

| Axis | Where it lives | What it promises | Who bumps it |
|---|---|---|---|
| **Crate SemVer** | `Cargo.toml` `version` (workspace-inherited) | API/behavior compatibility per the rules above | Every release, per the changes it carries |
| **FormatVersion** | `kyzo_core::FormatVersion` (`crates/kyzo-core/src/storage/mod.rs`), an integer stamped into every store at creation | On-disk byte-layout compatibility — a store written at one FormatVersion either opens at that exact version or is refused, never silently misread | Only when the on-disk encoding changes; independent of the SemVer bump size |
| **MSRV** | `rust-version` in the workspace `Cargo.toml` (currently **1.93**) | The minimum Rust toolchain that can build this crate | Raised deliberately, stated in the release that raises it |

A release's notes always state all three explicitly (template in [RELEASING.md](RELEASING.md)),
even when a given axis didn't move — "FormatVersion: 4 (unchanged)" is a claim, not a placeholder.
There is no migration tooling for FormatVersion bumps yet: pre-1.0, a bump is a stated breaking
change with an explicit migration stance (currently: none exists; a store must be recreated across a
FormatVersion boundary). That stance itself is part of what a release states, and changes only when
migration tooling lands as its own reviewed piece of work.

## Releases come only from green main

A release is a tag pointing at a commit that is:

1. An ancestor of `main` (no releasing a side branch, no releasing ahead of what's actually merged).
2. The commit CI reported green on, verified at release time, not assumed from memory. See
   `.github/workflows/release.yml`, job `verify-provenance`.

Nothing about producing a release depends on any machine but CI. A release built on a contributor's
laptop, however green their local run, is not a release.

## Cadence

One release per sealed story-wave. A "wave" is the set of board stories that land together and get
marked sealed; when a wave seals on green main, that is the trigger to cut the next `0.Y.Z`, not a
calendar interval. See [RELEASING.md](RELEASING.md) for the operator steps.

## Public interface

As of this ruling, the **only** public interface to KyzoDB's history is `v0.Y.Z` git tags and their
paired GitHub Releases (crates.io publication follows the same tags — see RELEASING.md). Dev-revision
git SHAs are not a public interface: any pinning against a specific commit (the bench lane, a
downstream binding, a private integration) is private coordination between teams, not something this
project supports or stabilizes. If you depend on KyzoDB from outside this org, depend on a tag or the
crates.io version, never a commit.
