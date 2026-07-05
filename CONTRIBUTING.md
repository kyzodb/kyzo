# Contributing to KyzoDB

Thank you for your interest in KyzoDB. This page explains how the project accepts help — and, just as
importantly, how it currently doesn't.

## Issues are welcome, and valued

Bug reports, questions, and feature requests are genuinely useful, and the best way to help the
project today. If something is wrong, unclear, or missing, please
[open an issue](https://github.com/kyzodb/kyzo/issues). A strong bug report — a minimal KyzoScript
reproduction, what you expected, what you actually got, and your platform — is one of the most
valuable things you can send.

## We are not accepting code contributions right now

KyzoDB does not currently accept external pull requests. This is a deliberate stewardship choice, in
the tradition of projects like [SQLite](https://www.sqlite.org/copyright.html), and it is not a
judgment on anyone's work:

- **Design coherence.** The engine is young and its architecture is still settling. A single, tightly
  coordinated set of authors keeps the type system, the query semantics, and the on-disk format
  consistent while they are still in flux.
- **A correctness bar that's hard to delegate.** Every change lands only after an adversarial review
  and a differential oracle agree, byte for byte. That gate is far easier to hold with a small team
  than across an open inbound-PR surface.
- **Clean provenance.** KyzoDB is an MPL-2.0 fork of [CozoDB](https://github.com/cozodb/cozo).
  Keeping the copyright and attribution chain unambiguous is simpler without an inbound-contribution
  licensing surface.

This posture may relax as the project matures. For now, the most valuable contribution you can make
is a sharp issue.

## Security

If you believe you've found a security vulnerability, please report it privately (see
[SECURITY.md](SECURITY.md) if present, or the security contact on the repository) rather than opening
a public issue.
