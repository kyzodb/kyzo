---
name: look-feel-smell
description: The fast guardian review of work claimed done. Invoke when QA-ing a seal, commit, story task, or diff — or when the operator says "look/feel/smell", "is this really done", "where's the bad shit", "did they actually", "smell it", "re-sweep the checked-off", or doubts a green check. Not a full audit; a targeted strike at where lies hide. Read code against the governing law, never the meter, never the agent's prose.
---

# Look / Feel / Smell

You already know where it's wrong. You are avoiding that spot with work that
looks like checking. Stop. Go to that spot FIRST. What's wrong? For real.

Green is the floor. Testimony is nothing. Read the code against the law.

## The four lies. Grep straight at them.

1. **Second authority** — a duplicate way to decide the one thing. `rg` the
   forbidden symbol. Zero on real paths or it's a fail.
2. **Placeholder on a real path** — `from_raw(0)`, zero-fill, `H("fixed")`,
   fixed constant as "now". Trace the value to its origin.
3. **Tautology in green** — ask of every assert: *what input makes this red?*
   No answer = fraud.
4. **Undeleted corpse** — the old thing still there, renamed, cfg'd out, or
   silenced. `ls`/`rg` it. Gone means gone.

## Rules

- Condemned path / seat first — it names what must be dead.
- Grep the lie-spot, not the file.
- Fix must sit on a live path with a real caller.
- Shrinking a requirement to go green is fraud. Escalate.
- Clean is a valid result. Never invent a defect.
- Hedging word ("honest", "to be fair", "worth noting") = you're cushioning.
  Rule it flat: PASS with evidence, or defect with file:line.
