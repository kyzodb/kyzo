# KyzoScript for VS Code

Syntax highlighting for KyzoScript (`.kz`, `.kzs`), the Datalog dialect KyzoDB queries and mutates
in. This is the editor-tooling scaffold from story #73 (KyzoScript devex): a hand-written TextMate
grammar derived directly from `kyzo-core/src/kyzoscript.pest`, wrapped as a minimal VS Code
extension. The playground's own editor consumes the same `syntaxes/kyzoscript.tmLanguage.json`
(TextMate grammars are the shared artifact between a VS Code extension and a Monaco-based web
editor).

## Layout

- `syntaxes/kyzoscript.tmLanguage.json` — the grammar. Every pattern's `comment` field cites the
  `kyzoscript.pest` rule it renders; re-derive against the grammar when the two drift.
- `language-configuration.json` — comment tokens (`#`, `/* */`), bracket/quote auto-closing.
- `package.json` — the VS Code extension manifest (language + grammar contribution points).

## Try it

```sh
cd editors/vscode-kyzoscript
code --install-extension .   # or: open this folder in VS Code and F5 to launch an Extension
                              # Development Host
```

## Verifying the grammar

The grammar was checked against a real TextMate tokenizer (`vscode-textmate` + `vscode-oniguruma`,
the same engine VS Code and Monaco embed) over a sample program covering every chapter of
`kyzo-core/examples/language_tour.rs` — relations, rules, recursion, aggregation, both `@` forms,
vector/FTS search atoms, and an imperative block — not just eyeballed as JSON. There is no
checked-in test harness for this yet (it needs a `node_modules` this pure-Rust repo doesn't
otherwise carry); the smallest useful next step here is wiring that verification into CI as its own
job, not inside `cargo test`.

## What's covered

Line comments (`#`), nestable block comments (`/* /* */ */`), all three string forms (double-quoted,
single-quoted, and the `_"…"_`-fenced raw form with a matching close via its exact fence length),
every numeric literal shape (decimal/hex/octal/binary integers, `_` digit-group separators, both
float forms), the entry marker `?`, `$parameters`, `*relation`/`~relation:index` sigils and the bare
`relation:index` shape DDL statements use, every rule arrow (`:=`, `<-`, `<~`), the `@`/`@spans`/
`@delta`/`@delta_sys` validity-clause family, every operator, the `%`-imperative vocabulary, and
every `:option`/`::sys-op` via one general prefix pattern (deliberately not an enumerated keyword
list — enumeration is the thing that goes stale when the grammar grows a new option).

## Not yet covered (named, not hidden)

A formatter (`kyzo fmt` over the parse tree) and an LSP (diagnostics-as-you-type, completion against
a live store) are the other two pieces story #73 names under "editor tooling." Both are
substantially larger builds — a formatter needs a pretty-printer over the AST with real
round-trip-preserves-semantics tests, and an LSP is a standalone long-running server with its own
protocol surface — than a grammar file, and neither is started here.
