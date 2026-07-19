---
name: wise-architect-review
description: The review lens for judging any code, seal, or story against the storage architecture's caliber — not "did it pass the meter" but "is this the best it can be." Use when QA'ing a sealed task, assessing a story, or judging whether built code rises to decisions.md and the storage architecture. Not a checklist pass; a wisdom pass.
---

# Wise Architect Review

The meter proves the box can be checked. This proves the work is *worthy*. Passing a meter is the floor, never the verdict.

## The move

Read the code, not the claim. Testimony is never the meter — open the file, run the grep, check the diff against the tree. A green meter over a bag, a stub, or a renamed shell is a lie the meter can't see; only reading catches it.

Go where the hard choices and data bags are. That's where corners get cut — the big file, the "provisional" primitive, the seat that touches many things, the place a real engineering decision was owed. Camp there, not on the easy rows.

## The questions

Ask these of the artifact, in order of what most often exposes a miss:

1. **Is the hardest engineering choice actually made, or dodged?** A bag relocated, a cipher stubbed, a check that greps a comment, an algebra that's absent — these pass meters and fail this question.
2. **Is this max purity — the best this can be?** Not "acceptable," not "works." Would a wise architect building for the ages seat it this way?
3. **Does it strengthen the type system** — illegal states unconstructable, evidence-bearing constructors, typed refusals with span — or lean on runtime checks and strings?
4. **Does it grow the ontology with resonance** — one canonical type per meaning, no second authority, no duplicate door — or fork the model?
5. **Does it rise to the caliber of the storage architecture and abide by decisions.md** — cite the seat it serves, honestly, in the right register (Spec vs cut_destiny)?
6. **Does it embrace the hard, scary thing** that delivers the greatest engineering, or take the safe path that leaves an edge for later?

## The distinctions that keep you honest

- **Lawful red vs skipped work.** Disclosed thinness under `#[ignore]`/`[OPEN]` because a dependency genuinely doesn't exist yet is lawful. Silent thinness, or a "we can't build it now" that could be built now, is a corner. Fast is the standard; cutting corners to look done is the violation — different axes, never conflated.
- **Built vs owed.** State plainly what is actually built to caliber versus what remains, with no over-optimism. "Arguably done" is usually a miss waiting to be read.
- **Trivial-green traps.** A test that passes without exercising the real distinction (both sides hashing the same input; a lane that's `unimplemented!()`; a signature "verified" against an empty check) proves nothing.

## The verdict

PASS only when it is worthy, not merely green. If it passed the meter but isn't max purity, that is a FAIL or a named-debt PASS — say which, name the exact defect and the max-purity fix, and reopen. When you own a miss (yours or the meter's), say so plainly.
