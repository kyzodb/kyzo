---
name: bug-hunt-for-money
description: Rank where to attack KyzoDB for maximum-severity bugs — the one law (binary order = semantic order), forged records/certs, bitemporal history rewrites, crypto-shred reversal, the federation gate, decoder panics on wire/disk bytes — and hunt green campaigns that never drove their adversary. Use when hunting bugs or vulnerabilities, prioritizing a security review, or deciding which invariant to break first. Not QA-ing one diff claimed done (look-feel-smell); not verifying a single claim (fraud-hunter).
---

How I'd hunt this for max payout

Bug money hunter rule: don't spray. Target the invariants whose failure is catastrophic, because that's where the money is. A DB's severity ladder isn't "XSS/CSRF" — it's silent wrong answers and broken accountability. Rank by blast radius:

1. Break the one law — binary order ≠ semantic order (crown jewel, top bounty).
The entire product is "one substrate serves every query model because encoded byte-order equals semantic order." One counterexample = silent wrong answers across relational/graph/vector/range — undetectable in prod. I'd write a single property test: for all a,b: semantic_cmp(a,b) == bytes_cmp(encode(a), encode(b)) and fuzz it against the ugly cases where naive encoders die:
- floats: -0.0 vs +0.0, NaN, subnormals, -inf
- negatives (sign-flip / two's-complement ordering)
- variable-length strings/bytes that aren't prefix-free (does "ab" sort against "a\0…" correctly?)
- composite keys where a short field lets the next field bleed across a boundary
- cross-type ordering and where NULL sorts
This is machine-findable and the highest payout per hour. I'd start here.

2. Forge a KyzoRecord / an AdmissionCertificate (accountability-kill).
If I can mint a record without the one admission door, or forge a cert that verifies, the whole "cross-examinable" promise is fraud. Attack surface I'd go straight at: the crypto (ed25519 usage, not the primitive) — nonce reuse across CryptoDomain×IncarnationId, a MAC accepted where a signature belongs, id == secret degeneracies, provisional/zero signatures on a real path. And the forge wall itself — is there a from_bytes/decode path that reconstructs a record? (I literally just caught this wall shipping a fake proof — trybuild green for the wrong reason. A hunter who reads that test sees the invariant was never actually proven and goes looking for the real hole.)

3. Make as-of lie / supersession overwrite history.
Time-travel audit is a headline feature. If a "correction" mutates the prior committed bytes, or as-of(pre-correction) replays the new value, the audit trail is falsifiable — regulatory-catastrophe severity. I'd hammer the bitemporal key: two writes colliding on (valid, sys), a client-supplied timestamp that reorders local commits, a correction that reuses the prior key.

4. Reverse a crypto-shred (right-to-be-forgotten kill).
Recover "shredded" plaintext — including embeddings, backup-proof. If the wrapped-salt or a DEK survives anywhere (a log, a manifest, an SST that wasn't rewrapped), the erasure promise is dead. High-compliance-value bounty.

5. Federation seam.
- Mint a CrossingValidated without passing the 9-step gate (it's guarded by a private _priv:() — so I'd look for any pub constructor, a Default, a serde derive, or a test-only builder that leaked to a real path).
- Get a graph-bound key to authorize cross-graph.
- Promote with divergent meaning that still replay_equals — i.e., find a field the digest doesn't cover (this is why I flag un-driven negative tests: they mark exactly the seat nobody verified).

6. Panic / DoS on any byte sequence from wire or disk.
zone-store law: "any input from disk decodes to typed refusal — no panic on any byte." zone-session: "no unwrap/expect on any caller-reachable path." So I'd fuzz every decoder — WAL replay, SST, dump/restore, foreign import, the crossing envelope — with crafted/oversized/duplicate-key/unknown-version bytes. Every unwrap I can reach from untrusted input is a bounty; a panic in WAL replay is a durability/DoS bounty.

The meta-move that prints money here

Hunt the test-theater, not just the code. This codebase's credibility rests on "campaign-proven" invariants. The highest-leverage play for a broke hacker is to find a green campaign that never drove its adversary — then exhibit the real violation it was supposed to catch. You get paid twice: the bug, and the demonstration that a "proven" guarantee was hollow. The map is the four lie-shapes:
- tautology in green (assert on a value built to pass; the promotion test that only checks before == after) → the invariant is unverified → go break it.
- placeholder on a real path (debug_assert that's compiled out in release — I flagged exactly one here; a hunter runs the release binary and walks through the door that only debug guarded).
- compile-only meters — several board Checks here are cargo check, not tests. Anything that compiles but misbehaves at runtime sails through. I'd focus on behavioral bugs in exactly those slices.
- second authority (a duplicate encode/order path beside the one law) → make the two disagree.

Method, resource-constrained: read decisions.md — every seat is a claimed invariant, i.e., a labeled bounty target. Attack the seat, diff its campaign for the un-driven case, fuzz the one law and the decoders. That's the whole efficient playbook.

