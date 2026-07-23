#!/usr/bin/env python3
"""Claude→Cursor mailbox consume door.

Arm (`.kyzo/bus-arm.txt`) advances ONLY here. The monitor may write
`.kyzo/bus-unread.json` but never consumes. Hooks call `peek`; agents call `read`.
"""
from __future__ import annotations

import argparse
import json
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent
KYZO = "http://127.0.0.1:9077/text-query"
ARM = ROOT / "bus-arm.txt"
UNREAD = ROOT / "bus-unread.json"


def post_json(url: str, payload: dict, timeout: float = 10.0) -> dict:
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def read_arm() -> int:
    if not ARM.exists():
        return 0
    text = ARM.read_text().strip() or "0"
    try:
        return int(text)
    except ValueError:
        return 0


def write_arm(n: int) -> None:
    ARM.write_text(f"{n}\n")


def write_unread(tips: list[dict]) -> None:
    UNREAD.write_text(json.dumps({"tips": tips}, indent=2) + "\n")


def clear_unread() -> None:
    write_unread([])


def list_since(after_id: int) -> list[list]:
    d = post_json(
        KYZO,
        {
            "script": (
                "?[id, from_agent, to_agent, kind, story, task, standing, body] := "
                "*agent_messages{id, from_agent, to_agent, kind, story, task, standing, body}, "
                f"id > {int(after_id)} :order id"
            ),
            "params": {},
        },
    )
    if not d.get("ok"):
        raise RuntimeError(d.get("message") or d)
    return d.get("rows") or []


def claude_to_cursor(after_id: int) -> list[dict]:
    tips = []
    for row in list_since(after_id):
        mid, frm, to, kind, story, task, standing, body = row
        if frm != "claude" or to != "cursor":
            continue
        tips.append(
            {
                "id": int(mid),
                "kind": kind,
                "story": story,
                "task": task,
                "standing": standing,
                "body": body,
            }
        )
    return tips


def cmd_peek() -> int:
    """Non-consuming: refresh unread from bus relative to arm; print JSON."""
    try:
        tips = claude_to_cursor(read_arm())
    except Exception as e:
        print(json.dumps({"ok": False, "error": str(e)}))
        return 1
    write_unread(tips)
    print(json.dumps({"ok": True, "arm": read_arm(), "count": len(tips), "tips": tips}))
    return 0


def cmd_read() -> int:
    """Consume: print tips, advance arm past them, clear unread."""
    arm = read_arm()
    try:
        tips = claude_to_cursor(arm)
    except Exception as e:
        print(json.dumps({"ok": False, "error": str(e)}))
        return 1
    if tips:
        write_arm(max(t["id"] for t in tips))
    clear_unread()
    print(json.dumps({"ok": True, "arm_before": arm, "arm_after": read_arm(), "count": len(tips), "tips": tips}, indent=2))
    return 0


def cmd_status() -> int:
    unread = []
    if UNREAD.exists():
        try:
            unread = json.loads(UNREAD.read_text()).get("tips") or []
        except json.JSONDecodeError:
            unread = []
    print(
        json.dumps(
            {
                "ok": True,
                "arm": read_arm(),
                "unread_file_count": len(unread),
                "monitor_pid_file": (ROOT / "bus-monitor.pid").exists(),
            }
        )
    )
    return 0


def main() -> None:
    ap = argparse.ArgumentParser(description="Cursor mailbox (Claude→Cursor bus)")
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("peek", help="refresh unread; do not advance arm")
    sub.add_parser("read", help="consume unread Claude→Cursor tips; advance arm")
    sub.add_parser("status", help="arm + unread file status")
    args = ap.parse_args()
    if args.cmd == "peek":
        raise SystemExit(cmd_peek())
    if args.cmd == "read":
        raise SystemExit(cmd_read())
    if args.cmd == "status":
        raise SystemExit(cmd_status())
    raise SystemExit(2)


if __name__ == "__main__":
    main()
