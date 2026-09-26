#!/usr/bin/env bash
# make lint: the linters of this repository, cheap ones first.
#
#   .github/workflows/*.yml   actionlint   syntax and expressions of GitHub Actions
#   tools/*.sh, tools/kt      shellcheck   quoting, unset variables, the classics
#   kotlin/**/*.kt            ktlint       the rules of .editorconfig
#
# (the tool names go second on purpose: a comment line that starts with a linter's name is read by that linter
# as a directive)
#
# Single-file binaries pinned by version, downloaded once into this machine's cache. None needs root or a package
# manager: a linter that is not there is a linter nobody runs.
set -euo pipefail
cd "$(dirname "$0")/.."

KTLINT_VERSION=1.8.0
SHELLCHECK_VERSION=0.11.0
ACTIONLINT_VERSION=1.7.12

CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/limen/lint"
mkdir -p "$CACHE"

fix=0
[[ "${1:-}" == "--fix" ]] && fix=1

case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 al_arch=amd64 ;;
  arm64 | aarch64) arch=aarch64 al_arch=arm64 ;;
  *) echo "lint: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

fetch() {
  local name=$1 url=$2
  [[ -x "$CACHE/$name" ]] && return 0
  echo "lint: downloading $name"
  curl -fsSL --retry 3 -o "$CACHE/$name.part" "$url" || { echo "lint: cannot download $url" >&2; return 1; }
  mv "$CACHE/$name.part" "$CACHE/$name"
  chmod +x "$CACHE/$name"
}

fetch_tar() {
  local name=$1 url=$2 inner=$3 tmp
  [[ -x "$CACHE/$name" ]] && return 0
  echo "lint: downloading $name"
  tmp=$(mktemp -d)
  curl -fsSL --retry 3 -o "$tmp/archive" "$url" || { echo "lint: cannot download $url" >&2; rm -rf "$tmp"; return 1; }
  tar -xf "$tmp/archive" -C "$tmp"
  mv "$tmp/$inner" "$CACHE/$name"
  chmod +x "$CACHE/$name"
  rm -rf "$tmp"
}

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

fetch_tar "shellcheck-$SHELLCHECK_VERSION" \
  "https://github.com/koalaman/shellcheck/releases/download/v$SHELLCHECK_VERSION/shellcheck-v$SHELLCHECK_VERSION.linux.$arch.tar.xz" \
  "shellcheck-v$SHELLCHECK_VERSION/shellcheck"
run shellcheck "$CACHE/shellcheck-$SHELLCHECK_VERSION" tools/*.sh tools/kt

fetch_tar "actionlint-$ACTIONLINT_VERSION" \
  "https://github.com/rhysd/actionlint/releases/download/v$ACTIONLINT_VERSION/actionlint_${ACTIONLINT_VERSION}_linux_${al_arch}.tar.gz" \
  actionlint
# With -shellcheck, so the `run:` blocks are checked here as strictly as on the CI runner, which has it installed.
run actionlint "$CACHE/actionlint-$ACTIONLINT_VERSION" -shellcheck "$CACHE/shellcheck-$SHELLCHECK_VERSION"

# A self-executing jar: it needs a JDK on the machine.
if ! command -v java >/dev/null 2>&1; then
  echo "lint: ktlint needs a JDK on the machine; install one and run it again" >&2
  exit 1
fi
fetch "ktlint-$KTLINT_VERSION" "https://github.com/pinterest/ktlint/releases/download/$KTLINT_VERSION/ktlint"
# `BuildStamp.kt` is written by tools/stamp.sh; formatting it would only fight the generator.
GENERATED='!kotlin/core/src/limen/core/BuildStamp.kt'
if [[ $fix -eq 1 ]]; then
  run ktlint "$CACHE/ktlint-$KTLINT_VERSION" --format --relative "kotlin/**/*.kt" "$GENERATED"
else
  run ktlint "$CACHE/ktlint-$KTLINT_VERSION" --relative "kotlin/**/*.kt" "$GENERATED"
fi

[[ $status -eq 0 ]] && echo "lint: OK"
exit $status
