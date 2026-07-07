# Task #21 — Pre-merge commit cross-attribution cleanup (PARKED)

Before merging the older story branch to main, clean commit cross-attribution
where one commit carries another story's hunk:

- `ba820b8` — carries #86's work.
- `1d2de0d` — carries #73's work.
- `5f93b12` — (#61) carries #80's laws-ungating hunk.

Goal: each commit's message/authorship reflects the work it actually contains,
so the main history is truthful. Do this as part of the release path in
`task-36-release-0.9.0.md` (cleanup #21 -> merge branch to main).

Verify these SHAs still exist / are relevant against current main before
acting — the #118/#119 work landed after this was captured.
