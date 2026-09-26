#!/usr/bin/env bash
# make stamp: writes kotlin/core/src/limen/core/BuildStamp.kt, what `limen version`, `hello` and the MCP server
# info say. Not committed: it changes with every release.
#
# A local build says `dev` and the day it was built, which answers "which one do I have?" without inventing a
# number, and changes at most once a day, so `core` is not recompiled on every build. CI fills in the rest:
#
#   LIMEN_VERSION        X.Y.Z, from the release tag (release.yml)
#   LIMEN_BUILD_DATE     ISO-8601 UTC, seconds: when the run started
#   LIMEN_BUILD_NUMBER   the number of the CI run
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
version=${LIMEN_VERSION:-dev}
date=${LIMEN_BUILD_DATE:-$(date -u +%Y-%m-%d)}
number=${LIMEN_BUILD_NUMBER:-}
# What a stamp may say, so a wrong environment variable can't put anything into the source.
[[ "$version" == dev || "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "stamp: LIMEN_VERSION '$version' is not X.Y.Z" >&2; exit 64; }
[[ "$date" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}(T[0-9]{2}:[0-9]{2}:[0-9]{2}Z)?$ ]] || { echo "stamp: LIMEN_BUILD_DATE '$date' is not ISO-8601" >&2; exit 64; }
[[ -z "$number" || "$number" =~ ^[0-9]{1,12}$ ]] || { echo "stamp: LIMEN_BUILD_NUMBER '$number' is not a number" >&2; exit 64; }
out="$ROOT/kotlin/core/src/limen/core/BuildStamp.kt"
content="package limen.core

// Written by tools/stamp.sh before every build; not committed.
const val LIMEN_VERSION = \"$version\"
const val LIMEN_BUILD_DATE = \"$date\"
const val LIMEN_BUILD_NUMBER = \"$number\"
"
# Rewritten only when it changes, so the toolchain does not recompile `core` for nothing.
if [[ ! -f "$out" || "$(cat "$out")"$'\n' != "$content" ]]; then
  printf '%s' "$content" > "$out"
fi
