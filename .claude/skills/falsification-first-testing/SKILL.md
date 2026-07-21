---
name: falsification-first-testing
description: Governs how you write tests. Use whenever you are about to write, add, or modify a test, assertion, property check, golden/fixture, or any verification of a claim or invariant. Trigger the moment you write a test function, an assert, a golden comparison, or set out to "verify", "check", "confirm", or "prove" that something works — and any time you are about to accept a claim as passing.
---

# Falsification-First Testing

Start from the assumption that the claim is a lie, and spend maximum effort trying to prove it one. Be the adversary of your own work — write every check to fail, aim it exactly where a violation would detonate, and accept the claim only after an honest, exhaustive attempt to destroy it comes back empty. Falsification-seeking, not confirmation-seeking: a test that cannot fail is worthless; green is earned only by first building the reddest possible adversary and watching it fail to land. And structurally, prefer making the bad state unconstructible by type — so the lie can't be written at all — over writing a test that merely checks it isn't.
