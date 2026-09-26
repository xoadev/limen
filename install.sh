#!/bin/sh
# limen's installer for a node: downloads the static binary for this machine from the latest release, checks it
# against the release's SHA256SUMS, and runs `limen install`, which creates the users (or, on OpenWrt, root's
# dropbear keys), the sudo rule, /etc/limen and, with a repository, asks for its token and checks it out.
#
#   curl -fsSL https://raw.githubusercontent.com/xoadev/limen/main/install.sh | sudo sh
#   wget -qO- https://raw.githubusercontent.com/xoadev/limen/main/install.sh | sh          (OpenWrt, as root)
#
# POSIX sh, not bash: OpenWrt has only busybox's ash. It asks on the terminal; every answer can come from the
# environment instead, which is how it runs unattended:
#
#   LIMEN_READ_KEY     the hub's public key (the read role), or a .pub file         asked; required
#   LIMEN_DEPLOY_KEY   CI's or a person's public key (the deploy role), or a file   asked; "none" for no deploy role
#   LIMEN_FROM         addresses or CIDRs the keys may connect from (not OpenWrt)   asked; empty for anywhere
#   LIMEN_REPO         the Git repository this node follows                         asked; "none" for no repository
#   LIMEN_BRANCH       its branch                                                   asked; default main
#   LIMEN_PATH         this node's folder in it                                     asked; default nodes/<hostname>
#   LIMEN_REPO_TOKEN   a token that reads it, when it is private                    asked by `limen install` if needed
#   LIMEN_VERSION      the release to install, X.Y.Z                                default: the latest
#   LIMEN_BINARY       a limen binary already on this machine, instead of downloading one
#   LIMEN_YES=1        ask nothing: the environment and the defaults
set -eu

SOURCE=xoadev/limen

say() { printf '%s\n' "$*"; }
die() {
  printf 'limen-install: %s\n' "$*" >&2
  exit 1
}

[ "$(id -u)" = 0 ] || die "run it as root (… | sudo sh)"

# Answers come from the terminal: under `curl | sh`, stdin is this script.
interactive=1
if [ "${LIMEN_YES:-0}" = 1 ] || ! (: < /dev/tty) 2> /dev/null; then interactive=0; fi

# ask <variable> <question> <default>: keeps a value the environment already gave.
ask() {
  eval "given=\${$1:-}"
  if [ -n "$given" ]; then return 0; fi
  if [ "$interactive" = 0 ]; then
    eval "$1=\$3"
    return 0
  fi
  if [ -n "$3" ]; then printf '%s [%s]: ' "$2" "$3" > /dev/tty; else printf '%s: ' "$2" > /dev/tty; fi
  read -r answer < /dev/tty || answer=
  [ -n "$answer" ] || answer=$3
  eval "$1=\$answer"
}

# A public key given as itself or as the path of a .pub file.
key() {
  if [ -f "$1" ]; then cat "$1"; else printf '%s' "$1"; fi
}

confirm() {
  [ "$interactive" = 1 ] || return 0
  printf '%s [Y/n]: ' "$1" > /dev/tty
  read -r answer < /dev/tty || answer=
  case "$answer" in n* | N*) return 1 ;; *) return 0 ;; esac
}

fetch() {
  if command -v curl > /dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget > /dev/null 2>&1; then
    wget -qO "$2" "$1"
  elif command -v uclient-fetch > /dev/null 2>&1; then
    uclient-fetch -qO "$2" "$1"
  else
    die "no curl, wget or uclient-fetch to download with"
  fi
}

openwrt=0
[ -f /etc/openwrt_release ] && openwrt=1
hostname=$(cat /proc/sys/kernel/hostname 2> /dev/null || echo node)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

# --- the binary ----------------------------------------------------------------------------------------------------
if [ -n "${LIMEN_BINARY:-}" ]; then
  [ -x "$LIMEN_BINARY" ] || die "LIMEN_BINARY=$LIMEN_BINARY is not an executable file"
  cp "$LIMEN_BINARY" "$work/limen"
