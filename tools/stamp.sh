#!/usr/bin/env bash
# make stamp: writes the version the binary reports (`limen version`, `hello`, MCP serverInfo). Not committed:
# `dev` unless whoever builds sets LIMEN_VERSION, which is what a release does.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
version=${LIMEN_VERSION:-dev}
[[ "$version" =~ ^[0-9A-Za-z.+-]+$ ]] || { echo "stamp: LIMEN_VERSION '$version' has characters a version does not" >&2; exit 64; }
out="$ROOT/kotlin/core/src/limen/core/BuildStamp.kt"
content="package limen.core

const val LIMEN_VERSION = \"$version\"
"
# Rewritten only when it changes, so the toolchain does not recompile `core` on every build.
if [[ ! -f "$out" || "$(cat "$out")"$'\n' != "$content" ]]; then
  printf '%s' "$content" > "$out"
fi
