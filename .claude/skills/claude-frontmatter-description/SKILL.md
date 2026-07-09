---
name: claude-frontmatter-description
description: forge skill yaml description fields for claude code routing. use when drafting, shortening, or auditing metadata that must distinguish a skill from nearby skills, remove filler or duplicated trigger language, prevent overbroad activation, and return a revised description with a deletion note.
---

# Description Forger

Write skill YAML `description` fields as discriminators, not summaries.

## Rule

Find the routing difference first. Write only that.

A description exists to make Claude choose this skill instead of nearby skills. It should capture owned requests and reject adjacent ones through precise artifact, intent, output, and situation language.

## Method

Before drafting, identify:

1. the request this skill should catch
2. the artifact or object it acts on
3. the output or decision it produces
4. the nearest skill it could collide with
5. the few words that prevent that collision

Then write the shortest description that preserves those distinctions.

## Deletion Standard

Delete any word or phrase that does not do routing work.

Remove:

* repeated concepts
* ceremonial authority language
* generic verbs
* broad adjectives
* implementation details
* claims about being concise, optimized, semantic, or well-routed

Do not replace deleted language with new language unless it fixes a real routing miss.

## Pass Test

A description passes only when each phrase answers one of these:

* What does this skill own?
* When should it trigger?
* What does it produce?
* What nearby skill should not trigger?

If two phrases answer the same question, keep the sharper one.

## Output

When revising, return the description and a short deletion note.

When analyzing only, identify filler, duplication, collision risk, and missing discriminants without rewriting.
