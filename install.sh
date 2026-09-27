#!/bin/sh
# limen's installer. POSIX sh, not bash: OpenWrt has only busybox's ash.
#
# A machine, joining a hub (spec §10.1), as root — the hub's `limen invite <name>` prints this line:
#   curl -fsSL https://raw.githubusercontent.com/xoadev/limen/main/install.sh | sudo sh -s -- --join '<line>'
# A hub for this user, for Claude Code on this machine (stdio):
#   curl -fsSL https://raw.githubusercontent.com/xoadev/limen/main/install.sh | sh -s -- --hub
#
# It downloads the static binary for this machine from the latest release and checks it against SHA256SUMS. On a
# machine it asks only what is optional —the repository it follows—, installs git if that needs it, and runs
# `limen join`, which fetches the hub's key, checks it against the fingerprint in the line, creates the users (or
# root's dropbear keys on OpenWrt), asks for the repository's token if it is private, and reports to the hub.
#
# Every answer can come from the environment, which is how it runs unattended:
#   LIMEN_JOIN         the line of `limen invite` (or --join)
#   LIMEN_HUB_KEY      the hub's public key or its .pub file, when the hub has no HTTP (or --hub-key)
#   LIMEN_NAME         this machine's name on the hub, with LIMEN_HUB_KEY (or --name; default: its hostname)
#   LIMEN_REPO         the Git repository it follows; "none" for none
#   LIMEN_BRANCH       its branch (default main)
#   LIMEN_PATH         its folder in the repository (default nodes/<name>)
#   LIMEN_REPO_TOKEN   a token that reads it, when it is private (otherwise asked, with a link that creates it)
#   LIMEN_REPO_TOKEN_FILE  a file holding that token: kept off sudo's command line and out of the environment
#   LIMEN_DEPLOY_KEY   a public key for the deploy role (CI or a person), or its file
#   LIMEN_FROM         addresses or CIDRs the keys may connect from (not OpenWrt)
#   LIMEN_ADDRESS      where the hub reaches this machine, with LIMEN_JOIN (default: where it saw the request come from)
#   LIMEN_SSH_PORT     this machine's SSH port, when it is not 22
#   LIMEN_VERSION      the release to install, X.Y.Z (default: the latest)
#   LIMEN_BINARY       a limen binary already on this machine, instead of downloading one
#   LIMEN_YES=1        ask nothing
set -eu

SOURCE=xoadev/limen

say() { printf '%s\n' "$*"; }
die() {
  printf 'limen-install: %s\n' "$*" >&2
  exit 1
}

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

confirm() {
  [ "$interactive" = 1 ] || return 0
  printf '%s [Y/n]: ' "$1" > /dev/tty
  read -r answer < /dev/tty || answer=
  case "$answer" in n* | N*) return 1 ;; *) return 0 ;; esac
}

# A public key given as itself or as the path of a .pub file.
key() {
  if [ -f "$1" ]; then cat "$1"; else printf '%s' "$1"; fi
}

# Only over https, and only with a client that checks certificates: busybox's own wget doesn't.
fetch() {
  if command -v curl > /dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1"
  elif command -v uclient-fetch > /dev/null 2>&1; then
    uclient-fetch -qO "$2" "$1"
  elif command -v wget > /dev/null 2>&1 && ! wget --help 2>&1 | grep -q BusyBox; then
    wget --https-only -qO "$2" "$1"
  else
    die "no curl, uclient-fetch or GNU wget to download with (busybox's wget doesn't check certificates)"
  fi
}

