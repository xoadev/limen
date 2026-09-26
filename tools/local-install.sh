#!/usr/bin/env bash
# make local-install: copies the binary of `make cli` to ~/.local/bin/limen (or $PREFIX/limen).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PREFIX=${PREFIX:-$HOME/.local/bin}
binary=$("$ROOT/tools/kt" artifact)
mkdir -p "$PREFIX"
install -m 0755 "$binary" "$PREFIX/limen.tmp"
mv "$PREFIX/limen.tmp" "$PREFIX/limen"
echo "local-install: $PREFIX/limen ($("$PREFIX/limen" version))"
