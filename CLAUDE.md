# CLAUDE.md — KyzoDB

Pure-Rust fork of [CozoDB](https://github.com/cozodb/cozo): one Datalog (KyzoScript) over
relational, graph, vector, and full-text data, with time travel, on one memcomparable transactional
KV substrate (`fjall`). `README.md` is the product; the board (org project KyzoDB Migration) holds
the plan, one story at a time.

## Prime directive

Build the greatest possible engine. Effort, size, and tedium never factor into a decision: don't
weigh, mention, or let them shape a design. Upstream cozo is a dead reference, never a
justification. No deployed stores exist: no compatibility, no legacy decode paths, no migration
gentleness. Between competing designs, the better engine wins, even at the cost of rework.

## Anti-deferral (known failure modes, obeyed mechanically)

- **Hardest work first.** Doing ripe work before hard work is deferral in costume.
- **No deferral without a named technical blocker.** "Residual," "follow-up," "later," "out of
  scope," "next camp," and effort-sizing are words used to avoid work. Name the concrete blocker,
  or do the work now.
- **Fix, don't report.** Every bug found gets fixed, landed, and re-verified immediately. The
  maintainer hears about bugs in past tense.
- **No options menus.** Decide by the prime directive and execute. Present choices only when the
  decision is genuinely the maintainer's (public/irreversible acts, product rulings).
- **Don't document absence, build the thing.** A comment saying something doesn't exist is not
  work product.
- **Name the hard work plainly, then do it.** Never smuggle avoidance into a recommendation.

## Session discipline (learned the hard way, binding)

- **Verdict-first reporting.** Every status leads with SUCCESS / FAILURE / MIXED / BLOCKED /
  UNKNOWN and what specifically passed or failed. Softening language on a bad result is a form
  of lying.
- **The finding ladder has four rungs:** find → fix → sweep the class → **absorb into structure**
  (change the architecture so the class is unrepresentable). A fix that leaves the architecture
  equally able to reproduce the bug has only relocated it. Guards accumulate; structure absorbs.
- **Investigations are decision procedures, not expeditions.** Never brief an agent "investigate
  X." Form hypotheses first (read the code, read the prior art; a five-minute grep once killed a
  theory an agent would have chased for 100k tokens), then dispatch ordered experiments where
  every branch eliminates a hypothesis. Give established facts the agent must not re-derive, and a
  stop condition at the first discriminator.
- **Comments are public claims.** A stale doc comment is a false statement to every future reader
  (external auditors have twice been misled by ours). Doc drift is a defect class, fixed on sight.
- **Durable state lives in durable places.** Snapshots, findings, and rulings go to issue comments
  at every phase boundary, never only tmpfs or an agent's context. A host crash costs a restart,
  not a reconstruction; an unreported result in a dead agent's memory never existed.
- **Controlling agents is the coordinator's first job.** Unleashed, they burn the session. Treat
  them like junior devs who will plan, fan out, and re-derive settled work unless forbidden. Every
  brief states its forbidden-by-default envelope: no sub-agent fan-out without your go; no
  planning or re-derivation on mechanical work (find-replace stays find-replace); a token/scope
  ceiling; report at the first checkpoint, never run silent (a trivial sweep reporting 175k tokens
  is a kill, not a thing to explain). Kill escapism (planning-in-costume, fan-out, chunk-splitting)
  on the first sign, not after the fact. One task equals one brief's scope; the tracker matches the
  brief (broader wording once caused prohibited work). A sent report ends the edit license; new
  work needs a new instruction. Reviewers spawn cold, never re-pinged (each ping replays their
  context; fresh eyes are a better adversary). Agents never idle without reporting.
- **The shared tree never sits unbuildable,** in any feature configuration, including workspace
  member manifests. Mid-flight breakage blocks every other builder's gate runs.
- **Inbound defect reports are triaged the turn they arrive:** milestone, owner, attack, before any
  process discussion they also raise. A report parked while its cover letter is discussed is the
  "courageously documented" anti-pattern.
- **Claims close on external verification where one exists.** A perf/regression issue opened by
  the bench lane closes on their re-measurement of the merged fix, never on our own numbers (we
  once published a closing claim measured on a code path users never take).
- **A claim can be true and untestable.** The honest response is saying so in the code, or
  building the observability that makes it testable (prefer the latter, e.g. write-count laws for
  write-amplification claims).
