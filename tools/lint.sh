#!/usr/bin/env bash
# make lint: the linters of this repository, cheap ones first.
#
#   .github/workflows/*.yml      actionlint   syntax and expressions of GitHub Actions
#   tools/, install.sh, packs/   shellcheck   quoting, unset variables, the classics
#   crates/**/*.rs               rustfmt      the layout of rustfmt.toml (FIX=1 rewrites)
#   crates/**/*.rs               clippy       Rust's own lints, every warning an error
#
# (the tool names go second on purpose: a comment line that starts with a linter's name is read by that linter
# as a directive)
#
# The shell and workflow linters are single-file binaries pinned by version and checksum, downloaded once into this
# machine's cache;
# the Rust ones come with the toolchain. None needs root or a package manager: a linter that is not there is a linter
# nobody runs.
set -euo pipefail
cd "$(dirname "$0")/.."

SHELLCHECK_VERSION=0.11.0
ACTIONLINT_VERSION=1.7.12
# sha256 of each archive, per architecture: a changed download is refused, not run. Update them with the versions.
declare -A SHA256=(
  [shellcheck-x86_64]=8c3be12b05d5c177a04c29e3c78ce89ac86f1595681cab149b65b97c4e227198
  [shellcheck-aarch64]=12b331c1d2db6b9eb13cfca64306b1b157a86eb69db83023e261eaa7e7c14588
  [actionlint-amd64]=8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8
  [actionlint-arm64]=325e971b6ba9bfa504672e29be93c24981eeb1c07576d730e9f7c8805afff0c6
)

CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/limen/lint"
mkdir -p "$CACHE"

case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 al_arch=amd64 ;;
  arm64 | aarch64) arch=aarch64 al_arch=arm64 ;;
  *) echo "lint: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

fetch_tar() {
  local name=$1 url=$2 inner=$3 sha256=$4 tmp
  [[ -x "$CACHE/$name" ]] && return 0
  echo "lint: downloading $name"
  tmp=$(mktemp -d)
  curl -fsSL --proto '=https' --retry 3 -o "$tmp/archive" "$url" || { echo "lint: cannot download $url" >&2; rm -rf "$tmp"; return 1; }
  echo "$sha256  $tmp/archive" | sha256sum --check --quiet || { echo "lint: $url is not the archive pinned here" >&2; rm -rf "$tmp"; return 1; }
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
  "shellcheck-v$SHELLCHECK_VERSION/shellcheck" "${SHA256[shellcheck-$arch]}"
# install.sh and the packs' scripts are POSIX sh (OpenWrt has no bash): their shebang tells shellcheck to hold them to
# that. A pack's scripts have no extension: they are its executable files.
mapfile -t pack_scripts < <(find packs -type f -perm -u+x | sort)
run shellcheck "$CACHE/shellcheck-$SHELLCHECK_VERSION" tools/*.sh install.sh "${pack_scripts[@]}"

fetch_tar "actionlint-$ACTIONLINT_VERSION" \
  "https://github.com/rhysd/actionlint/releases/download/v$ACTIONLINT_VERSION/actionlint_${ACTIONLINT_VERSION}_linux_${al_arch}.tar.gz" \
  actionlint "${SHA256[actionlint-$al_arch]}"
# With -shellcheck, so the `run:` blocks are checked here as strictly as on the CI runner, which has it installed.
run actionlint "$CACHE/actionlint-$ACTIONLINT_VERSION" -shellcheck "$CACHE/shellcheck-$SHELLCHECK_VERSION"

run rustfmt tools/cargo.sh fmt
run clippy tools/cargo.sh clippy

[[ $status -eq 0 ]] && echo "lint: OK"
exit $status
