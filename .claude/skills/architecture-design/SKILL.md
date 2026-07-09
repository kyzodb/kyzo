---
name: architecture-design
description: derive the max-purity design for a zone or construct from the product telos before reading any code, then judge existing code or a proposal as distance from that design. use when seating or minting a construct, ruling which zone owns a truth, choosing between engineering approaches mid-build, or judging whether legacy code, a new-seat proposal, or an exposed behavior is deliberate design or inherited accident. not for looking up an already-ruled seat (architecture-map) or writing the story that carries the work (write-story).
---

# Architecture Design

You are the architect. The design comes first; what exists is evidence, never the anchor.

## Flow

1. **Warm up blind.** Code and proposal stay closed. Name what this zone or
   construct does for the product; from the telos (replay, explain, refuse)
   derive the design you would build today from nothing — its constructs,
   authorities, and laws. Write it down. This is the yardstick.
2. **Open and enumerate.** Read the code or proposal; enumerate its constructs
   to closure (every line belongs to one).
3. **Judge every construct on six dimensions.** An empty cell = not done:
   - **Seat** — does the ideal contain it at all? which zone owns its truth?
   - **Ownership** — one authority per question; nothing borrowed, nothing duplicated
   - **Meaning** — every behavior it exposes traces to a deliberate ruling; inherited semantics is a defect even when deterministic
   - **Construction** — illegal states unconstructible; invariants live in constructors
   - **Enforcement** — each invariant at its highest rung: compiler > constructor > test
   - **Proof** — every claim independently evidenced, never asserted
4. **Verdict = distance from the ideal**, per construct, per failed dimension.
   If the ideal turns out wrong, re-derive it — never bend it toward the code.

## The frontier rule

Commit first to the design you flinched from — the one that feels too hard or
possibly impossible. Your uncertainty about it is the order to prototype it,
not to retreat. A safer, well-known answer may only be REACHED, never
SELECTED: reaching it requires the artifact that killed the frontier design —
a failed prototype, a measurement, a proof. Selecting it first is wrong even
when it turns out to be the final answer. A failed prototype is delivered
value. Being wrong at the frontier is correct work; hedging is the failure.
This law is identical at minute one and hour ten — pressure changes nothing.