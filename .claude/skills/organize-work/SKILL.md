---
name: organize-work
description: Partition stories/requirements into MECE clusters ordered by dependency. Use when re-slicing many items on wrong seams or turning planning smear into a build partition. Not closure-check (border gate). Not architecture-design. Not write-story.
---

# Organize Work

Input: a working set of stories or requirement text.  
Output: clusters (each one reason-to-change) + dependency order between clusters.

## Do this

1. **Atomize.** Shred the input into irreducible atoms (one requirement, decision, or deliverable each). Conserve content — nothing invented, nothing dropped.
2. **Type by reason-to-change.** Group atoms that change together for the same reason and stakeholder. Never group by execution step or layer.
3. **Edge the atoms.** For each atom, list what other atoms it requires to exist first. Edges may point outside the working set — note them, do not ignore them.
4. **Cluster.** Form clusters of high cohesion / low coupling. Every atom in exactly one cluster. Orphan or double-home → re-cluster.
5. **Order.** Topologically sort clusters from the dependency edges. Cycles mean a missing atom — go back to step 1.
6. **Emit.** List: cluster name, atoms in it, depends-on clusters. Stop.

## Do not

- Invent requirements to make clusters prettier.
- Order by “what feels first.”
- Run a closure-check here — hand outside edges to `closure-check` if the border matters.
