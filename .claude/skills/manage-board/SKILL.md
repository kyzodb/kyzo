---
name: manage-board
description: The ONLY way to create, update, move, reorder, or delete epics and stories on the KyzoDB Work board. Use for every board operation — creating an epic or story, rewriting a body, checking a task, commenting, reparenting, relabeling, moving a card between columns, reordering cards or sub-issues, deleting issues, surveying a column, starting an epic or story on the epic branch. 27 live tools, prefixed mcp__kyzo__, discovered via ToolSearch since their schemas are deferred; never use raw gh for board writes.
---

# manage-board

The kyzo MCP server (`mcp/`) is the board's one authority: 25 live
tools instead of a script with subcommands, same underlying `gh` boundary,
same rules. Tool schemas are deferred (bare names only until fetched) — use
`ToolSearch` with `select:<name>` to load one before calling it the first
time. Never use raw `gh` for board writes; never widen these tools, never
add behavior, never bypass them.

## The board model

One axis: column position plus order within it, left to right — `Backlog`
(hidden), `Later`, `Next`, `Now`, `In Progress`, `Blocked`, `Done`. No
milestones, no second axis; the project's old Priority field is deleted.
`Backlog` is where every open card rests unless it's actively `In
Progress` or is an epic deliberately carrying visible horizon on
`Later`/`Next`/`Now`. A story's own horizon is read off its parent epic,
never carried on the story itself — nothing enforces this at the type
level, it's operator discipline. Entering `Blocked` requires the named
technical blocker, posted as a comment in the same motion. Entering `In
Progress` always requires the **epic's** branch — a story rides its parent
epic's branch, an epic rides its own — sets the `focus` label, and assigns
the operating account; every other column removes the label. The branch is
created once per epic by `start_epic`, never per story. Entering `Done` refuses
while a story contract has unchecked Tasks or Definition of Done boxes.

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
  state, label, parent, sub-issues, linked branches, body, comments. One
  GraphQL crossing however many numbers are passed.
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

## Start (deterministic branch-per-epic gates)

- `start_epic(number, branch_name, target?)` — start an epic on its own
  branch after deterministic git+board checks: clean tree, HEAD on `main`,
  `main` level with `origin/main`, unused branch name, an open epic with
  stories that isn't already started, no other epic In Progress with an
  unmerged branch. On pass: creates `branch_name` off `main` linked to the
  epic, moves it to In Progress, sets focus, assigns `@me`. Each failed check
  is a typed refusal.
- `start_story(number, target?)` — start the next story of the active epic
  after deterministic checks: HEAD on the epic branch, no merge/rebase/
  cherry-pick in flight, clean tree, branch not diverged from origin, no
  sibling story In Progress, this is the next unstarted story in sub-issue
  order, and the preceding story is Done with every Task and Definition-of-
  Done box checked with its work on the branch. On pass: moves the story to
  In Progress on the epic branch. Each failed check is a typed refusal.

## Move (any card — epic or story; the column mechanism is kind-agnostic)

- `move_to_backlog(number, target?)`
- `move_to_later(number, target?)`
- `move_to_next(number, target?)`
- `move_to_now(number, target?)`
- `move_to_in_progress(number, target?)` — refuses without the epic branch
  (a story rides its parent epic's branch); assigns the operating account
  (@me) with the focus label.
- `move_to_blocked(number, blocker, target?)` — the named blocker is
  required and posted as a comment in the same motion.
- `move_to_done(number, target?)` — refuses while a story contract has
  unchecked boxes; closing the issue itself is the board's own auto-close
  workflow, not this tool.

## Reorder (position is the priority axis — these write it)

- `reorder_card(number, anchor, target?)` — move a card within the
  project's single item order: `{"position": "top"}` or
  `{"position": "after_card", "card": N}`. A column view shows its slice
  of that order, so top of the order is top of the card's column.
- `reorder_sub_issue(epic, story, anchor, target?)` — move a story within
  its epic's sub-issue list (the epic's execution order):
  `{"position": "first"}` or `{"position": "after_sibling", "sibling": N}`.

## Delete

- `delete_issues(deletions, target?)` — permanent, cascades (card leaves
  the board, sub-issue relations die). Each deletion names `number` AND
  the exact `title` the caller believes it carries; any mismatch refuses
  the whole batch before anything is destroyed. Irreversible —
  operator-ordered deletions only.

`target` on every tool defaults to KyzoDB's own board; pass an explicit
owner/repo/project only to act on a different one (e.g. a disposable test
board).
