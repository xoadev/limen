#!/usr/bin/env bash
# make e2e: the whole chain against real SSH servers, in throwaway containers. `limen install` sets each node up
# as root; the hub on this machine reaches it with its key; every scene checks one promise of the spec. Not part of `make check`: it needs Docker and pulls images. CI runs it on pull requests that touch
# code (e2e.yml) and before publishing a release, with the release binary.
#
#   SUITE=debian    OpenSSH, sudo, the user limen, packs of scripts
#   SUITE=openwrt   OpenWrt's own image: dropbear, root with forced commands, busybox, musl
#   SUITE=join      the hub's image in a container; machines join it with `limen invite` and install.sh
#   (default: all three)
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
command -v docker >/dev/null || { echo "e2e: needs Docker; skipped" >&2; exit 0; }
binary=$("$ROOT/tools/cargo.sh" artifact)
SUITE=${SUITE:-all}

# Short: ssh's control sockets live under it, and a socket path can't pass 108 bytes.
work=$(mktemp -d /tmp/le.XXXXXX)
containers=()
networks=()
volumes=()
cleanup() {
  for c in "${containers[@]}"; do docker rm -f "$c" >/dev/null 2>&1 || true; done
  for n in "${networks[@]}"; do docker network rm "$n" >/dev/null 2>&1 || true; done
  for v in "${volumes[@]}"; do docker volume rm "$v" >/dev/null 2>&1 || true; done
  rm -rf "$work"
}
trap cleanup EXIT
# Control sockets of this run live here, not with the hub of whoever runs it.
export XDG_RUNTIME_DIR="$work/rt"
mkdir -m 0700 "$XDG_RUNTIME_DIR"

passed=0
failed=0
scene() { printf '%-64s' "$1"; }
ok() {
  echo "ok"
  passed=$((passed + 1))
}
ko() {
  echo "FAIL"
  printf '    %s\n' "$@" >&2
  failed=$((failed + 1))
}
# expect <description> <needle> <command...>: the command's output (stdout and stderr) contains <needle>.
expect() {
  local what=$1 needle=$2 out
  shift 2
  scene "$what"
  out=$("$@" 2>&1) || true
  if [[ "$out" == *"$needle"* ]]; then ok; else ko "expected: $needle" "got: ${out:0:700}"; fi
}
# refuse <description> <needle> <absent> <command...>: the output contains <needle> and nothing matching the
# extended regex <absent>. The needle proves the command answered: an empty output lacks everything.
refuse() {
  local what=$1 needle=$2 absent=$3 out
  shift 3
  scene "$what"
  out=$("$@" 2>&1) || true
  if [[ "$out" != *"$needle"* ]]; then
    ko "expected: $needle" "got: ${out:0:700}"
  elif grep -Eq -- "$absent" <<< "$out"; then
    ko "did not expect: $absent" "got: ${out:0:700}"
  else
    ok
  fi
}

ssh-keygen -q -t ed25519 -N '' -f "$work/hub" -C limen-e2e-hub
ssh-keygen -q -t ed25519 -N '' -f "$work/stranger" -C limen-e2e-stranger

# hub_config <node> <port> <user> <host key>: a hub home for one node, in $work/<node>.
hub_config() {
  mkdir -p "$work/$1"
  cp "$work/hub" "$work/$1/hub"
  cat > "$work/$1/limen.toml" <<EOF
[ssh]
identity = "hub"

[nodes.$1]
host = "127.0.0.1"
port = $2
user = "$3"
host_key = "$4"
EOF
}

# ssh_as <key> <user> <port>: an ssh command line that trusts any host key, for the scenes that go around the hub.
ssh_as() {
  echo ssh -T -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o LogLevel=ERROR -i "$work/$1" -p "$3" "$2@127.0.0.1"
}

# mcp_session <node> <tool> [<arguments as JSON, without the node>]: initialize, tools/list and one call, over stdio.
mcp_session() {
  local arguments="{\"node\":\"$1\"${3:+,$3}}"
  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
    "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"$2\",\"arguments\":$arguments}}" \
    | "$binary" mcp --home "$work/$1" 2>/dev/null
}
# approval_session <node> <client capabilities as JSON> [<answer as JSON>]: calls `mark` over stdio and, a few seconds
# later, answers the hub's first question with <answer>, as a person at the client would.
approval_session() {
  {
    printf '%s\n' \
      "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":$2}}" \
      '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
      "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"mark\",\"arguments\":{\"node\":\"$1\",\"word\":\"approved\"}}}"
    sleep 4
    [ -z "${3:-}" ] || printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":\"limen-approval-0\",\"result\":$3}"
  } | "$binary" mcp --home "$work/$1" 2>/dev/null
}
# annotations <node> <tool>: `<tool> <its annotations as JSON>`, from the tools/list of an MCP session with <node>.
annotations() {
  mcp_session "$1" disk | jq -c --arg tool "$2" 'select(.id == 2) | .result.tools[] | select(.name == $tool) | "\(.name) \(.annotations)"'
}

