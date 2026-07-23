#!/usr/bin/env python3
"""Legacy one-shot tip printer. Does NOT advance the consume arm.

Only `mailbox.py read` advances `.kyzo/bus-arm.txt`. This watcher uses its own
watermark (`.kyzo/bus-watch-arm.txt`) so it cannot silence the stop-hook latch.
Prefer `.kyzo/bus_monitor.py` + Cursor stop hooks for delivery.
"""
from __future__ import annotations

import json
import time
import urllib.request
from pathlib import Path

KYZO = "http://127.0.0.1:9077/text-query"
ROOT = Path(__file__).resolve().parent
# Separate from consume arm — never write bus-arm.txt from this process.
WATCH_ARM = ROOT / "bus-watch-arm.txt"
CONSUME_ARM = ROOT / "bus-arm.txt"


def query(script: str) -> dict:
    req = urllib.request.Request(
        KYZO,
        data=json.dumps({"script": script, "params": {}}).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())


def read_arm(path: Path) -> int:
    if not path.exists():
        return 0
    try:
        return int(path.read_text().strip() or "0")
    except ValueError:
        return 0


def main() -> None:
    # Seed watch arm from consume arm once so we don't reprint ancient tips.
    if not WATCH_ARM.exists():
        WATCH_ARM.write_text(f"{read_arm(CONSUME_ARM)}\n")
    arm = read_arm(WATCH_ARM)
    print(f"BUS_CLAUDE_WATCHER armed_after={arm} (watch-arm only; consume arm untouched)", flush=True)
    while True:
        d = query(
            "?[id, from_agent, to_agent, kind, story, task, standing, body] := "
            "*agent_messages{id, from_agent, to_agent, kind, story, task, standing, body}, "
            f"id > {arm} :order id"
        )
        if not d.get("ok"):
            print("BUS_QUERY_ERR", d.get("message"), flush=True)
            time.sleep(3)
            continue
        for r in d.get("rows") or []:
            mid, frm, to, kind, story, task, standing, body = r
            mid = int(mid)
            print(
                f"BUS_NEW id={mid} from={frm} to={to} kind={kind} task={task}",
                flush=True,
            )
            if frm == "claude" and to == "cursor":
                print(f"BUS_CLAUDE_TIP id={mid} kind={kind} task={task}", flush=True)
                print("---", flush=True)
                print(body, flush=True)
                print("---", flush=True)
                WATCH_ARM.write_text(f"{mid}\n")
                return
            arm = max(arm, mid)
            WATCH_ARM.write_text(f"{arm}\n")
        time.sleep(3)


if __name__ == "__main__":
    main()
