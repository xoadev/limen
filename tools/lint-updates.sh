#!/usr/bin/env bash
# make lint-updates: whether a linter pinned in tools/lint.sh has a newer release at least a week old. Dependabot
# cannot see them (lint.sh is no package manifest), so lint-updates.yml runs this weekly and opens an issue.
# Exits 1 when one is behind, naming the release to bump to; AGENTS.md says how to bump it.
set -euo pipefail
cd "$(dirname "$0")/.."

# The *_VERSION variable of lint.sh and the GitHub repository it is released from.
TOOLS=(SHELLCHECK=koalaman/shellcheck ACTIONLINT=rhysd/actionlint RUMDL=rvben/rumdl TYPOS=crate-ci/typos)
cutoff=$(date -u -d '7 days ago' +%Y-%m-%dT%H:%M:%SZ)
auth=()
[[ -n "${GITHUB_TOKEN:-}" ]] && auth=(-H "Authorization: Bearer $GITHUB_TOKEN")

status=0
for entry in "${TOOLS[@]}"; do
  variable=${entry%%=*} repo=${entry#*=}
  pinned=$(sed -n "s/^${variable}_VERSION=//p" tools/lint.sh)
  [[ -n "$pinned" ]] || { echo "lint-updates: no ${variable}_VERSION in tools/lint.sh" >&2; exit 1; }
  latest=$(curl -fsSL --proto '=https' --retry 3 "${auth[@]}" "https://api.github.com/repos/$repo/releases?per_page=100" \
    | jq -r --arg cutoff "$cutoff" \
      'first(.[] | select((.draft or .prerelease) | not) | select(.published_at <= $cutoff)) | .tag_name | ltrimstr("v")')
  [[ -n "$latest" ]] || { echo "lint-updates: no release of $repo a week old" >&2; exit 1; }
  if [[ "$latest" != "$pinned" && "$(printf '%s\n' "$pinned" "$latest" | sort -V | tail -n1)" == "$latest" ]]; then
    echo "lint-updates: $repo $pinned -> $latest (https://github.com/$repo/releases/tag/v$latest)"
    status=1
  else
    echo "lint-updates: $repo $pinned is the latest a week old"
  fi
done
exit $status
