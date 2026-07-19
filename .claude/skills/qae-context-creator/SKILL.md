---
name: qae-context-creator
description: Create precise, high-context direction for LLMs as Q/A/E triplets — the hardest question, the ruled answer, the enforcement that makes it stick. Use when writing specs, rulings, migration destinies, agent priming, review standards, or any context where an LLM must make a choice exactly the way you would.
---

# QAE Context Creator

Direction for LLMs fails two ways: vague principles the model can't apply, or step lists the model follows off a cliff. A QAE triplet fixes both by transferring the decision itself.

## The triplet

**Q — Question.** The hardest precise question the topic is still swerving around — the one that, once answered, forbids the alternatives. Not "how should we handle errors?" but "when a read's snapshot dies mid-query: refuse or restart — pick one — and what makes silent continue on a newer snapshot unrepresentable?" If your Q has an easy answer, you haven't found the real Q yet.

**A — Answer.** The pick, stated so the alternatives are foreclosed: what is chosen, and what becomes impossible, illegal, or unrepresentable because of it. Name names — exact types, files, variants, terms. An A that both sides of a debate could read as agreeing with them is not an A.

**E — Explanation.** Why this pick, and what makes it stick: what gets deleted, what gets refused (with the named outcome), and what proof, check, or meter enforces it. Name the soft alternative and say why it fails — the E is what stops the ruling from regressing when a future reader is tempted by it.

## Rules

- One decision per triplet. If the A rules two things, split it.
- The Q must be adversarial to your own A — the strongest form of the question, not a setup.
- The A must foreclose: after reading it, a reader cannot choose otherwise and claim compliance.
- The E must name enforcement, not just rationale. "Because it's cleaner" is not an E; "delete X, refuse with Y, proven by Z" is.
- Tag authority honestly: if the triplet derives from a higher document, cite it; if it is your own ruling, mark it as yours. Never dress your inference in the higher document's authority.

## Why this works

One triplet transfers a decision. A stack of triplets transfers the judgment function — the reader learns how you rule, then rules the same way on questions you never wrote down. That is why QAE context outperforms both principle lists (no application) and checklists (no judgment): it is worked examples of deciding, which is what in-context learning absorbs best.

## Output form

Number the triplets. Group them by layer or domain if there are more than a handful. Keep each triplet self-contained — a reader landing on one triplet cold should understand the ruling without reading the others.
