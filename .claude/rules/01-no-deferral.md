# No Deferral in Costume

"Residual," "follow-up," "later," "out of scope," "next camp," and effort-sizing are words used to
avoid work. Name the concrete technical blocker, or do the work now.

The following phrases are not completion:

- warrants review
- metadata only
- mostly vendored
- honest status
- remaining cleanup
- proper fix is next story
- accepted regression
- test updated for new behavior

## The ledger

A remaining red item may move to a later story ONLY with this ledger:

    Item:
    Current status:
    Why it is not this story:
    Which story owns it:
    What invariant prevents it from becoming deferral:
    What shortcut would be unacceptable:

**Allowed later-story red:**

- old fixture vocabulary deleted by this story
- a future subsystem migration explicitly owned by the next story
- wiring that requires a foundation created (and proven) by this story

**Not allowed to remain red:**

- skipped proof
- weakened assertion
- copied golden
- unrun benchmark
- false doc claim
- red guard script
- compatibility shim
- placeholder semantics
- raw authority leak

If the item cannot pass the ledger — in particular, if it cannot be honestly described as "not this
story" — it belongs to the current story and must be fixed now.

## Anti-deferral, obeyed mechanically

- **Hardest work first.** Doing ripe work before hard work is deferral in costume. Name the hard
  work plainly, then do it — never smuggle avoidance into a recommendation.
- **No deferral without a named technical blocker.**
- **Don't document absence, build the thing.** A comment saying something doesn't exist is not work
  product.
- **When a blocker clears, re-walk its queue the same turn.**
- **No options menus.** Decide by the prime directive and execute. Present choices only when the
  decision is genuinely the maintainer's: public/irreversible acts, or product/semantic rulings.
