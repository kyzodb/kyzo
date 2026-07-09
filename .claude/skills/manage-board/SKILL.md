---
name: manage-board
description: The ONLY way to create, update, move, or delete epics and stories on the KyzoDB Work board. Use for every board operation — creating an epic or story, rewriting a body, checking a task, commenting, reparenting, relabeling, moving a card between columns or milestones, deleting issues. One tool, manage-board.py in this directory, seven subcommands (including the read-issue reader); never use raw gh for board writes.
---

# manage-board

One tool: `manage-board.py`. Each subcommand does only its named operation.
Every flag is a schema field — the command line is the story/epic contract as
a form. A missing flag is a usage error; a non-conforming issue body refuses
construction and the refusal is the report (rewrite the story to the contract
before operating on it). Never widen these commands, never add behavior,
never bypass them with raw gh.

## create-epic

```
manage-board.py create-epic
  --name STR              epic name: the value boundary crossed
  --label Feature|Bug|Performance|Security|Demo
  --outcome STR           the Outcome Description paragraph
  --milestone 1-Now|2-Next|3-Later
  --column todo|focus|done
```

Creates the epic issue with its label, sets the milestone, places its card in
the column. Stories attach themselves at their own creation.

## create-story

```
manage-board.py create-story
  --name STR              story name: domain + value-bearing mechanism
  --epic N                parent epic issue number (omit ONLY by operator decision)
  --column todo|focus|done
  --label Feature|Bug|Performance|Security|Demo
  --milestone 1-Now|2-Next|3-Later
  --actor STR             ## Description: As a <actor>,
  --want STR              I want <want>,
  --so-that STR           so that <so-that>.
  --condemned-path STR    ## Condemned: Path
  --condemned-reason STR  Reason
  --closure-test STR      Closure test
  --choice STR            ## Engineering Choice: Choice
  --choice-type STR       Representation | Authority Boundary | Execution Currency |
                          Cache Invalidation | Storage Contract | Ordering Invariant |
                          Admission Path | Evaluator Rule | Algorithm | Benchmark |
                          Failure Path | Evidence Boundary
  --consequence STR       Consequence
  --evidence-needed STR   Evidence needed (or "None")
  --context STR           ## Context
  --task STR              one ## Tasks line; repeat in order
  --dod STR               one ## Definition of Done line; repeat in order
```

Creates the story issue with its label and milestone, attaches it as a
sub-issue of the epic, places its card in the column.

## update-epic

```
manage-board.py update-epic N  (exactly one of:)
  --outcome STR           replace the body; requires --label (body and label
                          chip agree by construction)
  --comment STR           append one comment
```

## update-story

```
manage-board.py update-story N  (exactly one of:)
  --check "task text"     flip the one task matching the text to [x]
  --uncheck "task text"   flip it back to [ ]
  --comment STR           append one comment
  --epic M                reparent to a different epic
  --label-to L            reclassify (one of the five)
  --replace-body          rewrite the whole contract; takes the create-story
                          schema field flags (--actor … --dod)
```

A `--check`/`--uncheck` text must match exactly one task. Updates never
create issues or cards.

## move-issue

```
manage-board.py move-issue N  (at least one of:)
  --column todo|focus|done
  --milestone 1-Now|2-Next|3-Later
```

Moves the card and/or reassigns the milestone. `--column focus` sets the
In Progress column AND adds the "focus" state label — and REFUSES unless the
story has a linked branch (`gh issue develop N --name <branch> --base main`);
`--column todo|done` removes the focus label. Closing on done is the board's own auto-close
workflow, not this tool. Never creates a card — an issue with no card is
drift you must surface, not repair silently.

## read-issue

```
manage-board.py read-issue N [N ...]
```

The fast typed reader: one compact rendering per issue — title, state,
label, milestone, parent, sub-issues, body, comments. Use this to study
issues instead of hand-rolled `gh issue view --json` incantations.

## delete-issue

```
manage-board.py delete-issue N [N ...]
```

Permanently deletes each issue. GitHub's delete cascades: the card leaves
the board and the sub-issue relation dies with the issue. Irreversible —
operator-ordered deletions only.
