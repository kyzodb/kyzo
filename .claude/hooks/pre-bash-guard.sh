#!/usr/bin/env bash
# PreToolUse(Bash) hook: catch obvious gate-evasion in shell commands. It is a
# tripwire, not a proof system. It DENIES the clear failure-masking of gate
# commands and WARNS on the rest. It does NOT block git commit/push (this repo
# commits freely as units land).
set -euo pipefail

cmd=$(jq -r '.tool_input.command // ""')
[ -n "$cmd" ] || exit 0

# A heredoc body or a git commit/tag message legitimately QUOTES patterns
# (e.g. a commit documenting this guard). Those are message/script bodies, not
# simple gate invocations, so skip the invocation checks — this tripwire targets
# real commands, not prose. (A heredoc `<<` is the precise signal.)
case "$cmd" in
  *"<<"* | *"git commit"* | *"git tag"*)
    exit 0
    ;;
esac

deny() {
  jq -cn --arg r "$1" \
    '{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:"deny",permissionDecisionReason:$r}}'
  exit 0
}
warn() {
  jq -cn --arg m "$1" \
    '{hookSpecificOutput:{hookEventName:"PreToolUse",additionalContext:$m}}'
  exit 0
}

# 0. CONTAINER-ONLY. Every build/test/clippy/bench runs in the pinned
#    container; a native cargo/just or a hand-set memory/thread limit is a
#    defect (environment.md). Commands that invoke docker are the container
#    path and pass through.
if ! printf '%s' "$cmd" | grep -q 'docker'; then
  if printf '%s' "$cmd" | grep -Eq '(^|[^a-z])cargo[[:space:]]+(test|build|clippy|bench|run|fix|check|mutants|miri)([[:space:]]|$)'; then
    deny "Native cargo is banned. Run it in the container: docker compose run --rm kyzo-dev just <recipe> (gate | test | test-features | clippy | check | bench). The container's cgroup RSS ceiling and pinned RUST_TEST_THREADS ARE the limits — set none yourself (environment.md)."
  fi
  if printf '%s' "$cmd" | grep -Eq '(^|[^a-z])just[[:space:]]+(gate|test|test-features|clippy|check|memcheck|bench|unsafe|pure-rust|fetch)([[:space:]]|$)'; then
    deny "Run gate recipes IN the container: docker compose run --rm kyzo-dev just <recipe> (or kyzo-bench just bench). Never natively (environment.md)."
  fi
fi
# Hand-set memory/parallelism limits are banned outright — the container
# prebakes them.
if printf '%s' "$cmd" | grep -Eq 'ulimit[[:space:]]+-[vm]|--test-threads|RUST_TEST_THREADS='; then
  deny "Never hand-set a memory or thread limit (ulimit -v / --test-threads / RUST_TEST_THREADS). The container's mem_limit and pinned RUST_TEST_THREADS are the honest, prebaked limits. Use: docker compose run --rm kyzo-dev just <recipe> (environment.md)."
fi

# 1. Masking a cargo gate's failure with `|| true` (or `|| :`) — never allowed.
if printf '%s' "$cmd" | grep -Eq 'cargo[^|]*\|\|[[:space:]]*(true|:)'; then
  deny "This masks a cargo gate failure with '|| true'. A gate that cannot fail is not a gate (00-story-gates.md). Run it without the mask and read the real result."
fi

# 2. Running the whole suite with --ignored (the ignored set is timing/scaling
#    probes and benches; running it as if it were the gate is evasion).
if printf '%s' "$cmd" | grep -Eq 'cargo test.*--[[:space:]].*--ignored' \
   && ! printf '%s' "$cmd" | grep -Eq '\-\-ignored[[:space:]]+[a-z_]'; then
  warn "cargo test -- --ignored runs the timing/scaling probes, not the gate. Name a specific ignored test if that is the intent; do not treat the ignored set as the suite (tests-goldens.md)."
fi

# 3. In-place sed on test/fixture/golden files — high risk of silent test
#    weakening.
if printf '%s' "$cmd" | grep -Eq 'sed -i' \
   && printf '%s' "$cmd" | grep -Eq '(tests?/|golden|fixture|_test\.rs)'; then
  warn "In-place sed on a test/fixture/golden. If this weakens an assertion, broadens an error match, or copies a golden from output, it is forbidden (tests-goldens.md). Classify the failure (old-false-behavior / impl-violation / deleted-vocabulary) first."
fi

# 4. Dropping cargo build/test stderr into the void.
if printf '%s' "$cmd" | grep -Eq 'cargo (build|test|clippy)[^&]*2>[[:space:]]*/dev/null'; then
  warn "Redirecting a cargo gate's stderr to /dev/null hides the failure detail you need. Keep the output."
fi

exit 0
