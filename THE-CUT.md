# THE CUT

Wake up. You have no prior context. Read this file as law for this pass.

## What this is

Two jobs only: (1) get the **target-state ontology** right — every construct seated by the kind of truth it is; (2) **script the cut and run it** in bulk. This is not a careful migration. This is not V1. This is not production. The tree may stay red until the whole board is done.

**Max purity is the source of truth.** Not this file. Not `target-arch.md`. Not the deprecated census. Not a green `cargo check`. Helpers get reconciled against zone law, the storage architecture already ruled, and the live tree — then you seat. When a helper and purity disagree, the helper loses. Pattern-matching a stale doc into a pretty layout is failure.

## Why the “safe” path is the real risk

Construct-by-construct fear, compile-between-moves, and preserving condemned surfaces “so something still builds” have already caused rework and delay. That is not safety. That is documented harm.

A type-uplift epic already closed without finishing this seating. Next is a storage-architecture epic whose design is already ruled — it will drive massive change. **Do not polish corpses you are about to kill.** Spending cycles to keep today’s bags compiling is paying interest on lies.

Remaking a bunch of implementation later against a true seat is *better* than carrying a bag that compiles. The museum lives in **git history**, not in the tree. When a future gap needs an old shape, diff commits. Do not keep the old shape warm in-tree.

Greater safety, lower risk, lower burden, higher intelligence: one hard cut into an honest ontology; red until the board closes; build each truth once against the architecture you already decided.

## Ontology

We already earned the zone one-liners. This pass refines them from what we have learned since and from what storage will demand — not from old maps.

Seat every construct by the kind of truth it is. Prefer a split into a context the future will clearly own over parking two truths under one name because they can cohabit today. **Bag filenames that mean two things are the enemy of the foundation.** Relocating a bag under a new path is still a bag — that is not a migration, that is fraud against the ontology.

Priority order for the cut (purity first, not “easiest delete”):

1. **Cut welds** — files that fuse declaration with implementation, or model meaning with exec/session/store behavior (`data/aggr.rs`, `data/expr.rs`, `data/program.rs`, `data/functions.rs`, and the other named splits in `docs/deprecated/`). Script the *partition*. Destination seats get the declared halves; the source dies.
2. **Enforce zone walls** — smash old roots (`data/`, `engines/`, `query/`, `runtime/`, `storage/`, `fixed_rule/`) into `model` / `exec` / `project` / `session` / `store` / `rules` / trials / oracle by kind of truth. Named splits in the deprecated files are the scalpel — paste them into the spawn; do not vibe.
3. **Collapse duplicate seats** — one tree per concept (e.g. do not leave flat stubs and a nested twin both pretending to be the seat).
4. **Then** overlay already-pure 1:1 faces, delete sealed doors, delete the retired museum. Cleanup trails the ontology; it does not lead it.

Maps, census rows, prior seating tables, and `docs/deprecated/deprecated-*.md` L1/L2 blocks are **helpers and scalpels**. They are not trusted as authority. Use their inventories and destination lists when they match purity; override them when they don’t. Observation, induction, deduction — then name the seat.

## The script

Once the seating table is ruled, encode it and execute in one hard cut. Bulk apply is the default: faster and safer than construct-by-construct fear. **Red after the cut is signal** — the diff and the table locate the break; fix from there. Do not optimize for a perfect first compile. Optimize for a complete, interrogable cut from a true ontology.

Do not test between moves. Do not ask permission to leave the tree broken. Git is the fallback. The kill list below is closure: every path on it is gone when the cut is done — not “gone from the happy path,” **gone**.

## Agent handcuffs (read twice)

Task agents will try to be helpful. Helpfulness here is how the cut dies. Assume they will:

- treat red as an emergency and `git restore` / checkout condemned paths
- resurrect old zone shells (`data/`, `engines/`, `runtime/`, …) as compatibility shims
- copy a whole weld into a new seat and rename until it compiles (bag relocation)
- shrink the job to manufacture green

**Firm hand — non-negotiable:**

- Allowlist only. Edit/Write those paths. Nothing else.
- No Bash. No git. No board mutate. First offense whip; second kill.
- Split means split. No “keep a facade in the old zone so imports work.”
- Red is success mid-cut. The meter is “seats match the partition,” not `cargo check`.
- Path firehose over prose. Off-allowlist path → whip. Restoring a kill-list path → kill.
- One chunk per spawn. Paste the exact L1/L2 bullets. Not “migrate `data/`.”
- Parent owns Check, judge, and restore. Restore never means “put the old bag back.” Only the operator may undo a cut.

