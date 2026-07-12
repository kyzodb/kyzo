---
paths:
  - "crates/kyzo-lsp/**/*.rs"
---

# Zone: Host, LSP — the language server

Editor tooling over the language, not over the engine.

## Required

- Speaks the model's parse tier only: diagnostics, spans, and completions come
  from the same lift and refusals users get at runtime — one grammar, one
  truth about what parses.
- Every diagnostic carries the parse tier's reason and span verbatim; the LSP
  invents no judgments of its own.
- Degrades gracefully: a file that does not parse still gets best-effort
  spans, never a crash.

## Forbidden

- Depending on the engine (storage, execution, session) — the language server
  needs the language, not the database.
- A private parse door or a second grammar — if the LSP needs something the
  parse tier does not expose, extend the parse tier's public surface.
- Editor-side reimplementation of formatting — the canonical formatter is the
  one formatter.