# script <container> <path> <mode>: the script on stdin, written in the container as root with that mode.
script() {
  docker exec -i "$1" sh -c "mkdir -p \"\$(dirname '$2')\" && cat > '$2' && chmod $3 '$2'"
}

# e2e_pack <container> <dir>: the scripts the scenes run, in a pack at <dir>.
e2e_pack() {
  script "$1" "$2/disk.sh" 0755 <<'EOF'
#!/bin/sh
#: description = "Root filesystem usage"
#: [args.threshold]
#: type = "int"
#: default = 100
#: range = [1, 100]
used=$(df -P / | tail -1 | awk '{print $5}' | tr -dc 0-9)
echo "root at ${used}%, threshold ${LIMEN_ARG_THRESHOLD}%"
[ "$used" -lt "$LIMEN_ARG_THRESHOLD" ] || exit 1
EOF
  script "$1" "$2/where" 0755 <<'EOF'
#!/bin/sh
#: description = "Where it runs"
echo "pack $LIMEN_PACK node $LIMEN_NODE user $(id -un) cwd $(pwd)"
echo "a secret password=hunter2"
echo "to stderr" >&2
EOF
  script "$1" "$2/mark" 0755 <<'EOF'
#!/bin/sh
#: description = "Changes the machine: leaves a marker"
#: [args.word]
#: type = "string"
touch /tmp/limen-marked && echo "marked $LIMEN_ARG_WORD"
EOF
  script "$1" "$2/slow" 0755 <<'EOF'
#!/bin/sh
#: description = "Never finishes in time"
#: timeout = "1s"
sleep 30
EOF
  script "$1" "$2/helper.sh" 0755 <<'EOF'
#!/bin/sh
echo "a helper: no header, no tool"
EOF
  script "$1" "$2/README.md" 0644 <<'EOF'
# The scenes' pack
EOF
}

