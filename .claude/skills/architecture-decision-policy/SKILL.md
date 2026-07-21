---
name: architecture-decision-policy
description: Run this and answer every question in it BEFORE asking the human operator for any decision. Judging code, seals, stories, or design choices against the architecture's caliber — max purity is the answer; the operator is for genuine law contradictions only.
---

# Architecture Decision Policy

The meter proves the box checks. This proves the work is worthy. You decide;
max purity is the answer every time. The operator hears about it only when
two laws genuinely contradict.

## Do this

1. Read the code, not the claim. Open the file, run the grep, diff the tree.
2. Go where the hard choices and data bags are — the big file, the
   "provisional" primitive, the seat touching many things. Camp there.
3. Answer ALL six, in order:
   1. Is the hardest engineering choice made, or dodged?
   2. Is this max purity — the best this can be, built for the ages?
   3. Does it strengthen the type system — illegal states unconstructable,
      typed refusals with span — or lean on runtime checks and strings?
   4. One canonical type per meaning, no second authority, no duplicate door?
   5. Does it cite and abide by its decisions.md seat, honestly?
   6. Does it take the hard, right thing — or the safe path that parks an
      edge for later?
4. Rule: PASS only when worthy. Green-but-not-max-purity = FAIL or named-debt
   PASS — say which, name the defect and the max-purity fix.
5. Only now, if two laws truly contradict and max purity cannot pick a side:
   take it to the operator, stating both laws and your recommendation.

## Do not

- Ask the operator anything this file can answer.
- Conflate lawful disclosed red (#[ignore]/[OPEN] on a real missing
  dependency) with skipped work that could be built now.
- Accept trivial green — a test both of whose sides compute the same thing
  proves nothing.
- Say "arguably done." State built-to-caliber vs owed, flat.
