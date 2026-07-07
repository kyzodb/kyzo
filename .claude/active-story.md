story: 119 — the value plane
branch: story-119-value-plane
status: in-progress

acceptance-boundary:
  - 16-byte tagged cell over the epoch-scoped interning arena; one canonical byte format
  - unforgeable authority tokens (Code, StampedCode, CanonicalBytes, BulkSpendAuthority, Minted,
    RelationId), proven by compile-fail absence proofs
  - the value plane's execution-currency FOUNDATION (ExecRows / join_project / ExecDedup) built and
    proven — NOT wired into the RA engine (that is #120)

allowed-red:
  - the RA hot-loop migration onto ExecRows/ExecDedup (bench recovery) — owned by #120, a foundation
    #119 defines and proves

forbidden-shortcuts:
  - weakened tests, copied goldens, compatibility shims, unrun benchmarks, false doc claims,
    red guard scripts, raw-authority leaks, a second value serialization authority

required-gates:
  - full suite green (lib + integration), both feature configs
  - clippy own-code -D warnings clean (both configs), fmt clean
  - compile-fail absence proofs (mutation-verified)
  - benchmark vs baseline published with root cause + #120 recovery ledger
  - #![forbid(unsafe_code)] holds; scripts/check-unsafe.sh green