suite_debian() {
  local node=limen-e2e-debian-$$ port host_key pack=/opt/state/packs/e2e
  echo "e2e/debian: node image"
  docker build -q -t limen-e2e-node -f "$ROOT/etc/e2e/node.Dockerfile" "$ROOT/etc/e2e" >/dev/null
  docker run -d --name "$node" -p 127.0.0.1::22 limen-e2e-node >/dev/null
  containers+=("$node")
  port=$(docker port "$node" 22/tcp | head -1 | sed 's/.*://')
  docker cp "$binary" "$node:/tmp/limen"
  docker cp "$ROOT/install.sh" "$node:/tmp/install.sh"
  docker cp "$work/hub.pub" "$node:/tmp/hub.pub"
  docker exec "$node" mkdir -p /opt/state
  docker cp "$ROOT/packs" "$node:/opt/state/"
  # docker cp keeps the owner and mode of this checkout: root's and not group-writable, as an operator's setup leaves
  # them.
  docker exec "$node" sh -c 'chown -R root:root /opt/state && chmod -R go-w /opt/state'

  echo "e2e/debian: install"
  # Through install.sh, unattended, as dash runs it; the hub's key given as a file.
  expect "install.sh sets the node up" "limen is installed" \
    docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_HUB_KEY=/tmp/hub.pub -e LIMEN_NAME=debian \
    "$node" sh /tmp/install.sh
  refuse "install twice changes nothing" "limen is installed" "^(write|create|add|remove|run) " \
    docker exec "$node" /tmp/limen install --hub-key "$(cat "$work/hub.pub")"
  expect "sudo gives limen its gate and nothing else" "password is required" \
    docker exec -u limen "$node" sudo -n /bin/true

  # What the node lets read, and the packs it offers: the scenes' own and the example `system`.
  docker exec -i "$node" sh -c 'cat > /etc/limen/kept.toml' <<EOF
[files]
allow = ["/etc/hostname", "/etc/limen-e2e/**", "/var/log/e2e.log"]
deny = ["**/*.env"]

[scripts]
packs = ["$pack", "/opt/state/packs/system"]

[redact]
names = ["MQTT_PASS"]

[limits]
max_lines = 100
EOF
  # limen.toml as a link to a file kept elsewhere, as when it comes from a repository: read through it.
  docker exec "$node" sh -c 'ln -sf kept.toml /etc/limen/limen.toml'
  docker exec "$node" sh -c 'mkdir -p /etc/limen-e2e && printf "user=app\npassword=hunter2\nMQTT_PASS=s3cr3t\n" > /etc/limen-e2e/app.conf \
    && echo "TOKEN=x" > /etc/limen-e2e/app.env && ln -s /etc/shadow /etc/limen-e2e/shadow-link \
    && { seq 1 50 | sed "s/^/line /"; echo "ERROR disk full"; seq 52 110 | sed "s/^/line /"; } > /var/log/e2e.log \
    && printf "a\\000b" > /etc/limen-e2e/blob'
  e2e_pack "$node" "$pack"
  script "$node" "$pack/loose" 0775 <<'EOF'
#!/bin/sh
#: description = "Group-writable: anyone in the group could change what root runs"
echo ran
EOF
  for ext in sh py; do
    script "$node" "$pack/twice.$ext" 0755 <<'EOF'
#!/bin/sh
#: description = "One name, two files"
echo which
EOF
  done

  host_key=$(docker exec "$node" cat /etc/ssh/ssh_host_ed25519_key.pub | cut -d' ' -f1,2)
  hub_config debian "$port" limen "$host_key"
  limen() { "$binary" "$@" --home "$work/debian"; }

  echo "e2e/debian: files"
  expect "hello reports the catalog" '"name": "disk"' limen call debian hello
  expect "hello reports a script it won't run, and why" 'loose is writable by group or others' limen call debian hello
  refuse "a file without a header is not a script" '"name": "disk"' '"name": "helper"' limen call debian hello
  expect "an allowed file is read" '"content": "' limen call debian read_file --arg path=/etc/hostname
  expect "a secret in an allowed file is redacted" 'password=[redacted]' limen call debian read_file --arg path=/etc/limen-e2e/app.conf
  expect "and so is a name of redact.names" 'MQTT_PASS=[redacted]' limen call debian read_file --arg path=/etc/limen-e2e/app.conf
  expect "a file outside the allowlist is denied" '/etc/passwd is not readable' limen call debian read_file --arg path=/etc/passwd
  expect "files.deny wins over files.allow" 'app.env is not readable' limen call debian read_file --arg path=/etc/limen-e2e/app.env
  expect "the built-in deny list can't be allowed" '/etc/shadow is not readable' limen call debian read_file --arg path=/etc/shadow
  expect "a symlink to a secret stays denied" 'shadow-link is not readable' limen call debian read_file --arg path=/etc/limen-e2e/shadow-link
  expect "list_dir walks towards allowed files" '"name": "limen-e2e"' limen call debian list_dir --arg path=/etc
  expect "list_dir names owners without NSS" '"owner": "root"' limen call debian list_dir --arg path=/etc
  refuse "list_dir hides what is not allowed" '"name": "limen-e2e"' '"(passwd|shadow|limen)"' limen call debian list_dir --arg path=/etc
  expect "a denied path that doesn't exist is denied too" '/root/nothing is not readable' limen call debian read_file --arg path=/root/nothing
  expect "a denied path is denied before its type is told" '"denied"' limen call debian list_dir --arg path=/etc/shadow
  expect "dots are walked where links lead, not as text" '/var/run/../etc/shadowX is not readable' \
    limen call debian read_file --arg path=/var/run/../etc/shadowX
  expect "a binary file answers its size, no content" '"binary": true' limen call debian read_file --arg path=/etc/limen-e2e/blob
  expect "tail: the last lines" '"line 110"' limen call debian read_file --arg path=/var/log/e2e.log --tail 5
  refuse "tail: only the lines asked for" '"line 110"' 'ERROR' limen call debian read_file --arg path=/var/log/e2e.log --tail 5
  expect "grep searches the whole end of the file" 'ERROR disk full' limen call debian read_file --arg path=/var/log/e2e.log --grep error
  expect "more lines than limits.max_lines: cut, and said so" '"truncated": true' \
    limen call debian read_file --arg path=/var/log/e2e.log --tail 5000
  expect "a range and a tail at once is refused" 'not both' limen call debian read_file --arg path=/var/log/e2e.log --arg from=1 --tail 5

  echo "e2e/debian: scripts"
  expect "a script runs with its default" 'threshold 100%' limen call debian disk
  expect "an argument reaches the script" 'threshold 1%' limen call debian disk --arg threshold=1
  expect "its exit code comes back" '"exit": 1' limen call debian disk --arg threshold=1
  expect "a bad argument never reaches the script" 'threshold must be at most 100' limen call debian disk --arg threshold=500
  expect "it runs as root, from /, with its pack and node" "pack $pack node " limen call debian where
  expect "and as root" "user root cwd /" limen call debian where
  expect "its output is redacted" 'password=[redacted]' limen call debian where
  expect "stderr comes back apart" '"stderr": "to stderr"' limen call debian where
  refuse "grep sees only what redaction left" '"exit": 0' 'secret' limen call debian where --grep password=h
  expect "a script that changes the machine" 'marked here' limen call debian mark --arg word=here
  expect "it did" "present" docker exec "$node" sh -c 'test -f /tmp/limen-marked && echo present'
  expect "a string argument can't be an option" 'word does not match' limen call debian mark --arg word=-rf
  expect "a script past its timeout is stopped" '"code": "timeout"' limen call debian slow
  expect "a group-writable script is not run" 'writable by group or others' limen call debian loose
  expect "two files with one script name: neither runs" "are both 'twice'" limen call debian twice
  expect "and lint says so" "are both 'twice'" docker exec "$node" limen lint
  expect "limen run on the node exits with the script's code" "exit 1" \
    docker exec "$node" sh -c 'limen run disk --arg threshold=1 >/dev/null; echo "exit $?"'
  expect "an example pack runs: system's processes" 'sshd' limen call debian processes
  expect "system's network: the default route" 'default route: via' limen call debian network
  expect "system's filesystems: space and inodes" 'MOUNT' limen call debian filesystems
  expect "system's time: the clock and its zone" 'time zone' limen call debian time
  expect "system's reach: a port that listens" 'port 22: open' limen call debian reach --arg host=127.0.0.1 --arg port=22
  expect "and one that doesn't" 'port 9: closed' limen call debian reach --arg host=127.0.0.1 --arg port=9
  expect "a host can't be an option" 'host does not match' limen call debian reach --arg host=-c
  expect "history records the client" '"client": "' limen call debian history --arg lines=3
  expect "and a script's exit code" '"exit": 1' limen call debian history --arg lines=40
  docker exec "$node" sh -c 'cp /etc/limen/kept.toml /tmp/kept.toml && printf "max_response = 300\n" >> /etc/limen/kept.toml'
  expect "an answer over limits.max_response is refused" 'over limits.max_response' limen call debian hello
  docker exec "$node" cp /tmp/kept.toml /etc/limen/kept.toml

  echo "e2e/debian: the gate is the only way in"
  read -ra hub_ssh <<< "$(ssh_as hub limen "$port")"
  expect "a command sent over ssh is ignored" 'no request on stdin' "${hub_ssh[@]}" 'cat /etc/shadow' </dev/null
  expect "a request limen doesn't know is refused" "unknown request 'apply'" "${hub_ssh[@]}" <<< '{"v":1,"request":"apply"}'
  expect "an unknown protocol version says so" '"versions":[1]' "${hub_ssh[@]}" <<< '{"v":9,"request":"hello"}'
  expect "and is in the audit log too" '"result": "unsupported_version"' limen call debian history --arg lines=5
  expect "a field nobody reads is an error" 'bad_request' "${hub_ssh[@]}" <<< '{"v":1,"request":"hello","role":"deploy"}'
  expect "a flood of arguments is refused" 'bad_request' \
    "${hub_ssh[@]}" <<< "{\"v\":1,\"request\":\"hello\",\"args\":{\"x\":\"$(head -c 200000 /dev/zero | tr '\0' x)\"}}"
  expect "and not copied into the audit log" '"omitted_bytes": ' limen call debian history --arg lines=3
  expect "no forwarding through the gate" 'stdio forwarding failed' "${hub_ssh[0]}" -W 127.0.0.1:22 "${hub_ssh[@]:1}" </dev/null
  read -ra stranger_ssh <<< "$(ssh_as stranger limen "$port")"
  expect "an unknown key doesn't get in" 'Permission denied' "${stranger_ssh[@]}" <<< '{"v":1,"request":"hello"}'
  docker exec "$node" limen install --hub-key "$(cat "$work/hub.pub")" --from 10.99.0.0/16 >/dev/null
  expect "--from: the key only from there" 'Permission denied' "${hub_ssh[@]}" <<< '{"v":1,"request":"hello"}'
  docker exec "$node" limen install --hub-key "$(cat "$work/hub.pub")" >/dev/null
  expect "install without --from lifts it" '"ok":true' "${hub_ssh[@]}" <<< '{"v":1,"request":"hello"}'

  echo "e2e/debian: hub"
  expect "mcp lists every script as a tool" '"name":"mark"' mcp_session debian disk
  expect "mcp calls one" 'threshold 100%' mcp_session debian disk
  expect "with its arguments and filters" 'marked mcp' mcp_session debian mark '"word":"mcp","tail":1'
  expect "files are read-only tools" 'read_file {\"readOnlyHint\":true' annotations debian read_file
  expect "so is a script whose header says read_only" 'status {\"readOnlyHint\":true' annotations debian status
  printf '\n[approval]\nscripts = ["mark"]\ntimeout = "20s"\n' >> "$work/debian/limen.toml"
  expect "a change waits for a person's yes" 'marked approved' \
    approval_session debian '{"elicitation":{}}' '{"action":"accept","content":{"approve":true}}'
  expect "the question shows what would run" 'run the script mark on debian, with arguments: {\"word\":\"approved\"}' \
    approval_session debian '{"elicitation":{}}' '{"action":"decline"}'
  refuse "a no runs nothing" 'not approved: the person said no' 'marked approved' \
    approval_session debian '{"elicitation":{}}' '{"action":"decline"}'
  refuse "a client that can't ask runs nothing" 'declares no elicitation' 'marked approved' \
    approval_session debian '{}'
  sed -i '/^\[approval\]/,$d' "$work/debian/limen.toml"
  printf '[approval]\nscripts = "changes"\nexcept = ["mark"]\n' >> "$work/debian/limen.toml"
  expect "a change excepted runs without asking" 'marked approved' approval_session debian '{}'
  sed -i '/^\[approval\]/,$d' "$work/debian/limen.toml"
  expect "one without it may change the machine" 'mark {\"readOnlyHint\":false,\"destructiveHint\":true}' annotations debian mark
  # Someone else's host key: a real one, so ssh refuses it for not matching and for nothing else.
  sed -i "s|^host_key = .*|host_key = \"$(cut -d' ' -f1,2 "$work/stranger.pub")\"|" "$work/debian/limen.toml"
  # A fresh control socket: a multiplexed connection would skip the host key check (AGENTS.md).
  XDG_RUNTIME_DIR="$work/rt-debian-2" && mkdir -m 0700 "$XDG_RUNTIME_DIR"
  expect "a host key that does not match is refused" "host_key_mismatch" limen call debian hello

  echo "e2e/debian: uninstall"
  expect "uninstall removes the user" "limen is uninstalled" docker exec "$node" limen uninstall --purge
  expect "and the user is gone" "no such user" docker exec "$node" id limen

  echo "e2e/debian: configuration and packs before a hub"
  # As an operator's own setup does: limen.toml a link into a checkout, there before limen is.
  docker exec "$node" sh -c 'mkdir -p /etc/limen && ln -s /opt/state/limen.toml /etc/limen/limen.toml'
  docker exec -i "$node" sh -c 'cat > /opt/state/limen.toml' <<EOF
[scripts]
packs = ["$pack"]
EOF
  expect "install with no hub key" "with no hub yet" docker exec "$node" /tmp/limen install
  expect "leaves the linked limen.toml alone" "/opt/state/limen.toml" docker exec "$node" readlink /etc/limen/limen.toml
  read -ra hub_ssh <<< "$(ssh_as hub limen "$port")"
  expect "and nothing a hub could log in with" 'Permission denied' "${hub_ssh[@]}" <<< '{"v":1,"request":"hello"}'
  expect "its scripts run on the machine itself" "marked early" docker exec "$node" limen run mark --arg word=early
  expect "a hub joins later" "limen is installed" docker exec "$node" limen install --hub-key "$(cat "$work/hub.pub")"
  expect "and gets in" '"ok":true' "${hub_ssh[@]}" <<< '{"v":1,"request":"hello"}'
}

