# Licensing

This repository is multi-licensed. Which license applies is determined by path,
and — for the MPL parts — by the per-file license header. This file is the map;
when in doubt, the per-file header and this map agree.

## Mozilla Public License 2.0 (open source) — `LICENSE-MPL`

The KyzoDB engine and its hosts are MPL-2.0. KyzoDB is a fork of CozoDB (see
`FORK.md`); the MPL is inherited from that lineage and preserved per file. These
paths, and every file carrying an MPL header, are MPL-2.0:

- `kyzo-core/`
- `kyzo-bin/`
- `kyzo-lsp/`
- `kyzo-crashfs/`
- `kyzo-arrow-interop/`
- `fuzz/`
- `xtask/`
- `vendor/` — the owned storage fork; upstream licenses preserved as noted there.

You may not relicense these files. Modifications to MPL-covered files remain
MPL-2.0.

## Business Source License 1.1 (source-available) — `LICENSE-BSL`

The agent-development and code-intelligence layer — original work, not derived
from CozoDB — is BSL-1.1. These paths are BSL-1.1:

- `.claude/` — the KyzoDB agent tooling: skills, rules, hooks, agents, settings.

The KyzoDB MCP server and the codegraph code lens are also BSL-1.1, but they live in their own
repositories (codegraph is the source of truth, redeployed from there) and are gitignored here, not
tracked in this repo — so they are not part of this path→license map.

Under the BSL you may freely use, modify, and build on this code for any
non-production purpose. Production use — hosting a version of it as a service
(for example, hosting codegraph), or embedding it in a commercial product or
service for a fee — requires a commercial license until the Change Date, after
which it becomes MPL-2.0. See `LICENSE-BSL` for the exact terms and parameters.

## New code

A new file's license follows the path it lives under. A new crate that is wholly
original work (no CozoDB lineage, no MPL header) may be licensed BSL by adding it
to the BSL list above; any file that modifies MPL-covered code stays MPL-2.0.

## Future: per-zone relicensing (not done)

Much of the engine is in fact original work — whole subsystems CozoDB never had
(the `project/` search engines, `data/sketch/`, the `fjall`/`merkle`/crash/sim
storage work, the `oracle`/`trials`/`crashfs` crates) — that currently carries an
MPL header by policy, not by copyright obligation. Those could be relicensed BSL.
This is a deliberate future pass, not done here, and it wants two things first:

1. the target-architecture migration, which physically separates the genuinely
   derivative core (the value plane / memcomparable / datalog evaluator, headed
   for the `kyzo-model` crate and `exec`/`session` zones) from the new subsystems
   above, so relicensing is a per-zone/per-crate decision instead of a per-file
   forensic one; and
2. a legal review, because "heavily rewritten" does not escape derivative-work
   status — a split or refactor of a CozoDB file is still MPL.

Until both are done, the conservative split above holds: MPL on the whole engine,
BSL only on `.claude/`.
