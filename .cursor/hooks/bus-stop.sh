#!/usr/bin/env bash
# Thin wrapper — real logic in bus_stop.py (needs stdin for hook JSON).
set -u
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
exec /usr/bin/python3 "$ROOT/.cursor/hooks/bus_stop.py"
