#!/usr/bin/env bash
# make lint: the linters of this repository, cheap ones first.
#
#   .github/workflows/*.yml   actionlint   syntax and expressions of GitHub Actions
#   tools/, install.sh        shellcheck   quoting, unset variables, the classics
#   crates/**/*.rs            rustfmt      the layout of rustfmt.toml (FIX=1 rewrites)
#   crates/**/*.rs            clippy       Rust's own lints, every warning an error
#
# (the tool names go second on purpose: a comment line that starts with a linter's name is read by that linter
# as a directive)
#
# The shell and workflow linters are single-file binaries pinned by version, downloaded once into this machine's cache;
# the Rust ones come with the toolchain. None needs root or a package manager: a linter that is not there is a linter
# nobody runs.
set -euo pipefail
cd "$(dirname "$0")/.."

SHELLCHECK_VERSION=0.11.0
ACTIONLINT_VERSION=1.7.12

CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/limen/lint"
mkdir -p "$CACHE"

case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 al_arch=amd64 ;;
  arm64 | aarch64) arch=aarch64 al_arch=arm64 ;;
  *) echo "lint: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

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
# install.sh is POSIX sh (OpenWrt has no bash): its shebang tells shellcheck to hold it to that.
run shellcheck "$CACHE/shellcheck-$SHELLCHECK_VERSION" tools/*.sh install.sh

fetch_tar "actionlint-$ACTIONLINT_VERSION" \
  "https://github.com/rhysd/actionlint/releases/download/v$ACTIONLINT_VERSION/actionlint_${ACTIONLINT_VERSION}_linux_${al_arch}.tar.gz" \
  actionlint
# With -shellcheck, so the `run:` blocks are checked here as strictly as on the CI runner, which has it installed.
run actionlint "$CACHE/actionlint-$ACTIONLINT_VERSION" -shellcheck "$CACHE/shellcheck-$SHELLCHECK_VERSION"

run rustfmt tools/cargo.sh fmt
run clippy tools/cargo.sh clippy

[[ $status -eq 0 ]] && echo "lint: OK"
exit $status
