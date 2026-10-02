#!/usr/bin/env bash
# One pack as it is released: packs/<PACK> as dist/limen-pack-<PACK>-<VERSION>.tar.gz, with dist/SHA256SUMS.
#
# The same bytes from the same commit, whoever runs it: names sorted, root as owner, the last commit's time on every
# file and none in gzip's header. Nobody but the owner may write a file, as limen requires of a script.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${PACK:?PACK=<pack> is the folder under packs/}"
VERSION=${VERSION:-dev}
[[ "$PACK" =~ ^[a-z0-9][a-z0-9_-]*$ && -d "packs/$PACK" ]] || { echo "pack: no pack '$PACK' under packs/" >&2; exit 1; }
[[ "$VERSION" == dev || "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "pack: VERSION must be X.Y.Z" >&2; exit 1; }

name=limen-pack-$PACK-$VERSION.tar.gz
rm -rf dist
mkdir dist
tar --create --sort=name --owner=0 --group=0 --numeric-owner --mode=go-w \
  --mtime="@$(git log -1 --format=%ct)" --directory=packs "$PACK" | gzip -9n > "dist/$name"
# `./*` and not `*`: a name starting with a dash would slip into sha256sum as an option.
(cd dist && sha256sum ./* > SHA256SUMS)
tar -tvzf "dist/$name"
cat dist/SHA256SUMS