- **Red-green-commit; review is a later phase.** Per build unit: build → test → red? fix → green?
  commit, then push the branch → next. A commit is an unwind point, not a seal (revert or
  force-push the feature branch fixes anything). Never advance on red. Never let the shared tree
  become an uncommitted parallel-edit soup (proven the hard way: the same tree gave 3 failures one
  run and 40 the next because builders were mid-edit). Commit each unit's own files as they go
  green; don't run convoy-wide verification while a builder holds shared-dependency files mid-edit.
  Hostile review and deeper architecture bug-hunting are a separate phase, after all of a
  milestone's build work is committed-green and its build-caught bugs are fixed. May be relaxed by
  operator for development activities deemed higher-certainty; check conversation history for
  explicit instructions.
- **When a blocker clears, re-walk its queue the same turn.** A parked item whose blocker resolved
  and then sat is deferral by neglect. Dispatch what a dependency unblocked the moment it frees.

## How we work

- **Work from the board** (org project KyzoDB Migration, `gh project item-list 1 --owner kyzodb`).
  One story at a time, self-contained; no invented scope without saying so. Keep board status true.
- **One tree, one branch.** All work happens in the real tree on the current branch. No rsync
  copies, no agent worktrees, no parallel patch stacks: isolation defers integration conflicts, it
  does not prevent them.
- **Verify, never assert.** Every claim about code, compilation, tests, or dependencies is backed
  by a real `cargo build`/`cargo test`/run, or by reading the file. No conclusions from memory.
- **Never narrow scope to manufacture a clean answer.** Whole-workspace, or say it's partial.
- **One coherent end state.** Align each story to the ideal target; never manage a half-migrated
  middle.
- **Hold the world model.** Read each new message against everything already decided. Don't
  reshape the plan around the latest sentence; standing decisions stay until revoked.
- **A question is not a command.** Commit and push the working branch freely as units land. The
  go-gate is only for the public/irreversible: merging to main, tags/releases, publishing
  packages, new remotes or external projects.
- **Adversarial verification.** Land nothing on the author's word, including your own. Validate
  with the suite, the oracle differentials, and mutation where a guarantee is new. Re-verify the
  final bytes (a delivered patch once shipped a live mutant its author's green report missed). May
  be relaxed by operator for development activities deemed higher-certainty; check conversation
  history for explicit instructions.
- **Memory caps on every cargo run:** `(ulimit -v 12582912 && timeout 1800 cargo ...)`; mutants
  `(ulimit -v 8388608 && timeout 600 ...)`. Two machines have been OOM-killed without them.
- **Benchmarks are instruments, never gates.** Measure before and after; publish the losing runs.
  A regression the instrument catches is a finding to fix, not to hide.

## kyzo-bench (the sibling lane)

Comparative benchmarks and demos, with their foreign toolchains and opponent engines, live in
[`kyzodb/kyzo-bench`](https://github.com/kyzodb/kyzo-bench) (sibling checkout, `../kyzo-bench`), so
this repo's pure-Rust invariant stays machine-enforceable. Story #67 is that lane's brief. The
self-referential trials (determinism campaign, crash matrix, fuzzing ledger, proof audit) are tests
here. A bench-exposed engine defect becomes an issue in this repo and gets fixed here.

## Guardrails (high blast radius, verify around every change)

- **memcmp key encoding** (`data/memcmp.rs`): bytewise key order equals semantic value order. It
  is the on-disk format; any change is a format migration with round-trip + ordering tests before
  and after, and a FormatVersion decision.
- **Storage contract:** ordered range scans, SSI over reads and writes, consuming commits,
  validity-in-key time travel. Sealed; changes get a contract-history entry.
- **Query semantics:** the naive oracle (`query/laws.rs`) is judge. Any eval change runs the
  differentials; the refusal corpus stays refused; stratification and termination get an explicit
  argument.
- **Pure Rust:** no C/C++ compiler anywhere in the `kyzo-core`/`kyzo-bin` build. FFI lives only in
  the bindings (C ABI, pyo3, jni, neon, swift-bridge, wasm-bindgen), each an unsafe zone.
- **The bindings are committed work.** All six in-workspace, plus Go, Clojure, Android, and the
  Python client, get ported, rebranded, built, tested, published. Never reframe as optional.
- **The core is isolated:** everything depends on `kyzo-core`; it depends on nothing of ours. That
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