suite_openwrt() {
  local node=limen-e2e-openwrt-$$ port host_key pack=/opt/state/packs/e2e
  echo "e2e/openwrt: node image"
  # The host key first, as OpenWrt's init script does at first boot: dropbear -R would only make it on a connection.
  # Password logins off (-s), as on a router set up with keys: the image's root has no password, and with them on
  # dropbear lets anyone in without a key, forced commands and all (install warns about it).
  docker run -d --name "$node" -p 127.0.0.1::22 openwrt/rootfs:x86-64 sh -c \
    'mkdir -p /etc/dropbear && dropbearkey -t ed25519 -f /etc/dropbear/dropbear_ed25519_host_key >/dev/null && exec /usr/sbin/dropbear -F -E -s -p 22' \
    >/dev/null
  containers+=("$node")
  port=$(docker port "$node" 22/tcp | head -1 | sed 's/.*://')
  docker cp "$binary" "$node:/tmp/limen"
  docker cp "$ROOT/install.sh" "$node:/tmp/install.sh"
  docker exec "$node" mkdir -p /opt/state
  docker cp "$ROOT/packs" "$node:/opt/state/"
  docker exec "$node" sh -c 'chown -R root:root /opt/state && chmod -R go-w /opt/state'
  # Someone already administers this router with their own key: limen must leave it alone.
  docker exec "$node" sh -c 'mkdir -p /etc/dropbear && echo "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAdminAdminAdminAdminAdminAdminAdminAdmin1 admin" > /etc/dropbear/authorized_keys'

  echo "e2e/openwrt: install"
  # Through install.sh, unattended, as busybox's ash runs it.
  expect "install.sh under ash: dropbear and root" '--user root' \
    docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_HUB_KEY="$(cat "$work/hub.pub")" -e LIMEN_NAME=openwrt \
    "$node" sh /tmp/install.sh
  expect "install warns: root without a password" "root has no password" \
    docker exec "$node" /tmp/limen install --hub-key "$(cat "$work/hub.pub")"
  expect "the administrator's key is kept" "admin" docker exec "$node" cat /etc/dropbear/authorized_keys
  expect "sysupgrade keeps limen" "/usr/bin/limen" docker exec "$node" cat /lib/upgrade/keep.d/limen
  refuse "install twice changes nothing" "limen is installed" "^(write|create|add|remove|run) " \
    docker exec "$node" /tmp/limen install --hub-key "$(cat "$work/hub.pub")"
  expect "--from is refused: dropbear can't do it" "dropbear has no from=" \
    docker exec "$node" /tmp/limen install --hub-key "$(cat "$work/hub.pub")" --from 10.0.0.0/8

  docker exec -i "$node" sh -c 'cat > /etc/limen/limen.toml' <<EOF
[files]
allow = ["/etc/config/**", "/etc/openwrt_release"]

[scripts]
packs = ["$pack", "/opt/state/packs/system", "/opt/state/packs/openwrt"]
EOF
  e2e_pack "$node" "$pack"
  host_key=$(docker exec "$node" dropbearkey -y -f /etc/dropbear/dropbear_ed25519_host_key | grep '^ssh-ed25519' | cut -d' ' -f1,2)
  hub_config openwrt "$port" root "$host_key"
  limen() { "$binary" "$@" --home "$work/openwrt"; }

  echo "e2e/openwrt: files and scripts"
  expect "hello: the OS" '"os": "OpenWrt' limen call openwrt hello
  expect "an allowed uci file is read" '"path": "/etc/config/' limen call openwrt read_file --arg path=/etc/config/dropbear
  expect "the Wi-Fi keys are never read" 'wireless is not readable' limen call openwrt read_file --arg path=/etc/config/wireless
  expect "root's files are owned by root, without NSS" '"owner": "root"' limen call openwrt list_dir --arg path=/etc/config
  expect "a script runs under busybox" 'user root cwd /' limen call openwrt where
  expect "and one changes the machine" 'marked router' limen call openwrt mark --arg word=router
  expect "it did" "present" docker exec "$node" sh -c 'test -f /tmp/limen-marked && echo present'
  # The system pack under busybox: its ip, df, awk and ping, and no getent.
  expect "system's network under busybox" 'default route: via' limen call openwrt network
  expect "system's filesystems under busybox" 'MOUNT' limen call openwrt filesystems
  expect "system's time: sysntpd" 'sysntpd: ' limen call openwrt time
  expect "system's reach: ping, a name from /etc/hosts" 'localhost answers ping' limen call openwrt reach --arg host=localhost
  expect "openwrt's dhcp_leases without dnsmasq running" 'no IPv4 leases' limen call openwrt dhcp_leases

  echo "e2e/openwrt: the gate is the only way in"
  read -ra hub_ssh <<< "$(ssh_as hub root "$port")"
  expect "a command sent over ssh is ignored" 'no request on stdin' "${hub_ssh[@]}" 'cat /etc/shadow' </dev/null
  read -ra stranger_ssh <<< "$(ssh_as stranger root "$port")"
  expect "an unknown key doesn't get in" 'Permission denied' "${stranger_ssh[@]}" <<< '{"v":1,"request":"hello"}'

  echo "e2e/openwrt: uninstall"
  expect "uninstall" "limen is uninstalled" docker exec "$node" limen uninstall --purge
  refuse "limen's key is gone, the administrator's kept" "admin" "limen gate" docker exec "$node" cat /etc/dropbear/authorized_keys
}

