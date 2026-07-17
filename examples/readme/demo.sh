#!/usr/bin/env bash
# Seed the README ops world and run the knowing query.
# Requires: a `kyzo` binary on PATH, or KYZO=/path/to/kyzo, or a release build at
# ../../target/release/kyzo relative to this script.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [[ -n "${KYZO:-}" ]]; then
  BIN="$KYZO"
elif command -v kyzo >/dev/null 2>&1; then
  BIN="$(command -v kyzo)"
elif [[ -x "$ROOT/target/release/kyzo" ]]; then
  BIN="$ROOT/target/release/kyzo"
else
  echo "No kyzo binary found. Install from releases, or: cargo build -p kyzo-bin --release" >&2
  exit 1
fi

PORT="${PORT:-$((19070 + RANDOM % 1000))}"
LOG="$(mktemp)"
cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  rm -f "$LOG"
}
trap cleanup EXIT

"$BIN" server -e mem -b 127.0.0.1 -P "$PORT" >"$LOG" 2>&1 &
PID=$!

for _ in $(seq 1 50); do
  if curl -sf "http://127.0.0.1:${PORT}/" >/dev/null 2>&1 \
    || curl -sf -X POST "http://127.0.0.1:${PORT}/text-query" \
      -H 'content-type: application/json' \
      -d '{"script":"?[x] <- [[1]]","params":{}}' >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done

export DEMO_URL="http://127.0.0.1:${PORT}/text-query"
python3 - <<'PY'
from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

URL = os.environ["DEMO_URL"]


