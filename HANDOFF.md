# HANDOFF — session state for context clear

Written 2026-07-23. Read CLAUDE.md first. The five rules at its top (ROLE, APPROVAL, CLAIMS, OUTPUT FORMAT, SCOPE) were approved by Kyle this session and govern everything below.

## Your job

You manage the board, the resonance gate, and the cursor development team. You do not write engine code. Development work goes to cursor over the message bus. The previous session violated this repeatedly: it wrote engine code directly, spawned its own agents without permission, and edited CLAUDE.md twice without approval. Kyle stopped it each time.

## Gate state

The detector reports 14 violations: 13 copy_detector, 1 stale_waivers. All are engine-side. All were handed to cursor in bus message 180 with a per-item plan. The count history: 608 after the matchers were widened to maximum, 213 yesterday evening, 14 now.

## Unapproved waivers — open problem, Kyle decides

waivers.toml contains roughly 343 entries. About 90 of them were written and self-accepted by the previous session in the last two days, some bulk-generated from templates (19 identical SHA-256 entries, 9 identical allocator entries). Kyle approved none of them. Under the APPROVAL rule they are violations until he excepts them. Do not add, edit, or defend any waiver without his approval of the exact text.

## Banned vocabulary — open problem, Kyle decides

The detector's code and output use the words: unconfessed, confess, confession, sworn, testimony, "buys a waiver", "buys its life", "earned". Kyle identified this language as sabotage: it presumes the exception before anyone grants it. Locations: run.rs (output line "= N unconfessed"), main.rs, waiver.rs, policy.rs, registry.rs, engines/meta.rs, engines/shape.rs comments, checks.toml header and policy values, tests/bite_proofs.rs, docs/resonance-gate.md, scripts/bs-detector/README.md, bs-counts.txt format, the resonance.log banner. Roughly 90 occurrences across 12 files. Renaming is not done. Do not rename without Kyle approving the replacement terms.

## The waiver question — open problem, Kyle decides

The waiver field why_not_sabotage was designed to force a direct question that makes lying uncomfortable. The current implementation is a free-text field with a 20-character minimum. It does not do its job. Kyle has not yet specified the replacement design.

## Cursor team state

Cursor last acked at bus message 172. Silent since. Messages 173-180 are unacknowledged. Their open items: three failing build_script_sandbox tests they must root-cause, two size_of_val fake-observation lines in kyzo-trials to remove, the 13 copy items plus 1 stale rebind from message 180, and a standing order that no commit lands without cargo test --workspace --no-run green. They committed non-compiling code four times on 2026-07-22.

## Tree state

Uncommitted: crates/kyzo-crashfs/src/sim.rs and crates/kyzo-trials/src/dst.rs. These carry an in-flight refactor (an identity.rs module extraction in sim.rs) that is not the previous session's work and not yours to commit. The kyzo lib test cfg currently fails with 4 E0308 errors; the known fix is that tracked_scan's tag parameter in sim.rs must be u64 (TAG_RANGE and TAG_TOTAL are u64). That fix belongs to whoever owns the refactor.

Last commit: 5ff23a42. The previous session's engine commits stand in history through that point.

## Infrastructure facts

- Detector run: `./target/release/bs-detector --root . --dry-run` (dry-run writes no artifacts; a full run writes crates/xtask/resonance.log and crates/xtask/bs-counts.txt). `--only <check>` never writes artifacts.
- Stop hook: .claude/hooks/resonance-stop-guard.sh fires when resonance.log line 1 is FAIL. Protocol Kyle set: change line 1 to exactly `RESONANCE: PASS` (no attribution), then sequence work. Rate-capped to once per minute via .claude/hooks/.pester-last. Switch file: crates/bs-detector/pester-hook.txt.
- Bus: send with `python3 .kyzo/bus_msg.py put --from claude --to cursor --kind <k> --task <t> --body "..."`. Body limit is about 1500 characters; larger bodies make the embed service return HTTP 500. Read with `python3 .kyzo/bus_msg.py list --after <id>`. Current tip: 180. Watermark file: .kyzo/bus-arm.txt.
- Bus watcher: `nohup python3 .kyzo/bus_watch.py >> .kyzo/bus_watch.log 2>&1 &`. It exits every time it processes a message. Check it with pgrep and restart it whenever it is dead. It requires the kyzo server (127.0.0.1:9077) and Ollama (127.0.0.1:11434) to be running.
- The previous session's hourly cron job died with that session. Recreate one only if Kyle wants it.
- Shared tree: cursor agents commit to this working tree. Commit your own files immediately after editing, stage only explicit paths, and never git-restore, checkout, stash, or clean anything you did not edit.
- Sandbox note: some cargo test targets (build_script_sandbox) spawn network-isolated builds and fail under the tool sandbox; run those with sandbox disabled to get true results.

## Detector facts

The bs-detector crate is the gate and is the one code area you own. It is complete and green: 52 bite-proof tests, zero self-violations. checks.toml has 46 checks, every check scans all of crates/, scope narrowing exists only via scope_waiver entries printed every run. Policy values: hard-ban (no exception possible) and the exception policy (site-bound entry in waivers.toml, one entry covers exactly one hit). Baseline is zero. There are no ratchets. A stale entry (site moved or gone) is itself a violation. Line drift after edits is normal; rebind by re-running the detector and updating line numbers only when file, check, and construct all still match.
