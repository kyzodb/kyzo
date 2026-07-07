# Story Gates

Every story preserves the current architectural law. **A gate not run means the story is not
complete.** No success language until gates pass.

## Facts before completion

- commit range
- exact commands run
- test count (pass/fail) + ignored/skipped count
- clippy (own code, both feature configs) and fmt result
- feature-config result (default AND `--features bench-internals,fuzz-internals`)
- benchmark result when performance is touched
- compile-fail result when authority surfaces are touched
- remaining red ledger (`01-no-deferral.md`)

## Gate discipline

- **Verdict-first.** Every status leads with SUCCESS / FAILURE / MIXED / BLOCKED / UNKNOWN and what
  specifically passed or failed. Softening a bad result is lying.
- **The whole workspace, or say it's partial.** Gate `cargo check --workspace --all-targets`, not
  `-p kyzo`. A downstream member that won't compile against a reshaped API is a red gate.
- **Red-green-commit.** build → test → red? fix → green? commit → next. Never advance on red. The
  shared tree never sits unbuildable, in any feature config or workspace member.
- **The finding ladder:** find → fix → sweep the class → **absorb into structure** (make the class
  unrepresentable). A fix that leaves the architecture able to reproduce the bug only relocated it.
- **Fix, don't report.** Every bug found is fixed, landed, and re-verified immediately — the
  maintainer hears about bugs in past tense.
- **Adversarial verification.** Land nothing on the author's word, including your own. Run the
  semantics-reviewer / unsafe-ffi-reviewer agents on the diff after build work is committed-green;
  reviewers spawn cold, never re-pinged. (This pass has caught real silent bugs — it is not optional.)
