---
name: phoenix-arize
description: Mandatory requirements for developing evals and telemetry in Arize Phoenix, whose primary purpose is VISUAL consumption of information by people. Use when instrumenting traces, spans, sessions, embeddings, annotations, or evaluator results for Phoenix, or choosing which OpenInference attributes and metadata to log. Design telemetry to activate Phoenix's visual features, not merely fill default columns.
---

# Arize Phoenix Observability Rules

1. Treat default table columns as the minimum Phoenix integration, not the target experience.

2. Design telemetry specifically to activate Phoenix’s strongest visual features: embedding projections, clustering, trace trees, session views, annotations, comparisons, metrics, and cost analysis.

3. Log vector embeddings alongside the associated text inputs and outputs so Phoenix can generate UMAP point-cloud visualizations.

4. Attach meaningful dimensions to embedding records so point clouds can be colored and segmented by tags, correctness, prediction outcome, experiment group, or other diagnostic attributes.

5. Provide both primary inference data and reference baseline data when semantic drift analysis is required.

6. Structure reference and primary datasets consistently so Phoenix can identify, cluster, and rank regions of semantic drift.

7. Assign a stable session ID to every related group of traces.

8. Use session IDs to group multi-turn conversations, asynchronous workflows, and related operations into a unified Phoenix session view.

9. Preserve the real execution hierarchy through correct parent-child span relationships.

10. Represent agentic execution using meaningful span kinds and nesting, such as:

    `Agent -> Chain -> Tool -> LLM`

11. Never emit related spans as an unrelated flat list when their execution hierarchy is known.

12. Follow the OpenInference semantic conventions for span kinds, attributes, inputs, outputs, model data, and relationships.

13. Log the prompt template separately from the rendered prompt sent to the model.

14. Log prompt variables as structured attributes rather than embedding them only inside the final prompt text.

15. Preserve enough prompt information for Phoenix to reconstruct how each variable produced the rendered prompt.

16. Attach structured metadata and tags to spans for every dimension likely to support filtering, comparison, diagnosis, or experimentation.

17. Prefer stable, consistently named metadata fields such as:

    * `experiment_group`
    * `model`
    * `temperature`
    * `environment`
    * `tenant`
    * `workflow`
    * `feature`
    * `release`
    * `dataset`
    * `user_feedback`

18. Do not encode important diagnostic dimensions only inside unstructured span text.

19. Record evaluator results using Phoenix’s native annotation schema rather than representing evaluators only as ordinary spans.

20. Attach evaluation annotations directly to the trace or span being evaluated.

21. Use clear annotation names and typed values, such as:

    * `hallucination = false`
    * `relevance = 0.95`
    * `correctness = 1`
    * `quality = "pass"`

22. Distinguish automated evaluator annotations from human feedback.

23. Preserve human-in-the-loop labels such as approval, rejection, thumbs-up, thumbs-down, correction, and reviewer notes.

24. Treat human feedback as ground-truth data that must remain queryable, filterable, and visually distinguishable.

25. Log prompt-token and completion-token counts using Phoenix-compatible OpenInference attributes.

26. Record the model identity accurately so Phoenix can associate usage with the correct pricing and performance characteristics.

27. Ensure spans have accurate start times, end times, and completion states.

28. Mark failures using proper error status and structured error attributes rather than placing failure information only in logs or output text.

29. Preserve latency at every meaningful level of execution, including agent, chain, retrieval, tool, and LLM spans.

30. Make retrieval operations independently observable when retrieval quality matters.

31. Log retrieved documents, relevance scores, ranks, identifiers, and source metadata in structured form.

32. Link evaluations, annotations, embeddings, retrieval records, prompts, and model calls back to the exact trace and span that produced them.

33. Use consistent schemas across environments and releases so Phoenix comparisons remain valid over time.

34. Optimize instrumentation for exploration, not merely ingestion: every important operational or quality question should correspond to a filterable field, visible annotation, trace relationship, embedding dimension, or metric.

35. Consider the Phoenix UI incomplete until an operator can:

    * Follow a request through the complete trace tree.
    * Group related traces into a session.
    * Inspect prompt templates and substitutions.
    * Compare successful and failed executions.
    * Filter by operational and experimental metadata.
    * See evaluator and human annotations directly on spans.
    * Explore semantic clusters and drift.
    * Analyze latency, errors, token usage, and cost.
    * Link every diagnostic signal back to the exact execution that produced it.
