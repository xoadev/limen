#!/usr/bin/env bash
# make docker: builds the hub image (limen:local, or IMAGE) from the binaries `make cli` produced.
#
# The build context is a scratch directory with just the binaries, laid out as `limen-<arch>` for the Dockerfile
# to pick by TARGETARCH, so the image ships exactly what a release attaches and Docker never sees the repository.
#
#   IMAGE=limen:local                   the tag
#   TAGS="a:1 a:latest"                 extra tags
#   PLATFORMS=linux/amd64,linux/arm64   what to build (default: this machine's)
#   PUSH=1                              push instead of loading into Docker (needed for more than one platform)
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=${IMAGE:-limen:local}
case "$(uname -m)" in
  x86_64) host=amd64 ;;
  aarch64 | arm64) host=arm64 ;;
  *) echo "docker: no image for a $(uname -m) host" >&2; exit 1 ;;
esac
PLATFORMS=${PLATFORMS:-linux/$host}
if [[ "${PUSH:-0}" != 1 && "$PLATFORMS" == *,* ]]; then
  echo "docker: Docker loads one platform per tag; build one, or PUSH=1" >&2
  exit 64
fi
context="$ROOT/kotlin/build/docker"
rm -rf "$context" && mkdir -p "$context"
IFS=, read -ra platforms <<< "$PLATFORMS"
for platform in "${platforms[@]}"; do
  arch=${platform#linux/}
  case "$arch" in
    amd64) kt_arch=x86_64 ;;
    arm64) kt_arch=aarch64 ;;
    *) echo "docker: no binary for $platform" >&2; exit 64 ;;
  esac
  cp "$("$ROOT/tools/kt" artifact "$kt_arch")" "$context/limen-$arch"
done
args=(--file "$ROOT/etc/Dockerfile" --platform "$PLATFORMS" --tag "$IMAGE")
for tag in ${TAGS:-}; do args+=(--tag "$tag"); done
if [[ "${PUSH:-0}" == 1 ]]; then args+=(--push); else args+=(--load); fi
docker buildx build "${args[@]}" "$context"
echo "docker: built $IMAGE for $PLATFORMS"
