#!/usr/bin/env bash
# Claude Code's PostToolUse hook (.claude/settings.json): formats a Rust file as soon as an agent edits it, as
# `make lint FIX=1` would. It never fails the edit: `make lint` is the check.
set -uo pipefail
file=$(sed -n 's/.*"file_path" *: *"\([^"]*\)".*/\1/p' | head -n 1)
[[ "$file" == *.rs && -f "$file" ]] || exit 0
cd "$(dirname "$0")/.." || exit 0
PATH="$HOME/.cargo/bin:$PATH" rustfmt --edition 2024 "$file" 2> /dev/null || true
