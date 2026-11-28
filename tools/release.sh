#!/usr/bin/env bash
# make release-files: everything a limen release publishes, into release/, for xoa-base's release.yml, which runs
# it at the draft's commit and publishes what it leaves there.
#
#   VERSION=X.Y.Z (or dev)   the number stamped into `limen --version` and the files' names
#   BUILD_NUMBER             the CI run, stamped too
#   RELEASE_IMAGE            where the image is published, for the notes (default ghcr.io/xoadev/limen)
#
# The binaries are built release, not debug: smaller and optimised, which is what goes on a router. Every
# end-to-end scene runs with them, and the hub image is built from them and started on both architectures, so a
# binary that does not run on its platform stops the release before anything is handed over. Needs Docker with
# buildx, and QEMU for the arm64 image.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=${VERSION:-dev}
[[ "$VERSION" == dev || "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "release: VERSION must be X.Y.Z or dev" >&2
  exit 1
}
image=${RELEASE_IMAGE:-ghcr.io/xoadev/limen}
repository=https://github.com/xoadev/limen
LIMEN_VERSION=$VERSION LIMEN_BUILD_DATE=$(date -u +%Y-%m-%dT%H:%M:%SZ) LIMEN_BUILD_NUMBER=${BUILD_NUMBER:-}
export LIMEN_VERSION LIMEN_BUILD_DATE LIMEN_BUILD_NUMBER
# Both binaries, linked one after the other: `make e2e` and `make docker` go through `make cli` and find them done.
export ARCH="x86_64 aarch64" VARIANT=release
echo "release: $VERSION, build ${LIMEN_BUILD_NUMBER:-none}"

make --no-print-directory cli
make --no-print-directory e2e

rm -rf release
mkdir release
# Named after what `uname -m` says, so `limen-<version>-linux-$(uname -m)` is the one to download.
for arch in x86_64 aarch64; do
  cp "$(tools/cargo.sh artifact "$arch")" "release/limen-$VERSION-linux-$arch"
  file "release/limen-$VERSION-linux-$arch" | grep -Eq "static(-pie)?(ally)? linked" || {
    echo "release: limen-$VERSION-linux-$arch is not statically linked" >&2
    exit 1
  }
done
# `./*` and not `*`: a name starting with a dash would slip into sha256sum as an option.
(cd release && sha256sum ./* >SHA256SUMS)

binaries=(BINARY_AMD64="$PWD/release/limen-$VERSION-linux-x86_64" BINARY_ARM64="$PWD/release/limen-$VERSION-linux-aarch64")
for arch in amd64 arm64; do
  make --no-print-directory docker PLATFORMS="linux/$arch" IMAGE="limen:$VERSION-$arch" "${binaries[@]}"
  docker run --rm --platform "linux/$arch" "limen:$VERSION-$arch" --version
  test "$(docker image inspect --format '{{.Architecture}}' "limen:$VERSION-$arch")" = "$arch"
done
labels=(
  "org.opencontainers.image.source=$repository"
  "org.opencontainers.image.version=$VERSION"
  "org.opencontainers.image.revision=$(git rev-parse HEAD)"
  "org.opencontainers.image.licenses=Apache-2.0"
)
make --no-print-directory docker PLATFORMS=linux/amd64,linux/arm64 IMAGE="limen:$VERSION" \
  OCI="$PWD/release/image.tar" "${binaries[@]}" LABELS="${labels[*]}"

cat >release/notes.md <<NOTES
**limen $VERSION** · build ${LIMEN_BUILD_NUMBER:-none} · $LIMEN_BUILD_DATE

- \`$image:$VERSION\`: the hub, for amd64 and arm64 under one tag. Start it and join machines as the
  [README]($repository#install) says.
- \`limen-$VERSION-linux-x86_64\` and \`limen-$VERSION-linux-aarch64\`: one static binary per architecture, for any
  Linux (Debian, Ubuntu, OpenWrt…). \`install.sh\` picks the one \`uname -m\` names.
- \`SHA256SUMS\`: to check what you downloaded.

Each binary and the image carry a build provenance attestation:
\`gh attestation verify limen-$VERSION-linux-x86_64 --repo xoadev/limen\`, or
\`gh attestation verify oci://$image:$VERSION --repo xoadev/limen\`, says they were built by this repository's
release workflow, which calls xoa-base's.
NOTES
ls -l release
