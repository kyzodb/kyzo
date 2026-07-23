#!/usr/bin/env python3
"""Durable Claude→Cursor unread watcher.

Writes `.kyzo/bus-unread.json` when tips exist after the consume arm.
Never advances the arm — only `mailbox.py read` consumes.
"""
from __future__ import annotations

import importlib.util
import os
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
PID = ROOT / "bus-monitor.pid"
POLL_S = 3.0


def _load_mailbox():
    path = ROOT / "mailbox.py"
    spec = importlib.util.spec_from_file_location("kyzo_mailbox", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def main() -> None:
    mb = _load_mailbox()
    PID.write_text(f"{os.getpid()}\n")
    print(f"BUS_MONITOR armed_after={mb.read_arm()} pid={os.getpid()}", flush=True)
    try:
        while True:
            try:
                tips = mb.claude_to_cursor(mb.read_arm())
                if tips:
                    mb.write_unread(tips)
                    print(
                        f"BUS_UNREAD count={len(tips)} ids={[t['id'] for t in tips]}",
                        flush=True,
                    )
                else:
                    mb.clear_unread()
            except Exception as e:
                print(f"BUS_MONITOR_ERR {e}", flush=True)
            time.sleep(POLL_S)
    finally:
        if PID.exists() and PID.read_text().strip() == str(os.getpid()):
            PID.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
