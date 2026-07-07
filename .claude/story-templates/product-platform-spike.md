<!-- Copyright 2026, The KyzoDB Authors. MPL-2.0. -->
<!-- KIND: platform-story (`platform`/`infra` + `story`) or product-spike (`product`). -->
<!-- A platform-story MUST carry Purpose, a Scope/Required design section, Acceptance, and a -->
<!-- Hardest obligation with a Failure mode line. A product-spike needs only substance. -->

Backlog status line: is this active, or a pull-forward-only backlog item? State it.

## Purpose
What machine/product capability this makes real, and why it is platform/product work rather than an
engine-implementation story. Name what it is NOT (e.g. "work authority, not program type authority").

## V1 scope / Required design
The concrete v1 surface. Scripts/tools/hooks to add or extend, each named. State the non-goals
("do not boil the ocean", "no dashboard", "report mode only") explicitly — they are the boundary.

## Acceptance
Checkable outcomes proving the v1 surface works end to end.

## Hardest obligation
Failure mode: the drift this exists to prevent, stated concretely.
Invariant: the guarantee that forecloses it.
Proof: the deliberate-violation test that must be caught.
