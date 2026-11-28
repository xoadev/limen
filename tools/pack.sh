#!/usr/bin/env bash
# One pack as it is released: packs/<PACK> as release/limen-pack-<PACK>-<VERSION>.tar.gz, with release/SHA256SUMS
# and the release's notes, for xoa-base's release.yml (packs.yml), which publishes what it leaves there.
#
# The same bytes from the same commit, whoever runs it: names sorted, root as owner, the last commit's time on every
# file and none in gzip's header. Nobody but the owner may write a file, as limen requires of a script.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${PACK:?PACK=<pack> is the folder under packs/}"
VERSION=${VERSION:-dev}
[[ "$PACK" =~ ^[a-z0-9][a-z0-9_-]*$ && -d "packs/$PACK" ]] || {
  echo "pack: no pack '$PACK' under packs/" >&2
  exit 1
}
[[ "$VERSION" == dev || "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "pack: VERSION must be X.Y.Z" >&2
  exit 1
}

name=limen-pack-$PACK-$VERSION.tar.gz
rm -rf release
mkdir release
tar --create --sort=name --owner=0 --group=0 --numeric-owner --mode=go-w \
  --mtime="@$(git log -1 --format=%ct)" --directory=packs "$PACK" | gzip -9n >"release/$name"
# `./*` and not `*`: a name starting with a dash would slip into sha256sum as an option.
(cd release && sha256sum ./* >SHA256SUMS)
cat >release/notes.md <<NOTES
**limen pack $PACK $VERSION** · build ${BUILD_NUMBER:-none}

- \`$name\`: the folder \`$PACK/\`, owned by root and writable only by its owner. Unpack it where the machine's
  \`limen.toml\` lists it, as [docs/scripts.md](https://github.com/xoadev/limen/blob/main/docs/scripts.md#released-packs)
  says.
- \`SHA256SUMS\`: to check what you downloaded. Pin that hash, not only the tag.

\`gh attestation verify $name --repo xoadev/limen\` says it was built by this repository's packs workflow, which calls
xoa-base's release workflow.
NOTES
tar -tvzf "release/$name"
cat release/SHA256SUMS
