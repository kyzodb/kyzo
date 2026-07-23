#!/usr/bin/env python3
"""postToolUse: re-arm bus monitor after mailbox.py read."""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path


def main() -> None:
    root = Path(__file__).resolve().parents[2]
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        print("{}")
        return

    name = payload.get("tool_name") or ""
    inp = payload.get("tool_input") or {}
    cmd = inp.get("command") if isinstance(inp, dict) else ""
    cmd = cmd or ""
    tokens = cmd.split()
    if name != "Shell" or "mailbox.py" not in cmd or "read" not in tokens:
        print("{}")
        return

    monitor = root / ".kyzo" / "bus_monitor.py"
    pidfile = root / ".kyzo" / "bus-monitor.pid"
    log = root / ".kyzo" / "bus_monitor.log"

    alive = False
    if pidfile.exists():
        try:
            pid = int(pidfile.read_text().strip())
            os.kill(pid, 0)
            alive = True
        except (ValueError, OSError):
            alive = False
    if not alive:
        subprocess.Popen(
            ["/usr/bin/python3", str(monitor)],
            cwd=str(root),
            stdin=subprocess.DEVNULL,
            stdout=open(log, "a"),
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        print(json.dumps({"additional_context": "Mailbox consumed; bus monitor re-armed."}))
    else:
        print(json.dumps({"additional_context": "Mailbox consumed; bus monitor still running."}))


if __name__ == "__main__":
    main()
