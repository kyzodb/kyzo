#!/usr/bin/env python3
"""stop hook: follow up while Claude→Cursor mail is unread."""
from __future__ import annotations

import importlib.util
import json
import sys
import time
from pathlib import Path


def log(root: Path, line: str) -> None:
    path = root / ".kyzo" / "hooks-run.log"
    try:
        with path.open("a") as f:
            f.write(f"{time.strftime('%Y-%m-%dT%H:%M:%S')} stop {line}\n")
    except OSError:
        pass


def load_mailbox(root: Path):
    path = root / ".kyzo" / "mailbox.py"
    spec = importlib.util.spec_from_file_location("kyzo_mailbox", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def main() -> None:
    root = Path(__file__).resolve().parents[2]
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        payload = {}

    status = payload.get("status") or ""
    loop_count = int(payload.get("loop_count") or 0)
    log(root, f"fired status={status!r} loop_count={loop_count}")

    if status != "completed":
        print("{}")
        return

    try:
        mb = load_mailbox(root)
        tips = mb.claude_to_cursor(mb.read_arm())
        mb.write_unread(tips)
    except Exception as e:
        log(root, f"mailbox_err {e}")
        print("{}")
        return

    if not tips:
        log(root, "no_unread")
        print("{}")
        return

    heads = []
    for t in tips[:5]:
        kind = t.get("kind") or "?"
        task = t.get("task") or ""
        heads.append(f"#{t.get('id')} {kind}" + (f"/{task}" if task else ""))
    more = "" if len(tips) <= 5 else f" (+{len(tips) - 5} more)"
    body = (
        f"UNREAD Claude→Cursor bus mail ({len(tips)}). Do not stop.\n"
        f"Run exactly: python3 .kyzo/mailbox.py read\n"
        f"Then act on every tip (delegate with background Task agents in Multitask; "
        f"do not hold this chat on grind work). Re-arm is automatic via postToolUse.\n"
        f"Tips: {', '.join(heads)}{more}\n"
        f"(stop-hook loop_count={loop_count})"
    )
    log(root, f"followup count={len(tips)} ids={[t['id'] for t in tips]}")
    print(json.dumps({"followup_message": body}))


if __name__ == "__main__":
    main()
