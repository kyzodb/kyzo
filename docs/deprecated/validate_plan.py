#!/usr/bin/env python3
"""Mech-suit validator for KyzoDB migration-model instances (migration-schema.json).

Structural checks the JSON Schema cannot express: joint satisfaction,
condemned/target disjointness, MECE ownership, authority honesty, meter
executability. Zero dependencies. Exit 0 = green, 1 = violations.

Usage: validate_plan.py <instance.json> [--decisions docs/decisions.md]
"""
import json
import re
import sys

ERR, WARN = [], []


def err(msg):
    ERR.append(msg)


def warn(msg):
    WARN.append(msg)


UNDECIDABLE = ["reachable refuse", "allowlisted review", "as appropriate", "as needed", "where sensible", "etc."]
WALL_WORDS = ["compile-fail", "crate boundary", "private constructor"]
DETECT_WORDS = ["residual accepted", "poison", "dst", "campaign", "chain-meet", "none at runtime"]


def check(plan, decisions_text):
    # -- basic contract ------------------------------------------------------
    if plan.get("status") != "cut_destiny":
        err("status must be cut_destiny")
    tags = set(plan.get("claim_tags", {})) - {"law"}
    if tags != {"Unconstructible", "Refused", "Unexposed"}:
        err(f"claim_tags must be exactly the three Spec tags, got {sorted(tags)}")
    auth = plan.get("authority", [])
    if not auth or "decisions.md" not in auth[0]:
        err("authority[0] must be decisions.md (sole Spec authority)")

    # -- condemned vs target disjointness -----------------------------------
    targets = set(plan.get("allowlist", {}).get("targets", []))
    delete_after = set(plan.get("allowlist", {}).get("delete_after", []))
    del_paths = {p["path"] for p in plan.get("deletes", {}).get("paths", [])}
    sev_paths = {e["path"] for e in plan.get("severance_edits", {}).get("entries", [])}
    both = targets & del_paths
    if both:
        err(f"allowlist.targets contains condemned paths: {sorted(both)}")
    for path in sorted(del_paths - delete_after - sev_paths):
        warn(f"deletes.path not in delete_after or severance_edits (deferred? state it in scope.not_this_json): {path}")
    bt_condemned = set(plan.get("by_target", {})) & del_paths
    if bt_condemned:
        err(f"by_target seats on condemned paths: {sorted(bt_condemned)}")

    # -- joint satisfaction: every meter path is licensed --------------------
    licensed = targets | delete_after | sev_paths
    meter_lists = [plan.get("deletes", {}).get("meters", []),
                   plan.get("tests", {}).get("meters", []),
                   plan.get("final_meters", [])]
    path_re = re.compile(r"(crates/[\w./-]+\.rs|\.claude/[\w./-]+\.md|docs/[\w./-]+\.(md|json))")
    for meters in meter_lists:
        for m in meters:
            for match in path_re.finditer(m):
                p = match.group(1)
                # directory-level and glob meters are checked by prefix
                if p not in licensed and not any(t.startswith(p.rstrip("/")) or p.startswith(t.rstrip("/"))
                                                 for t in licensed):
                    warn(f"meter names unlicensed path {p!r}: {m[:90]}")

    # -- MECE ownership per register -----------------------------------------
    # 'owns' is the declaration register; 'owns_ops' the body register;
    # 'owns_helpers' the helper register. A symbol may legally appear once in
    # the declaration register AND once in the body register (decl ≠ body is
    # the type law) — but never twice within the same register.
    for key in ("owns", "owns_ops", "owns_helpers"):
        owner = {}
        for seat, spec in plan.get("by_target", {}).items():
            for sym in spec.get(key, []):
                if sym.startswith("imports "):
                    continue
                if sym in owner:
                    err(f"symbol {sym!r} owned in register {key!r} by both {owner[sym]} and {seat}")
                owner[sym] = seat

    # -- authority honesty ---------------------------------------------------
    seat_nums = set()
    if decisions_text:
        seat_nums = {int(n) for n in re.findall(r"^(\d+)\.\s+\*\*", decisions_text, re.M)}
    for item in plan.get("refused", {}).get("items", []):
        a = item.get("authority", "")
        if "cut_destiny" not in a and "decisions.md" not in a:
            err(f"refused item {item.get('name', '?')!r}: authority must cite decisions.md or cut_destiny")
        if decisions_text:
            for n in re.findall(r"§\s*(\d+)", a):
                if int(n) not in seat_nums:
                    err(f"refused item cites decisions.md §{n}, which does not exist")
        if not item.get("variant"):
            err(f"refused item {item.get('name', '?')!r}: missing named variant")

    # -- unexposed: walls are not detection ----------------------------------
    for item in plan.get("unexposed", {}).get("items", []):
        det = item.get("detection", "").lower()
        if any(w in det for w in WALL_WORDS) and not any(d in det for d in DETECT_WORDS):
            err(f"unexposed item {item.get('name', '?')!r}: detection lists walls, not detection")
        if not any(d in det for d in DETECT_WORDS):
            warn(f"unexposed item {item.get('name', '?')!r}: no recognizable detection or 'residual accepted'")

    # -- unconstructible: compile-fail only, no symbol-absence proofs --------
    for item in plan.get("ontology", {}).get("unconstructible", []):
        proof = item.get("proof", "")
        if "compile-fail" not in proof:
            err(f"unconstructible {item.get('name', '?')!r}: proof must be compile-fail")
        if re.search(r"\brg\b|test ! -f|grep", proof):
            err(f"unconstructible {item.get('name', '?')!r}: rg/test-f cannot seal this tag")
        if "cannot exist" in item.get("name", "") or "second pub" in item.get("name", ""):
            warn(f"unconstructible {item.get('name', '?')!r}: symbol-absence claims belong in delete_guarded_invariants")

    # -- meter executability: no undecidable qualifiers ----------------------
    def sweep_meters(where, meters):
        for m in meters:
            low = m.lower()
            for q in UNDECIDABLE:
                if q in low:
                    err(f"{where}: undecidable qualifier {q!r} in meter: {m[:90]}")

    sweep_meters("deletes.meters", plan.get("deletes", {}).get("meters", []))
    sweep_meters("final_meters", plan.get("final_meters", []))
    for law in plan.get("seated_laws", []):
        sweep_meters(f"seated_laws[{law.get('law', '?')[:40]}]", [law.get("meter", "")])
        if "exceptions" not in law and ("panic" in law.get("meter", "") or "unwrap" in law.get("meter", "")):
            warn(f"seated_law {law.get('law', '?')[:40]!r}: grep meter without explicit exceptions ledger")

    # -- qae triplets --------------------------------------------------------
    for i, t in enumerate(plan.get("qae", [])):
        for k in ("q", "a", "e"):
            if not t.get(k, "").strip():
                err(f"qae[{i}]: empty {k!r}")
        if t.get("authority") != "cut_destiny":
            err(f"qae[{i}]: authority must be cut_destiny (open swerves are never Spec-ruled)")

    # -- campaigns: doubt about frozen law takes campaign form only ----------
    for c in plan.get("campaigns_proposed", []):
        if not c.get("campaign", "").strip():
            err(f"campaigns_proposed[{c.get('target_seat', '?')}]: empty campaign body")


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    plan = json.load(open(sys.argv[1]))
    decisions_text = None
    if "--decisions" in sys.argv:
        decisions_text = open(sys.argv[sys.argv.index("--decisions") + 1]).read()
    check(plan, decisions_text)
    for w in WARN:
        print(f"WARN  {w}")
    for e in ERR:
        print(f"ERROR {e}")
    print(f"\n{sys.argv[1]}: {len(ERR)} errors, {len(WARN)} warnings")
    return 1 if ERR else 0


if __name__ == "__main__":
    sys.exit(main())
