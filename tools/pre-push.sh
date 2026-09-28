#!/usr/bin/env bash
# pre-push: refuses to push to main. Everything goes in through a pull request (AGENTS.md).
set -euo pipefail
while read -r _ _ remote_ref _; do
  if [[ "$remote_ref" == "refs/heads/main" ]]; then
    echo "pre-push: main only changes through a pull request; push a branch and open one" >&2
    exit 1
  fi
done
