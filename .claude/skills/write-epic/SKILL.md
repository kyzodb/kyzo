---
name: write-epic
description: create, review, or revise kyzodb github epics used as epics. use when naming epic groups, writing epic outcome descriptions, or checking whether grouped stories share a real engineering/value transition. enforce epic language that describes the state of value change created by the story group instead of release theater, phase names, slogans, or generic completion language.
---

# Write Epic

## Instruction Block

A KyzoDB epic is an epic: a group of stories that together create a state of value change.

Do not write epics as release slogans, phases, status buckets, or heroic quality claims.

A valid epic explains why the grouped stories belong together and what engineering/value condition changes when they are complete.

The epic does not use story format. It does not have tasks. It does not have a Definition of Done. The stories carry execution. The epic carries grouping meaning.

Write the outcome as a transition: what KyzoDB is moving from, what it is moving to, and what shared technical boundary, authority, capability, proof, or failure class the grouped stories cross.

## Epic Schema

Use this exact markdown order.

```md
# <Epic Name>

## Outcome Description

<One paragraph describing the transition this group of stories creates. State what KyzoDB is moving from, what it is moving to, and what shared technical boundary, authority, capability, proof, or failure class makes these stories belong together.>
```

## GitHub and the board

- An epic is a GitHub issue; its stories are attached as **sub-issues** of it.
- Sub-issue list order is the execution order of the epic's stories.
- The epic carries exactly one of the five labels — `Feature`, `Bug`,
  `Performance`, `Security`, or `Demo`, matching the dominant character of its
  stories — as the GitHub label itself, never restated in the body (a body
  copy is a stored twin that goes stale on reclassification).
- **The epic is the one carrier of horizon, and it carries it as column
  position** (`Now` / `Next` / `Later`, moved via the manage-board tools) —
  never as a milestone or a body field. Milestones do not exist on this board.
  Its stories read their horizon through the parent relation and never carry
  their own.

## Field Rules

| Field                 | Rule                                                                                                                                                                                          |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Epic Name`      | Name the value boundary being crossed by the group of stories, in Title Case. Do not name a mood, phase, slogan, release ceremony, or generic work category.                                 |
| `Outcome Description` | Describe the aggregate state of value change created by the grouped stories. It must explain why the stories belong together and what engineering/value condition changes when they are done. |

## Invalid Epic Conditions

An epic is invalid when any of these are true:

* the name is a slogan, phase, or mood
* the outcome only says work will be completed
* the outcome does not explain why the stories belong together
* the outcome does not describe a transition from one engineering/value condition to another
* the epic uses story format
* the epic includes tasks or Definition of Done
* the body restates the label, or carries a milestone or horizon field — those
  live on the GitHub label and the board column, nowhere else
* the language performs quality instead of naming the shared boundary, authority, capability, proof, or failure class

