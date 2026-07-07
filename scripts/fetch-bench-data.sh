#!/usr/bin/env bash
#
# Fetch the standard published graphs the transitive-closure benchmark runs on.
# These are SNAP (Stanford Network Analysis Project) edge lists — real, cited,
# community-standard datasets. We download them from their canonical source
# rather than inventing or committing data; the URLs below ARE the provenance.
#
# Writes uncompressed edge lists to bench-data/ (gitignored). Idempotent.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
mkdir -p bench-data

# name -> SNAP URL. Small-to-medium graphs whose transitive closure completes
# under the memory cap on every engine revision we measure.
GRAPHS=(
  "email-Eu-core|https://snap.stanford.edu/data/email-Eu-core.txt.gz"
  "p2p-Gnutella08|https://snap.stanford.edu/data/p2p-Gnutella08.txt.gz"
  "wiki-Vote|https://snap.stanford.edu/data/wiki-Vote.txt.gz"
)

for entry in "${GRAPHS[@]}"; do
  name="${entry%%|*}"
  url="${entry##*|}"
  out="bench-data/${name}.txt"
  if [[ -s "$out" ]]; then
    echo "have  $out"
    continue
  fi
  echo "fetch $name <- $url"
  curl -sSL "$url" | gunzip > "$out"
done
echo "done -> bench-data/"
