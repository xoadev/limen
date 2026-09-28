#!/usr/bin/env bash
# The Rust build, for the Makefile: the toolchain rust-toolchain.toml pins, and static musl binaries that run on any
# Linux (spec §11).
#
#   tools/cargo.sh build | test | fmt | clippy | cli | artifact [arch] | clean
#
#   VARIANT=debug|release   what `cli` and `artifact` mean (debug links in seconds; a published binary is release)
#   ARCH="x86_64 aarch64"   what `cli` builds (default: this machine's)
#   FIX=1                   `fmt` rewrites instead of checking
#
# The version comes from the environment, `dev` otherwise: LIMEN_VERSION (X.Y.Z, from the release tag),
# LIMEN_BUILD_DATE (ISO-8601 UTC) and LIMEN_BUILD_NUMBER (the CI run). Checked here, so a wrong variable can't put
# anything else into the binary.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "cargo: install Rust with rustup (https://rustup.rs); rust-toolchain.toml pins the version" >&2; exit 1; }

version=${LIMEN_VERSION:-}
date=${LIMEN_BUILD_DATE:-}
number=${LIMEN_BUILD_NUMBER:-}
[[ -z "$version" || "$version" == dev || "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "cargo: LIMEN_VERSION '$version' is not X.Y.Z" >&2; exit 64; }
[[ -z "$date" || "$date" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}(T[0-9]{2}:[0-9]{2}:[0-9]{2}Z)?$ ]] || { echo "cargo: LIMEN_BUILD_DATE '$date' is not ISO-8601" >&2; exit 64; }
[[ -z "$number" || "$number" =~ ^[0-9]{1,12}$ ]] || { echo "cargo: LIMEN_BUILD_NUMBER '$number' is not a number" >&2; exit 64; }

VARIANT=${VARIANT:-debug}
[[ "$VARIANT" == debug || "$VARIANT" == release ]] || { echo "cargo: VARIANT is debug or release" >&2; exit 64; }

host_arch() {
  case "$(uname -m)" in
    x86_64 | amd64) echo x86_64 ;;
    aarch64 | arm64) echo aarch64 ;;
    *) echo "cargo: no binary for a $(uname -m) machine" >&2; exit 1 ;;
  esac
}

case "${1:-}" in
  build) cargo build --workspace --all-targets --locked ;;
  test) cargo test --workspace --locked ;;
  fmt) if [[ "${FIX:-0}" == 1 ]]; then cargo fmt --all; else cargo fmt --all --check; fi ;;
  clippy) cargo clippy --workspace --all-targets --locked -- -D warnings ;;
  cli)
    for arch in ${ARCH:-$(host_arch)}; do
      flags=(-p limen --locked --target "$arch-unknown-linux-musl")
      [[ "$VARIANT" == release ]] && flags+=(--release)
      cargo build "${flags[@]}"
      echo "cli: $(tools/cargo.sh artifact "$arch")"
    done
    ;;
  artifact) echo "$ROOT/target/${2:-$(host_arch)}-unknown-linux-musl/$VARIANT/limen" ;;
  clean) cargo clean ;;
  *) echo "usage: tools/cargo.sh build|test|fmt|clippy|cli|artifact [arch]|clean" >&2; exit 64 ;;
esac
