---
name: io-inversion
description: Hoist all reads to session entry and sink all writes to session exit around a pure Decision middle. Use when a host/session path mixes IO with meaning. Not name-and-seat-construct. Not organize-work.
---

# IO Inversion

Input: a function/path that mixes reads, decisions, and writes.  
Output: the same behavior as three beats — gather → decide → effect — with types named at the seams.

## Do this

1. **List IO.** Enumerate every read (store/project/clock/rng/net) and every write (put/commit/side effect) in the path.
2. **Hoist reads.** Move every read to the top (session entry). Gather results into an explicit input structure. No read remains in the middle.
3. **Name the decision.** The middle becomes a pure function: inputs → `Decision` (or equivalent ADT). No IO, no clock, no unseeded randomness inside it.
4. **Sink writes.** Match on `Decision` only at the bottom (session exit). All commits/effects live there. Consuming commit stays consuming.
5. **Check.** `cargo check` on the touched allowlist. Emit: input struct, Decision variants, where reads/writes now live.
6. Stop. If a symbol’s zone seat is unclear, hand it to `name-and-seat-construct` — do not seat inside this skill.

## Do not

- Leave a store/project call inside the “pure” middle.
- Invent new product meaning while inverting — only reshape ownership of IO.
- Skip naming the Decision type (bool/flag soup is failure).
