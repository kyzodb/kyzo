---
name: manage-board
description: The ONLY way to create, update, move, or delete epics and stories on the KyzoDB Work board. Use for every board operation — creating an epic or story, rewriting a body, checking a task, commenting, reparenting, relabeling, moving a card between columns, deleting issues, surveying a column. 23 live tools, prefixed mcp__kyzo__, discovered via ToolSearch since their schemas are deferred; never use raw gh for board writes.
---

# manage-board

The kyzo MCP server (`mcp/`) is the board's one authority: 23 live
tools instead of a script with subcommands, same underlying `gh` boundary,
same rules. Tool schemas are deferred (bare names only until fetched) — use
`ToolSearch` with `select:<name>` to load one before calling it the first
time. Never use raw `gh` for board writes; never widen these tools, never
add behavior, never bypass them.

## The board model

One axis: column position, left to right — `Backlog` (hidden), `Later`,
`Next`, `Now`, `In Progress`, `Blocked`, `Done`. No milestones, no second
axis. `Backlog` is where every open card rests unless it's actively `In
Progress` or is an epic deliberately carrying visible horizon on
`Later`/`Next`/`Now`. A story's own horizon is read off its parent epic,
never carried on the story itself — nothing enforces this at the type
level, it's operator discipline. `Blocked` exists in the schema with zero
wired enforcement — no forcing function yet, use it honestly anyway.
Entering `In Progress` always requires a linked branch (`gh issue develop N
--name <branch> --base main`) and sets the `focus` label; every other
column removes it.

## Create

- `create_epic(name, body, column, target?)` — new epic issue: label +
  outcome-description body, card placed in `column` (normally `Later`,
  `Next`, or `Now`).
- `create_story(name, parent, column, body, target?)` — new story issue
  with its full contract (description, condemned path, engineering choice,
  tasks, definition of done), attached to `parent` unless omission is a
  declared operator decision, card placed in `column` (normally
  `Backlog`).

## Read

- `read_issues(numbers, target?)` — compact rendering per issue: title,
  state, label, parent, sub-issues, body, comments.
- `read_epic(number, target?)` — an epic plus every one of its sub-issue
  stories, one report.
- `list_board(column, label?, focus_only?, target?)` — every card
  currently in one column, optionally filtered to a classification label
  or focus-labeled cards only. The one tool that finds issue numbers
  without already knowing them.
- `story_progress(number, target?)` — a story's Tasks / Definition of Done
  completion as checked/total counts.

## Update

- `replace_epic_outcome(number, body, target?)` — rewrite an epic's
  Outcome Description + label together (they agree by construction).
- `comment_on_epic(number, comment, target?)` / `comment_on_story(number,
  comment, target?)` — append one comment.
- `check_story_task(number, task_text, target?)` /
  `uncheck_story_task(number, task_text, target?)` — flip the one task
  matching the text. Refuses on zero or multiple matches.
- `reparent_story(number, epic, target?)` — detach from the current
  parent (if any), attach to a new one.
- `reclassify_story(number, label, target?)` — change a story's label.
- `replace_story_body(number, body, target?)` — rewrite a story's whole
  contract (label + every body section). Use when the shape itself
  changed, not for a single-field edit.
- `rename_story(number, name, target?)` — rename a story's title only.

## Move (any card — epic or story; the column mechanism is kind-agnostic)

- `move_to_backlog(number, target?)`
- `move_to_later(number, target?)`
- `move_to_next(number, target?)`
- `move_to_now(number, target?)`
- `move_to_in_progress(number, target?)` — refuses without a linked
  branch.
- `move_to_blocked(number, target?)`
- `move_to_done(number, target?)` — closing the issue itself is the
  board's own auto-close workflow, not this tool.

## Delete

- `delete_issues(numbers, target?)` — permanent, cascades (card leaves the
  board, sub-issue relations die). Irreversible — operator-ordered
  deletions only.

`target` on every tool defaults to KyzoDB's own board; pass an explicit
owner/repo/project only to act on a different one (e.g. a disposable test
board).