suite_join() {
  local net=limen-e2e-$$ data=limen-e2e-hub-$$ hub=limen-e2e-hub-$$ hub_ip url token line
  local nas=limen-e2e-nas-$$ router=limen-e2e-router-$$ spare=limen-e2e-spare-$$ key trust spare_ip tampered
  echo "e2e/join: the hub's image"
  IMAGE=limen-e2e-hub:local "$ROOT/tools/docker.sh" >/dev/null
  docker build -q -t limen-e2e-node -f "$ROOT/etc/e2e/node.Dockerfile" "$ROOT/etc/e2e" >/dev/null
  docker network create "$net" >/dev/null
  networks+=("$net")
  docker volume create "$data" >/dev/null
  volumes+=("$data")
  docker run -d --name "$hub" --network "$net" -v "$data:/data" limen-e2e-hub:local >/dev/null
  containers+=("$hub")
  hub_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$hub")
  url="http://$hub_ip:7341"
  for _ in $(seq 1 50); do docker logs "$hub" 2>&1 | grep -q "serving MCP" && break; sleep 0.2; done
  docker logs "$hub" 2>&1 | grep -q "serving MCP" || { docker logs "$hub" >&2; echo "e2e/join: the hub did not start" >&2; exit 1; }
  hub_exec() { docker exec -e LIMEN_PUBLIC_URL="$url" "$hub" limen "$@"; }
  # The image is distroless: no shell and no cat in it, so its files come out through docker cp.
  hub_file() { docker cp "$hub:/data/$1" - | tar -xO; }

  expect "the hub creates itself on first start" "created /data/id_ed25519" docker logs "$hub"
  expect "the image is distroless: no shell in it" "no shell" \
    sh -c "docker exec '$hub' /bin/sh -c true 2>/dev/null || echo no shell"
  expect "and it runs as distroless's nonroot" "65532" docker inspect -f '{{.Config.User}}' "$hub"
  expect "connect: the line for an MCP client, with the token" "Authorization: Bearer" hub_exec connect
  token=$(hub_exec connect | sed -n 's/.*Bearer \([a-z2-7]*\)".*/\1/p')

  # Machines on the hub's network, with their SSH servers running.
  docker run -d --name "$nas" --network "$net" limen-e2e-node >/dev/null
  docker run -d --name "$spare" --network "$net" limen-e2e-node >/dev/null
  docker run -d --name "$router" --network "$net" openwrt/rootfs:x86-64 sh -c \
    'mkdir -p /etc/dropbear && dropbearkey -t ed25519 -f /etc/dropbear/dropbear_ed25519_host_key >/dev/null && exec /usr/sbin/dropbear -F -E -s -p 22' \
    >/dev/null
  containers+=("$nas" "$spare" "$router")
  for c in "$nas" "$spare" "$router"; do
    docker cp "$binary" "$c:/tmp/limen"
    docker cp "$ROOT/install.sh" "$c:/tmp/install.sh"
  done
  join_line() { hub_exec invite "$1" | sed -n "s/.*--join '\([^']*\)'.*/\1/p" | head -n 1; }

  echo "e2e/join: a machine joins with the invitation"
  line=$(join_line nas)
  expect "invite prints a line with the key's fingerprint" "#SHA256:" echo "$line"
  tampered="${line%#*}#SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.${line##*.}"
  expect "a line with another fingerprint is refused" "is not the one the join line names" \
    docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_JOIN="$tampered" "$nas" sh /tmp/install.sh
  expect "and nothing was installed" "no such user" docker exec "$nas" id limen
  expect "install.sh --join: the machine is on the hub" "nas is on the hub" \
    docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_JOIN="$line" "$nas" sh /tmp/install.sh
  expect "an invitation works once" "was used, or expired" \
    docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_JOIN="$line" "$nas" sh /tmp/install.sh
  expect "the hub reaches it, no restart" '"ok": true' hub_exec call nas hello
  expect "the hub wrote it into limen.toml" "[nodes.nas]" hub_file limen.toml

  echo "e2e/join: OpenWrt joins the same way, with its address given as a host name"
  line=$(join_line router)
  expect "install.sh --join --address under ash" "router is on the hub, at $router" \
    docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen "$router" sh /tmp/install.sh --join "$line" --address "$router"
  expect "the hub files the address given, not the request's" "host = \"$router\"" hub_file limen.toml
  expect "the hub logs into it as root" 'user = "root"' hub_file limen.toml
  expect "and sees OpenWrt" '"os": "OpenWrt' hub_exec call router hello

  echo "e2e/join: without a hub on HTTP, --hub-key and limen trust"
  key=$(hub_file id_ed25519.pub)
  trust=$(docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_HUB_KEY="$key" -e LIMEN_NAME=spare "$spare" sh /tmp/install.sh --ssh-port 2222 | grep 'limen trust')
  expect "the machine prints the line for the hub" "limen trust spare <address>" echo "$trust"
  expect "install.sh --ssh-port: the port goes in it" "--port 2222" echo "$trust"
  spare_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$spare")
  trust=$(docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen "$spare" sh /tmp/install.sh --hub-key "$key" --name spare --address "$spare_ip" --from "$hub_ip" | grep 'limen trust')
  expect "install.sh --address: the address goes in it" "limen trust spare $spare_ip '" echo "$trust"
  expect "install.sh --from: the key, only from the hub" "from=\"$hub_ip\"" docker exec "$spare" cat /var/lib/limen/.ssh/authorized_keys
  read -ra trust_args <<< "$(echo "$trust" | sed "s/^ *limen trust //" | tr -d "'")"
  expect "limen trust on the hub" "spare added" docker exec "$hub" limen trust "${trust_args[0]}" "${trust_args[1]}" "${trust_args[2]} ${trust_args[3]}"
  expect "the hub reaches it" '"ok": true' hub_exec call spare hello

  echo "e2e/join: MCP over HTTP"
  mcp_http() {
    curl -s -X POST -H "Authorization: Bearer $token" -H 'Content-Type: application/json' "$url/mcp" -d "$1"
  }
  expect "tools/list offers the nodes that joined" '"enum":["nas","router","spare"]' \
    mcp_http '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
  expect "a tool call reaches a node" 'duration_ms' \
    mcp_http '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"history","arguments":{"node":"nas"}}}'
  # Text both ways with more than ASCII: the hub's server must keep UTF-8 whole, in the request and in the answer.
  expect "UTF-8 in and out of the server" 'unknown tool ñandú' \
    mcp_http '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ñandú","arguments":{}}}'
  status_of() { curl -s -o /dev/null -w '%{http_code}' -X POST "$url/mcp" -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' "$@"; }
  expect "without the token, nothing" "401" status_of
  expect "another token, nothing" "401" status_of -H "Authorization: Bearer ${token}x"
  expect "the token, but not as a bearer, nothing" "401" status_of -H "Authorization: $token"
  expect "a web page on another origin, nothing" "403" status_of -H "Authorization: Bearer $token" -H "Origin: http://evil.example"
  expect "a body without its length, nothing" "411" status_of -H "Authorization: Bearer $token" -H 'Transfer-Encoding: chunked'
  expect "a hub won't serve with a short LIMEN_TOKEN" "16 characters" \
    docker run --rm -e LIMEN_TOKEN=short limen-e2e-hub:local
  expect "forget takes a node off the hub" "removed" hub_exec forget spare
  expect "and it is gone" '"enum":["nas","router"]' mcp_http '{"jsonrpc":"2.0","id":3,"method":"tools/list"}'
}

case "$SUITE" in
  debian) suite_debian ;;
  openwrt) suite_openwrt ;;
  join) suite_join ;;
  all)
    suite_debian
    suite_openwrt
    suite_join
    ;;
  *) echo "e2e: SUITE is debian, openwrt, join or all" >&2; exit 64 ;;
esac

echo
echo "e2e: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
