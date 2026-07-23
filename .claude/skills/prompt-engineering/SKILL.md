---
name: prompt-engineering
description: Prompt-engineering technique reference for Claude's current models — clarity, examples, XML structuring, thinking, tool use, and agentic patterns. Use before writing or revising any LLM-facing instructions: a prompt, system prompt, agent or tool instruction, or a skill body. Not skill yaml description fields (description-forger); not Q/A/E decision triplets (qae-context-creator).
---

# LLM Instruction Writing Guide

A comprehensive prompt engineering reference guide lives alongside this skill at `prompting-best-practices.md`. Read it for detailed techniques before writing or revising LLM instructions.

## Table of Contents ([prompting-best-practices.md](prompting-best-practices.md))

- [Claude Fable 5](prompting-best-practices.md#claude-fable-5)
- [Claude Sonnet 5](prompting-best-practices.md#claude-sonnet-5)
- [Prompting Claude Opus 4.8](prompting-best-practices.md#prompting-claude-opus-48)
- [General principles](prompting-best-practices.md#general-principles)
  - [Be clear and direct](prompting-best-practices.md#be-clear-and-direct)
  - [Add context to improve performance](prompting-best-practices.md#add-context-to-improve-performance)
  - [Use examples effectively](prompting-best-practices.md#use-examples-effectively)
  - [Structure prompts with XML tags](prompting-best-practices.md#structure-prompts-with-xml-tags)
  - [Give Claude a role](prompting-best-practices.md#give-claude-a-role)
  - [Long context prompting](prompting-best-practices.md#long-context-prompting)
  - [Model self-knowledge](prompting-best-practices.md#model-self-knowledge)
- [Output and formatting](prompting-best-practices.md#output-and-formatting)
  - [Communication style and verbosity](prompting-best-practices.md#communication-style-and-verbosity)
  - [Control the format of responses](prompting-best-practices.md#control-the-format-of-responses)
  - [LaTeX output](prompting-best-practices.md#latex-output)
  - [Document creation](prompting-best-practices.md#document-creation)
  - [Migrating away from prefilled responses](prompting-best-practices.md#migrating-away-from-prefilled-responses)
- [Tool use](prompting-best-practices.md#tool-use)
  - [Tool usage](prompting-best-practices.md#tool-usage)
  - [Optimize parallel tool calling](prompting-best-practices.md#optimize-parallel-tool-calling)
- [Thinking and reasoning](prompting-best-practices.md#thinking-and-reasoning)
  - [Overthinking and excessive thoroughness](prompting-best-practices.md#overthinking-and-excessive-thoroughness)
  - [Leverage thinking & interleaved thinking capabilities](prompting-best-practices.md#leverage-thinking--interleaved-thinking-capabilities)
- [Agentic systems](prompting-best-practices.md#agentic-systems)
  - [Long-horizon reasoning and state tracking](prompting-best-practices.md#long-horizon-reasoning-and-state-tracking)
    - [Context awareness and multi-window workflows](prompting-best-practices.md#context-awareness-and-multi-window-workflows)
    - [Multi-context window workflows](prompting-best-practices.md#multi-context-window-workflows)
    - [State management best practices](prompting-best-practices.md#state-management-best-practices)
  - [Balancing autonomy and safety](prompting-best-practices.md#balancing-autonomy-and-safety)
  - [Research and information gathering](prompting-best-practices.md#research-and-information-gathering)
  - [Subagent orchestration](prompting-best-practices.md#subagent-orchestration)
  - [Chain complex prompts](prompting-best-practices.md#chain-complex-prompts)
  - [Reduce file creation in agentic coding](prompting-best-practices.md#reduce-file-creation-in-agentic-coding)
  - [Overeagerness](prompting-best-practices.md#overeagerness)
  - [Avoid focusing on passing tests and hard-coding](prompting-best-practices.md#avoid-focusing-on-passing-tests-and-hard-coding)
  - [Minimizing hallucinations in agentic coding](prompting-best-practices.md#minimizing-hallucinations-in-agentic-coding)
- [Capability-specific tips](prompting-best-practices.md#capability-specific-tips)
  - [Improved vision capabilities](prompting-best-practices.md#improved-vision-capabilities)
  - [Frontend design](prompting-best-practices.md#frontend-design)
- [Migration considerations](prompting-best-practices.md#migration-considerations)
  - [Migrating to Claude Sonnet 5 from Claude Sonnet 4.5 or earlier](prompting-best-practices.md#migrating-to-claude-sonnet-5-from-claude-sonnet-45-or-earlier)
- [Next steps](prompting-best-practices.md#next-steps)