def q(script: str, params: dict | None = None) -> dict:
    body = json.dumps({"script": script, "params": params or {}}).encode()
    req = urllib.request.Request(URL, data=body, headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return json.load(resp)
    except urllib.error.HTTPError as e:
        return json.loads(e.read().decode())


def must(script: str, params: dict | None = None, label: str = "") -> dict:
    out = q(script, params)
    if not out.get("ok"):
        print(f"FAIL {label or script[:48]}", file=sys.stderr)
        print(json.dumps(out, indent=2)[:800], file=sys.stderr)
        sys.exit(1)
    return out


def vec(xs: list[float]) -> str:
    return "vec([" + ", ".join(f"{x:.3f}" for x in xs) + "])"


must(
    """?[src, dst] <- [
      ['svc-ci', 'role-deploy'], ['role-deploy', 'role-prod-ro'],
      ['role-prod-ro', 'db-customers'], ['role-deploy', 'kms-signing'],
      ['svc-ci', 's3-artifacts'], ['role-prod-ro', 's3-artifacts'],
      ['attacker', 'svc-ci'], ['legacy-batch', 'role-prod-ro']
    ] :create can_access {src, dst}"""
)
must(
    """?[id, env, critical] <- [
      ['db-customers', 'prod', true], ['kms-signing', 'prod', true],
      ['s3-artifacts', 'prod', false], ['role-deploy', 'prod', true],
      ['role-prod-ro', 'prod', false], ['svc-ci', 'ci', false],
      ['attacker', 'ext', true], ['legacy-batch', 'prod', false]
    ] :create service {id => env, critical}"""
)

incidents = [
    ("INC-pool", "Redis pool exhausted under load", [1, 0, 0, 0, 0, 0, 0, 0], "prod", True),
    ("INC-pool2", "Connection pool saturation in redis", [0.95, 0.05, 0, 0, 0, 0, 0, 0], "prod", True),
    ("INC-auth", "JWT validation bypass in gateway", [0, 1, 0, 0, 0, 0, 0, 0], "prod", True),
    ("INC-auth2", "Auth token forgery attempt", [0.1, 0.9, 0, 0, 0, 0, 0, 0], "staging", True),
    ("INC-disk", "Disk full on log volume", [0, 0, 1, 0, 0, 0, 0, 0], "prod", True),
    ("INC-noise", "Unrelated marketing pixel timeout", [0, 0, 0, 1, 0, 0, 0, 0], "prod", True),
    ("INC-old", "Old pool issue retracted", [0.9, 0.1, 0, 0, 0, 0, 0, 0], "prod", False),
    ("INC-rare", "Rare prod-only redis failover flap", [0.85, 0.1, 0.05, 0, 0, 0, 0, 0], "prod", True),
]
rows = ", ".join(
    f"['{i}', '{s}', {vec(e)}, '{env}', {str(live).lower()}]"
    for i, s, e, env, live in incidents
)
must(
    f"?[id, summary, emb, env, live] <- [{rows}] "
    f":create incident {{id => summary, emb: <F32; 8>, env, live}}"
)
must("::hnsw create incident:emb {fields: [emb], dim: 8, m: 16, ef_construction: 64, distance: L2}")
must(
    """?[inc, rb] <- [
      ['INC-pool', 'RB-pool-sizing'], ['INC-pool2', 'RB-pool-sizing'],
      ['INC-auth', 'RB-jwt-rotate'], ['INC-disk', 'RB-disk-expand'],
      ['INC-rare', 'RB-redis-failover']
    ] :create cites {inc, rb}"""
)
must(
    """?[id, title] <- [
      ['RB-pool-sizing', 'Resize redis pools'],
      ['RB-jwt-rotate', 'Rotate JWT keys'],
      ['RB-disk-expand', 'Expand log volumes'],
      ['RB-redis-failover', 'Redis failover playbook']
    ] :create runbook {id => title}"""
)
must("?[subject, kind] <- [['INC-pool', 'affirmed'], ['INC-auth', 'affirmed']] :create claim {subject => kind}")
must("?[customer, tier] <- [['C-77', 'trial']] :create coverage {customer => tier} @ '2024-01-01T00:00:00Z'")
must("?[customer, tier] <- [['C-77', 'enterprise']] :put coverage {customer => tier} @ '2024-05-01T00:00:00Z'")

print()
print("══ knowing ══════════════════════════════════════════════════════════════")
print("near this alert · live · prod · has a runbook · not claimed ·")
print("and the attacker can still reach db-customers")
print()

knowing = must(
    """
near[id, dist] := ~incident:emb{id | query: vec($v), k: 5, bind_distance: dist}
reach[r] := *can_access{src: 'attacker', dst: r}
reach[r] := reach[m], *can_access{src: m, dst: r}
?[id, summary, rb, dist] :=
    near[id, dist],
    *incident{id, summary, live: true, env: 'prod'},
    *cites{inc: id, rb},
    not *claim{subject: id},
    reach[jewel],
    *service{id: jewel, critical: true},
    jewel == 'db-customers'
:order dist
""",
    {"v": [0.92, 0.08, 0, 0, 0, 0, 0, 0]},
)
for row in knowing.get("rows") or []:
    print(f"  {row[0]:10}  {row[1]:40}  {row[2]:18}  dist={row[3]:.4f}")

print()
print("══ as-of (same world) ═══════════════════════════════════════════════════")
march = must("?[tier] := *coverage{customer: 'C-77', tier @ '2024-03-15T00:00:00Z'}")
june = must("?[tier] := *coverage{customer: 'C-77', tier @ '2024-06-01T00:00:00Z'}")
print(f"  C-77 on incident date → {march['rows'][0][0]}")
print(f"  C-77 today            → {june['rows'][0][0]}")

print()
print("══ ::verify (engine vs reference oracle) ════════════════════════════════")
vfy = must(
    """
::verify {
  path[x, y] := *can_access[x, y]
  path[x, z] := path[x, y], *can_access[y, z]
  ?[x, y] := path[x, y]
}
"""
)
print(f"  {vfy['rows'][0]}")
print()
print("Done. One language. One snapshot. Re-run anytime: examples/readme/demo.sh")
PY
