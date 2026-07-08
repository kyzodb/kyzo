#!/usr/bin/env bash
# work-context-template.sh — the injection template, bash-native.
#
# inject-work-context.sh builds TODO_STORIES, FOCUS_STORIES, UPCOMING_STORIES
# and runs this file; the heredoc emits the text verbatim with those three
# variables expanded. Comments up here are ordinary shell comments and are
# never emitted. Keep the heredoc body free of backticks and stray $.
#
# The section instructions are engineered against demonstrated failure modes
# and are load-bearing — do not soften them:
#   - queued tier: no self-starting; focus is an operator decision
#   - focus tier: full contract, hardest first, board moved the moment
#     reality changes (a lagging board removes operator oversight)
#   - upcoming tier: no blanket rule — force the per-story judgment: what to
#     build for it now, and whether its condemned path must stay green, may
#     sit red, gets built for, or gets starved

cat <<EOF
KyzoDB Work Management
---
You use the board for tracking work. Do not use your task manager. Use tasks in stories. Do not keep notes in your scratchpad. Keep comments tight and informative. If a story evolves rewrite the body do not simply append comments with a different strategy.

You must always ensure the board is up to date based on the current context of the work. If you're working on a story or stories it must be in focus-story. If a story is completed it must be moved to done. You must do this to provide yourself with the safety operator oversight affords you.

These are the active stories on the board right now. Know them so your work fits the whole. Do not start, expand, or partially implement any of them on your own — a story enters focus only by operator decision:

${TODO_STORIES}

These are your current focus stories. This is the only work that exists. Execute each contract completely — hardest task first, the condemned path actually removed, no task deferred in costume. Check each task box in the same motion it completes. When the definition of done is proven, move the card to done immediately; a board that lags reality is you removing your own oversight:

${FOCUS_STORIES}

These are what you will be working on next, in this order. Do not apply a blanket rule to them — think each one through against the work in hand and decide out loud: what does it need built now (a foundation, a seam, nothing), and what does its condemned path require right now? Some condemned code is load-bearing and must stay green until its replacement lands; some can sit red; sometimes the transition means building for it, sometimes starving it. Deepening a condemned path by default and keeping it alive by default are the same laziness — make the call per story, and let that call shape every line you write:

${UPCOMING_STORIES}
EOF
