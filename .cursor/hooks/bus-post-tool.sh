#!/usr/bin/env bash
set -u
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
exec /usr/bin/python3 "$ROOT/.cursor/hooks/bus_post_tool.py"