# Everything runs from here, once the whole script has been read: under `curl | sh`, a download cut short runs
# nothing, and no command started below can read the rest of the script from stdin.
main() {
  mode=node
  while [ $# -gt 0 ]; do
    case "$1" in
      --join) LIMEN_JOIN=${2:-}; shift 2 ;;
      --hub-key) LIMEN_HUB_KEY=${2:-}; shift 2 ;;
      --name) LIMEN_NAME=${2:-}; shift 2 ;;
      --hub) mode=hub; shift ;;
      *) die "unknown option $1 (--join <line>, --hub-key <key> --name <name>, or --hub)" ;;
    esac
  done

  if [ "$mode" = node ] && [ "$(id -u)" != 0 ]; then die "a machine is installed as root (… | sudo sh); a hub for yourself is --hub"; fi
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

  # --- a hub for this user -------------------------------------------------------------------------------------------
  if [ "$mode" = hub ]; then
    if [ "$(id -u)" = 0 ]; then bin=/usr/local/bin; else bin=$HOME/.local/bin; fi
    mkdir -p "$bin"
    cp "$work/limen" "$bin/limen.tmp" && mv "$bin/limen.tmp" "$bin/limen"
    say "Installed $("$bin/limen" --version) in $bin/limen"
    "$bin/limen" init
    say ""
    say "Connect Claude Code:"
    say "  $("$bin/limen" connect)"
    say "Add a machine: limen invite <name>"
    case ":$PATH:" in *":$bin:"*) ;; *) say "(add $bin to your PATH)" ;; esac
    exit 0
  fi

  # --- a machine: how it joins ---------------------------------------------------------------------------------------
  LIMEN_HUB_KEY=${LIMEN_HUB_KEY:-${LIMEN_READ_KEY:-}}
  if [ -z "${LIMEN_JOIN:-}" ] && [ -z "$LIMEN_HUB_KEY" ]; then
    ask LIMEN_JOIN "The join line from the hub's \`limen invite\` (or the hub's public key)" ""
    case "$LIMEN_JOIN" in
      ssh-* | /*) LIMEN_HUB_KEY=$LIMEN_JOIN; LIMEN_JOIN= ;;
      "") die "a join line (LIMEN_JOIN) or the hub's key (LIMEN_HUB_KEY) is needed" ;;
    esac
  fi
  if [ -n "$LIMEN_HUB_KEY" ] && [ -z "${LIMEN_JOIN:-}" ]; then ask LIMEN_NAME "This machine's name on the hub" "$hostname"; fi

  # --- what is optional: the repository ------------------------------------------------------------------------------
  ask LIMEN_REPO "Repository this machine follows, e.g. https://github.com/you/infra.git (none to skip)" "none"
  if [ "$LIMEN_REPO" != none ]; then
    ask LIMEN_BRANCH "Its branch" "main"
    ask LIMEN_PATH "Its folder in the repository (empty: nodes/<its name on the hub>)" ""
    if ! command -v git > /dev/null 2>&1; then
      if [ "$openwrt" = 1 ]; then
        if command -v apk > /dev/null 2>&1; then install_git="apk update && apk add git-http"; else install_git="opkg update && opkg install git-http"; fi
      elif command -v apt-get > /dev/null 2>&1; then
        install_git="apt-get update && apt-get install -y git"
      else
        die "the repository needs git on this machine; install it and run this again"
      fi
      confirm "The repository needs git, which is not installed. Run '$install_git'?" || die "git is needed for the repository"
      sh -c "$install_git" || die "could not install git"
    fi
  fi

  # --- limen join ----------------------------------------------------------------------------------------------------
  if [ -n "${LIMEN_JOIN:-}" ]; then
    set -- join "$LIMEN_JOIN"
  else
    set -- join --hub-key "$(key "$LIMEN_HUB_KEY")" --name "${LIMEN_NAME:-$hostname}"
  fi
  [ -z "${LIMEN_DEPLOY_KEY:-}" ] || [ "$LIMEN_DEPLOY_KEY" = none ] || set -- "$@" --deploy-key "$(key "$LIMEN_DEPLOY_KEY")"
  [ -z "${LIMEN_FROM:-}" ] || set -- "$@" --from "$LIMEN_FROM"
  [ -z "${LIMEN_ADDRESS:-}" ] || set -- "$@" --address "$LIMEN_ADDRESS"
  [ -z "${LIMEN_SSH_PORT:-}" ] || set -- "$@" --ssh-port "$LIMEN_SSH_PORT"
  if [ "$LIMEN_REPO" != none ]; then
    set -- "$@" --repo "$LIMEN_REPO" --branch "${LIMEN_BRANCH:-main}"
    [ -z "${LIMEN_PATH:-}" ] || set -- "$@" --path "$LIMEN_PATH"
  fi
  say ""
  # `limen join` asks for the repository's token itself, hidden, when the repository needs one.
  if [ -n "${LIMEN_REPO_TOKEN_FILE:-}" ]; then
    [ -r "$LIMEN_REPO_TOKEN_FILE" ] || die "cannot read LIMEN_REPO_TOKEN_FILE ($LIMEN_REPO_TOKEN_FILE)"
    "$work/limen" "$@" < "$LIMEN_REPO_TOKEN_FILE"
  elif [ -n "${LIMEN_REPO_TOKEN:-}" ]; then
    printf '%s\n' "$LIMEN_REPO_TOKEN" | "$work/limen" "$@"
  elif [ "$interactive" = 1 ]; then
    "$work/limen" "$@" < /dev/tty
  else
    "$work/limen" "$@" < /dev/null
  fi
}

main "$@"
