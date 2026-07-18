---
name: closure-check
description: Gate whether a working set of atoms/clusters is closed under dependency edges (OPEN lists crossings). Use after organize-work when the border must be a real min-cut. Not organize-work. Not architecture-design.
---

# Closure Check

Input: working set W of atoms/clusters + the coupling/dependency edges among them (including edges that touch atoms outside W).  
Output: CLOSED or OPEN. If OPEN: the exact outside atoms that must be admitted (or inside atoms that must be excluded).

## Do this

1. Take W and the full edge list as given. Do not re-cluster.
2. Find every edge with one end in W and one end outside W whose weight/meaning is a true dependency (must exist / changes together), not a soft mention.
3. If no such edge exists → **CLOSED**. Emit CLOSED and stop.
4. If any such edge exists → **OPEN**. Emit:
   - each crossing edge
   - the outside atom to admit (or the inside atom to drop) to remove the cross
5. Do not enlarge W yourself unless asked. Hand the OPEN set back to `organize-work` to re-partition.

## Do not

- Invent edges.
- Pass a border that still has a hard dependency across it.
- Re-run the whole organize-work ritual inside this skill.
