#!/usr/bin/env bash
# make docker: builds the hub image (limen:local, or IMAGE) from the binaries `make cli` produced.
#
# The build context is a scratch directory with just the binaries, laid out as `limen-<arch>` for the Dockerfile to
# pick by TARGETARCH, so the image ships exactly what a release attaches and Docker never sees the repository. The
# release runs this same script: the image CI publishes and one built here come out of one code path.
#
#   IMAGE=limen:local                   the tag
#   TAGS="a:1 a:latest"                 extra tags, space-separated
#   LABELS="k=v k2=v2"                  OCI labels, space-separated
#   PLATFORMS=linux/amd64,linux/arm64   what to build (default: this machine's)
#   PUSH=1                              push instead of loading into Docker, which holds one platform per tag
#   BINARY_AMD64=<path>                 the binary for amd64 (default: what `make cli` built, for VARIANT)
#   BINARY_ARM64=<path>                 the binary for arm64 (idem)
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
    amd64) binary=${BINARY_AMD64:-} kt_arch=x86_64 ;;
    arm64) binary=${BINARY_ARM64:-} kt_arch=aarch64 ;;
    *) echo "docker: no binary for $platform; the platforms are linux/amd64 and linux/arm64" >&2; exit 64 ;;
  esac
  [[ -n "$binary" ]] || binary=$("$ROOT/tools/kt" artifact "$kt_arch")
  [[ -x "$binary" ]] || { echo "docker: no binary at $binary; run make cli first" >&2; exit 1; }
  cp "$binary" "$context/limen-$arch"
done
args=(--file "$ROOT/etc/Dockerfile" --platform "$PLATFORMS" --tag "$IMAGE")
for tag in ${TAGS:-}; do args+=(--tag "$tag"); done
for label in ${LABELS:-}; do args+=(--label "$label"); done
if [[ "${PUSH:-0}" == 1 ]]; then args+=(--push); else args+=(--load); fi
docker buildx build "${args[@]}" "$context"
[[ "${PUSH:-0}" == 1 ]] && pushed=", pushed" || pushed=""
echo "docker: built $IMAGE${TAGS:+ (+ $TAGS)} for $PLATFORMS$pushed"
