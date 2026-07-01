#!/usr/bin/env bash
# MPL header preservation gate: every tracked .rs file must carry the Mozilla Public
# License header. KyzoDB is an MPL-2.0 fork of CozoDB; original copyright headers are
# preserved verbatim, ours added alongside, never overwriting (see CLAUDE.md).
#
# Runnable locally: scripts/check-mpl-headers.sh
set -euo pipefail
cd "$(dirname "$0")/.."

files=$(git ls-files '*.rs')
if [ -z "$files" ]; then
  echo "MPL header gate: no .rs files yet — armed but idle"
  exit 0
fi

fail=0
while IFS= read -r f; do
  if ! head -n 12 "$f" | grep -q "Mozilla Public License"; then
    echo "FAIL missing MPL header: $f"
    fail=1
  fi
done <<< "$files"

if [ "$fail" -ne 0 ]; then
  echo "MPL header gate: FAILED — every .rs file must reference the Mozilla Public License in its header."
  exit 1
fi

echo "MPL header gate: clean ($(echo "$files" | wc -l) .rs files checked)"
