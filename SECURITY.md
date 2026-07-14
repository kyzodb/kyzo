# Security Policy

## Supported scope

Security fixes are considered for the latest published release and `main`. Older releases are not
backported.

## Reporting a vulnerability

Report privately through GitHub's private vulnerability reporting on `kyzodb/kyzo`: repository
**Security** tab → **Report a vulnerability**. There is no email channel — do not report a
suspected vulnerability through a GitHub issue, discussion, or pull request, since those are public
from the moment they're filed.

## Response commitment

- **Acknowledgement within 7 days** of the report landing.
- **Fix or coordinated public disclosure within 90 days** of the report landing, whichever comes
  first.

## The never-silently-fixed rule

Every security-classed defect gets a dated entry in [`advisories/`](advisories/README.md) — a
security fix never ships in any release before its advisory entry lands. See
[`advisories/README.md`](advisories/README.md) for the entry format and the trail's rules.

## What reporters can expect

- Triage against the report, with follow-up questions if the mechanism needs clarifying.
- A severity call, communicated back to the reporter.
- Credit in the published advisory entry, unless the reporter asks to stay anonymous.
