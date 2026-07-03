# CLAUDE.md — KyzoDB

Pure-Rust fork of [CozoDB](https://github.com/cozodb/cozo): one Datalog (KyzoScript) over
relational, graph, vector, and full-text data, with time travel, on one memcomparable transactional
KV substrate (`fjall`). `README.md` is the product; `REFACTOR.md` is the plan.

## The prime directive

Build the greatest possible engine. Effort, size, and tedium are not inputs to any decision — you
do not weigh them, mention them, or let them shape a design. Upstream cozo is a dead reference,
never a justification. There are no deployed stores: no compatibility, no legacy decode paths, no
migration gentleness. When two designs compete, the better engine wins, even if it means rework.

## Anti-deferral (your known failure modes — obey these mechanically)

- **Hardest work first.** Doing ripe work before hard work is deferral in costume.
- **No deferral without a named technical blocker.** "Residual," "follow-up," "later," "out of
  scope," "next camp," and effort-sizing are the words you use when you are avoiding work. If you
  write one, either name the concrete technical blocker or do the work now.
- **Fix, don't report.** Every bug found gets fixed immediately by you, landed, and re-verified.
  The maintainer hears about bugs in past tense.
- **No options menus.** Decide by the prime directive and execute. Present choices only when the
  decision is genuinely the maintainer's (public/irreversible acts, product rulings).
- **Don't document absence — build the thing.** A comment saying something doesn't exist is not
  work product.
- **Name the hard work plainly, then do it.** Never smuggle avoidance into a recommendation.

## How we work

- **Work from the board** (org project KyzoDB Migration, `gh project item-list 1 --owner kyzodb`).
  One story at a time, self-contained; no invented scope without saying so. Keep board status true.
- **One tree, one branch.** All work happens in the real tree on the current branch. No rsync
  copies, no agent worktrees, no parallel patch stacks: isolation defers integration conflicts,
  it does not prevent them.
- **Verify, never assert.** Every claim about code, compilation, tests, or dependencies is backed
  by a real `cargo build`/`cargo test`/run or by reading the file. No conclusions from memory.
- **Never narrow scope to manufacture a clean answer.** Whole-workspace, or say it's partial.
- **One coherent end state.** Align each story to the ideal target; never manage a half-migrated
  middle.
- **Hold the world model.** Read each new message against everything already decided. Do not
  reshape the plan around the latest sentence; standing decisions stay until revoked.
- **A question is not a command.** Nothing public or irreversible (pushes to new surfaces,
  org/repo changes, publishing packages, posting to external projects) without an explicit go.
- **Adversarial verification.** Land nothing on the author's word — including your own. Validate
  with the suite, the oracle differentials, and mutation where a guarantee is new. Re-verify the
  final bytes: a delivered patch once shipped a live mutant its author's green report missed.
- **Memory caps on every cargo run**: `(ulimit -v 12582912 && timeout 1800 cargo ...)`; mutants
  `(ulimit -v 8388608 && timeout 600 ...)`. Two machines have been OOM-killed without them.
- **Benchmarks are instruments, never gates.** Measure before and after; publish the losing runs;
  a regression the instrument catches is a finding to fix, not to hide.

## kyzo-bench (the sibling lane)

Comparative benchmarks and demos, with their foreign toolchains and opponent engines, live in
[`kyzodb/kyzo-bench`](https://github.com/kyzodb/kyzo-bench) (sibling checkout, `../kyzo-bench`) so
this repo's pure-Rust invariant stays machine-enforceable. Story #67 is that lane's brief. The
self-referential trials (determinism campaign, crash matrix, fuzzing ledger, proof audit) are tests
here. A bench-exposed engine defect becomes an issue in this repo and gets fixed here.

## Guardrails (high blast radius — verify around every change)

- **memcmp key encoding** (`data/memcmp.rs`): bytewise key order equals semantic value order. It is
  the on-disk format; any change is a format migration with round-trip + ordering tests before and
  after, and a FormatVersion decision.
- **Storage contract**: ordered range scans, SSI over reads and writes, consuming commits,
  validity-in-key time travel. Sealed; changes get a contract-history entry.
- **Query semantics**: the naive oracle (`query/laws.rs`) is judge. Any eval change runs the
  differentials; the refusal corpus stays refused; stratification and termination get an explicit
  argument.
- **Pure Rust**: no C/C++ compiler anywhere in the `kyzo-core`/`kyzo-bin` build. FFI lives only in
  the bindings (C ABI, pyo3, jni, neon, swift-bridge, wasm-bindgen), each an unsafe zone.
- **The bindings are committed work.** All six in-workspace plus Go, Clojure, Android, and the
  Python client get ported, rebranded, built, tested, published. Never reframe as optional.
- **The core is isolated**: everything depends on `kyzo-core`; it depends on nothing of ours. That
  ordering is the dependency graph, not permission to skip bindings.

## Build, test, gate

    cargo build -p kyzo --release
    cargo test  -p kyzo --release

A seal requires: full suite green, `cargo clippy --release --all-targets -- -D warnings` clean in
both feature configs, `cargo fmt --check` clean.

## Licensing and attribution

- MPL-2.0. Preserve every CozoDB copyright header and all attribution verbatim; add ours alongside,
  never overwrite. Credit the original authors; never imply endorsement.
- Incorporated contributor fixes keep their original git authorship.
- Qdrant-derived design work keeps its Apache-2.0 attribution verbatim.
