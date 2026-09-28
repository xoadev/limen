#!/usr/bin/env bash
# make hooks: links the hooks of tools/ into .git/hooks. A reminder on this machine, not a gate: the gate is the
# pull request and `check`.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
ln -sf "$ROOT/tools/pre-push.sh" "$ROOT/.git/hooks/pre-push"
echo "hooks: pre-push installed"
