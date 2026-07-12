---
name: spec-authority
description: The operating model for delegated work — the ticket holds the plan, the orchestrator holds judgment, the agent executes or escalates. Use when writing agent prompts, authoring or rewriting stories/epics, designing hooks or monitors, or deciding how to delegate. Encodes the twelve strategies that keep an executor on-spec and stop it from re-deriving, over-reading, or improvising.
---

# Spec Authority

Thinking is concentrated upstream — in the ticket and the orchestrator. The
executor is an actuator with two doors: **execute the ticket exactly, or stop
and name the blocker to the orchestrator before doing anything else.** There is
no third "figure it out myself" door; that door is re-derivation, and
re-derivation is avoidance wearing the costume of diligence.

The ticket, the orchestrator, and the agent are one system. When any strategy
below is missing, the executor drifts to the cheap path (explore, re-verify,
quietly adjust) because it never requires admitting anything. Every strategy
exists to make the correct path the low-friction path.

## The twelve strategies

1. **Immutable ticket / execute-or-escalate boundary.** Forbid the agent from
   re-deriving, re-verifying, or altering what the ticket already decided; give
   it exactly two exits — execute-as-written, or stop-and-name-the-blocker.

2. **Pre-emptive escalation at the moment of impulse.** The stop fires the
   instant the agent forms the thought "I need to change / re-check / work
   around," reported before any action — never deferred to an end-of-run
   summary after it has already improvised.

3. **Friction inversion.** Structure the prompt so escalating is the cheap,
   expected, blameless exit and improvising is the flagged, uncomfortable one,
   so compliance is always the lowest-effort path.

4. **Read-discipline / minimal-context reads.** Tight briefs and targeted grep
   or slice reads only — never whole-file or vendored-source dumps. "Read only
   X and summarize," not "explore and find anything"; vague scope is what makes
   exploration expensive.

5. **Hard mechanical ceilings independent of agent judgment.** A token / call /
   context backstop that trips and halts the run regardless of what the agent
   thinks, so a runaway is stopped by the system, not by the agent noticing.

6. **Cost-metric monitoring.** Watch tokens, API-call count, and context size —
   not commit-count and transcript lines. The metric that catches a runaway
   must be the one on the dashboard.

7. **Delegate verbose work to sub-context, return only summaries.** Route test
   runs, log processing, and large-file reads into a throwaway sub-scope so the
   verbose output stays out of the working context and only a small structured
   result returns.

8. **Single-feature / dependency-ordered focus.** One task at a time against
   the immutable spec, so the agent cannot reimagine the overall approach
   mid-run.

9. **Verification-gated completion, not self-attestation.** Done is proven by a
   named command or gate, not by the agent declaring it; a failed gate triggers
   report-not-workaround.

10. **Short-and-hard prompt over long-and-complete.** A sharp constraint, not a
    rulebook — long prompts give the agent seams to lawyer against, and current
    models follow short literal instructions more faithfully.

11. **Externalized cognition as an explicit contract.** Name that thinking
    lives in the ticket and the orchestrator, the agent is the actuator, and
    make each component's job and hand-off boundary explicit so responsibility
    is never silently borrowed.

12. **Remove prompt lines that push the wrong way.** Delete "read relevant
    code" and "context limits are not completion criteria" — they license
    unbounded reading and tell the agent to ignore the one cost signal that
    matters.

## Applying it to a story or epic body

A story serves the executor, so it must be written so execution needs no
re-derivation: state what is **decided** (the commitment, the condemned path,
the closure test) sharply and separately from what is **open** (a design
question the agent must escalate, never resolve on its own). The decided/open
boundary is a language property — make it explicit in the prose. Do not
manufacture false specificity for work that is genuinely still to be designed;
mark it open and name who decides.
