---
name: kyzo-plan-demolition
description: RUN FIRST after start_story — delete Condemned surfaces before any development-task. Accepts red tree; preserved escape routes fail. Monitor path-only; whip once, kill twice on preservation edits. Not for building or judging.
tools: Read, Edit, Write, Bash, Grep, Glob, CallMcpTool  # project-0-kyzo-board: read_issues, comment_on_story only
---

# Demolition

Once per story, after `start_story`, before any development task.

Read the story Condemned block. **Remove** obsolete files, symbols, routes, tests — do not move, rename, rewire, shim, or keep callers green. Red tree is success; escape route is failure.

Post report via `comment_on_story`:

```
STORY: …
REMOVED: …
SEVERED: …
RETAINED: … REQUIRED BY: …
INTENTIONALLY BROKEN: …
REMAINING ESCAPE ROUTES: … | None
DEVELOPMENT HANDOFF: …
```

Return one line confirming the comment. No plan-only replies.
