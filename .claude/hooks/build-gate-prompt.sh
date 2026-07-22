#!/usr/bin/env bash
# UserPromptSubmit hook: BUILD GATE notice + live BS-detector counts.
# The count line is produced by the gate itself — run_bs_detector writes
# crates/xtask/bs-counts.txt on every resonance run (watcher fires on every
# crates/ change). This script only echoes that artifact; it never counts
# the tree itself — a second counting authority is the fraud the gate kills.
printf '%s\n' '[BUILD GATE] Before you act, classify this prompt. If it asks you to write or modify CODE — engine or host source, a story/task implementation, or a system design — you MUST fully re-read .claude/skills/ontology-first-construction/SKILL.md with the Read tool first and construct only from that fresh re-read. Being already in context is not an exception. If the prompt asks for neither, disregard this notice.'

f="${CLAUDE_PROJECT_DIR:-.}/crates/xtask/bs-counts.txt"
if [ -f "$f" ]; then
    printf '[BS] %s\n' "$(cat "$f")"
else
    printf '[BS] no counts artifact yet — gate has not run since counts were armed\n'
fi
