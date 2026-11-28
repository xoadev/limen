#!/usr/bin/env bash
# make lint: the Rust linters, after xoa-base's (make help). rustfmt keeps rustfmt.toml's layout (FIX=1, from
# `make format`, rewrites instead), and clippy holds every warning an error.
set -euo pipefail
cd "$(dirname "$0")/.."

status=0
run() {
  local what=$1
  shift
  if "$@"; then
    echo "lint: $what — OK"
  else
    echo "lint: $what — FAIL" >&2
    status=1
  fi
}

run rustfmt tools/cargo.sh fmt
run clippy tools/cargo.sh clippy

[[ $status -eq 0 ]] && echo "lint: OK"
exit $status
