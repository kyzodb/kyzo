# Security Advisories

The dated trail of every security-classed defect KyzoDB has shipped a fix for, or publicly
disclosed. See [`SECURITY.md`](../SECURITY.md) for how to report a vulnerability.

## The trail's law

One file per advisory, named `YYYY-MM-DD-<slug>.md`, where the date is the public disclosure date.
Entries are append-only: once published, an entry is never edited except to add links to
subsequent fixes or releases.

## The never-silently-fixed rule

A security-classed fix may not ship in any release before its advisory entry lands here. There is
no such thing as a quiet security fix in a changelog line — if it's security-classed, it gets an
entry in this directory first.

## Required fields per entry

- **Affected versions** — the version range carrying the defect.
- **Severity** — the severity call made during triage.
- **Mechanism** — what the defect allowed, stated plainly, not euphemized.
- **Fix** — the commit that fixed it and the first release it shipped in.
- **Timeline** — report received → acknowledgement → fix landed → public disclosure.
- **Credit** — the reporter, unless anonymity was requested.
