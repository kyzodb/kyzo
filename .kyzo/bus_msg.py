#!/usr/bin/env python3
"""Minimal agent bus client for 127.0.0.1:9077 agent_messages."""
from __future__ import annotations

import argparse
import json
import sys
import urllib.request

KYZO = "http://127.0.0.1:9077/text-query"
OLLAMA = "http://127.0.0.1:11434/api/embeddings"
MODEL = "granite-embedding:278m"
DIM = 768


def post_json(url: str, payload: dict, timeout: float = 60.0) -> dict:
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def embed(text: str) -> list[float]:
    d = post_json(OLLAMA, {"model": MODEL, "prompt": text})
    vec = d.get("embedding") or d.get("embeddings")
    if isinstance(vec, list) and vec and isinstance(vec[0], list):
        vec = vec[0]
    if not vec or len(vec) != DIM:
        raise SystemExit(f"bad embedding dim: {None if not vec else len(vec)}")
    return [float(x) for x in vec]


def next_id() -> int:
    d = post_json(
        KYZO,
        {
            "script": "?[id] := *agent_messages{id}",
            "params": {},
        },
    )
    if not d.get("ok"):
        raise SystemExit(d.get("message") or d)
    ids = [int(r[0]) for r in d.get("rows") or []]
    return (max(ids) + 1) if ids else 1


def _sq(s: str) -> str:
    """KyzoScript single-quoted string literal."""
    return "'" + s.replace("\\", "\\\\").replace("'", "\\'") + "'"


def put_msg(
    *,
    from_agent: str,
    to_agent: str,
    kind: str,
    story: str,
    task: str,
    standing: str,
    body: str,
    msg_id: int | None = None,
) -> int:
    mid = msg_id if msg_id is not None else next_id()
    vec = embed(body)
    lit = "[" + ",".join(str(x) for x in vec) + "]"
    script = (
        "?[id, from_agent, to_agent, kind, story, task, standing, body, body_vec] <- [["
        f"{mid}, {_sq(from_agent)}, {_sq(to_agent)}, {_sq(kind)}, "
        f"{_sq(story)}, {_sq(task)}, {_sq(standing)}, "
        f"{_sq(body)}, vec({lit})"
        "]] :put agent_messages {id => from_agent, to_agent, kind, story, task, standing, body, body_vec}"
    )
    d = post_json(KYZO, {"script": script, "params": {}})
    if not d.get("ok"):
        raise SystemExit(d.get("message") or d.get("display") or d)
    return mid


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
        raise SystemExit(d.get("message") or d)
    return d.get("rows") or []


def main() -> None:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("put")
    p.add_argument("--from", dest="from_agent", default="cursor")
    p.add_argument("--to", dest="to_agent", default="claude")
    p.add_argument("--kind", required=True)
    p.add_argument("--story", default="resonance")
    p.add_argument("--task", default="")
    p.add_argument("--standing", default="Open")
    p.add_argument("--body", required=True)
    p.add_argument("--id", type=int, default=None)

    l = sub.add_parser("list")
    l.add_argument("--after", type=int, default=0)

    args = ap.parse_args()
    if args.cmd == "put":
        mid = put_msg(
            from_agent=args.from_agent,
            to_agent=args.to_agent,
            kind=args.kind,
            story=args.story,
            task=args.task,
            standing=args.standing,
            body=args.body,
            msg_id=args.id,
        )
        print(json.dumps({"ok": True, "id": mid}))
    elif args.cmd == "list":
        rows = list_since(args.after)
        print(json.dumps({"ok": True, "rows": rows}, indent=2))


if __name__ == "__main__":
    main()