else
  case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "there is no limen binary for $(uname -m); the releases have x86_64 and aarch64" ;;
  esac
  version=${LIMEN_VERSION:-}
  if [ -z "$version" ]; then
    fetch "https://api.github.com/repos/$SOURCE/releases/latest" "$work/latest.json" ||
      die "cannot ask GitHub for the latest release; set LIMEN_VERSION=X.Y.Z"
    version=$(sed -n 's/.*"tag_name": *"v\([0-9][0-9.]*\)".*/\1/p' "$work/latest.json" | head -n 1)
    [ -n "$version" ] || die "no release found at github.com/$SOURCE"
  fi
  name="limen-$version-linux-$arch"
  say "Downloading limen $version for $arch"
  fetch "https://github.com/$SOURCE/releases/download/v$version/$name" "$work/limen" || die "cannot download $name"
  fetch "https://github.com/$SOURCE/releases/download/v$version/SHA256SUMS" "$work/SHA256SUMS" || die "cannot download SHA256SUMS"
  command -v sha256sum > /dev/null 2>&1 || die "sha256sum is needed to check the download"
  expected=$(grep " \./$name\$" "$work/SHA256SUMS" | cut -d ' ' -f 1)
  actual=$(sha256sum "$work/limen" | cut -d ' ' -f 1)
  if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then die "$name does not match the release's SHA256SUMS"; fi
  chmod 0755 "$work/limen"
fi
"$work/limen" --version > /dev/null || die "the binary does not run on this machine"

# --- the answers ---------------------------------------------------------------------------------------------------
if [ "$interactive" = 1 ]; then
  say ""
  say "limen: $("$work/limen" --version) on $hostname$([ "$openwrt" = 1 ] && echo ' (OpenWrt)')"
  say "Public keys can be pasted (ssh-ed25519 AAAA…) or given as the path of a .pub file."
fi
ask LIMEN_READ_KEY "The hub's public key (read role)" ""
[ -n "$LIMEN_READ_KEY" ] || die "the hub's public key is required (LIMEN_READ_KEY)"
ask LIMEN_DEPLOY_KEY "CI's or your public key for the deploy role (none to skip)" "none"
if [ "$openwrt" = 0 ]; then ask LIMEN_FROM "Addresses the keys may connect from, e.g. 100.64.0.0/10 (empty: anywhere)" ""; fi
ask LIMEN_REPO "Repository this node follows, e.g. https://github.com/you/infra.git (none to skip)" "none"
if [ "$LIMEN_REPO" != none ]; then
  ask LIMEN_BRANCH "Its branch" "main"
  ask LIMEN_PATH "This node's folder in it" "nodes/$hostname"
fi

# --- git, when there is a repository -------------------------------------------------------------------------------
if [ "$LIMEN_REPO" != none ] && ! command -v git > /dev/null 2>&1; then
  if [ "$openwrt" = 1 ]; then
    if command -v apk > /dev/null 2>&1; then install_git="apk update && apk add git-http"; else install_git="opkg update && opkg install git-http"; fi
  elif command -v apt-get > /dev/null 2>&1; then
    install_git="apt-get update && apt-get install -y git"
  else
    die "the repository needs git on this node; install it and run this again"
  fi
  confirm "The repository needs git, which is not installed. Run '$install_git'?" || die "git is needed for the repository"
  sh -c "$install_git" || die "could not install git"
fi

# --- limen install -------------------------------------------------------------------------------------------------
set -- install --read-key "$(key "$LIMEN_READ_KEY")"
[ "$LIMEN_DEPLOY_KEY" = none ] || set -- "$@" --deploy-key "$(key "$LIMEN_DEPLOY_KEY")"
[ -z "${LIMEN_FROM:-}" ] || set -- "$@" --from "$LIMEN_FROM"
[ "$LIMEN_REPO" = none ] || set -- "$@" --repo "$LIMEN_REPO" --branch "$LIMEN_BRANCH" --path "$LIMEN_PATH"
say ""
# `limen install` asks for the repository's token itself, hidden, when the repository needs one.
if [ -n "${LIMEN_REPO_TOKEN:-}" ]; then
  printf '%s\n' "$LIMEN_REPO_TOKEN" | "$work/limen" "$@"
elif [ "$interactive" = 1 ]; then
  "$work/limen" "$@" < /dev/tty
else
  "$work/limen" "$@" < /dev/null
fi
