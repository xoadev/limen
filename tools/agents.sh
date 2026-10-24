#!/usr/bin/env bash
# The head of AGENTS.md, everything above the MARK line, is shared by our repositories and lives at AGENTS_URL.
#
#   tools/agents.sh sync    rewrites that head from AGENTS_URL, keeping what is below the mark
#   tools/agents.sh check   fails if the head differs from AGENTS_URL (warns when it is unreachable)
set -euo pipefail
cd "$(dirname "$0")/.."

MARK='<!-- xoadev:end -->'
url=${AGENTS_URL:?AGENTS_URL is not set: run this through make agents-sync or make agents-check}
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

case "${1:-}" in
  sync)
    grep -qx "$MARK" AGENTS.md || { echo "AGENTS.md has no '$MARK' line: add it below the shared head" >&2; exit 1; }
    curl -fsS --max-time 20 "$url" -o "$tmp"
    sed -n "/^$MARK\$/,\$p" AGENTS.md >>"$tmp"
    cat "$tmp" >AGENTS.md
    ;;
  check)
    if ! curl -fsS --max-time 20 "$url" -o "$tmp"; then
      echo "warning: $url unreachable, AGENTS.md not checked" >&2
      exit 0
    fi
    sed "/^$MARK\$/,\$d" AGENTS.md | diff -u "$tmp" - >&2 \
      || { echo "AGENTS.md: the shared head differs from $url; run make agents-sync" >&2; exit 1; }
    ;;
  *) echo "usage: tools/agents.sh sync|check" >&2; exit 2 ;;
esac
