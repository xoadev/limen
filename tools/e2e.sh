#!/usr/bin/env bash
# make e2e: the whole chain against real SSH servers, in throwaway containers. `limen install` sets each node up
# as root; the hub on this machine reaches it with the read key and the deploy key; every scene checks one promise
# of the spec. Not part of `make check`: it needs Docker and pulls images.
#
#   SUITE=debian    OpenSSH, sudo, one user per role, a repository the node follows
#   SUITE=openwrt   OpenWrt's own image: dropbear, root with forced commands, busybox, musl
#   (default: both)
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
command -v docker >/dev/null || { echo "e2e: needs Docker; skipped" >&2; exit 0; }
binary=$("$ROOT/tools/kt" artifact)
SUITE=${SUITE:-all}

work=$(mktemp -d)
containers=()
cleanup() {
  for c in "${containers[@]}"; do docker rm -f "$c" >/dev/null 2>&1 || true; done
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
# refuse <description> <needle> <command...>: the output must NOT contain <needle>.
refuse() {
  local what=$1 needle=$2 out
  shift 2
  scene "$what"
  out=$("$@" 2>&1) || true
  if [[ "$out" != *"$needle"* ]]; then ok; else ko "did not expect: $needle" "got: ${out:0:700}"; fi
}

ssh-keygen -q -t ed25519 -N '' -f "$work/read" -C limen-e2e-read
ssh-keygen -q -t ed25519 -N '' -f "$work/deploy" -C limen-e2e-deploy

# hub_config <node> <port> <user> <host key>: a hub home for one node, in $work/<node>.
hub_config() {
  mkdir -p "$work/$1"
  cp "$work/read" "$work/$1/read"
  cat > "$work/$1/limen.toml" <<EOF
[ssh]
identity = "read"

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

mcp_session() {
  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
    "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"$2\",\"arguments\":{\"node\":\"$1\"}}}" \
    | "$binary" mcp --home "$work/$1" 2>/dev/null
}

suite_debian() {
  local node=limen-e2e-debian-$$ port host_key
  echo "e2e/debian: node image"
  docker build -q -t limen-e2e-node -f "$ROOT/etc/e2e/node.Dockerfile" "$ROOT/etc/e2e" >/dev/null
  docker run -d --name "$node" -p 127.0.0.1::22 limen-e2e-node >/dev/null
  containers+=("$node")
  port=$(docker port "$node" 22/tcp | head -1 | sed 's/.*://')
  docker cp "$binary" "$node:/tmp/limen"
  docker cp "$ROOT/install.sh" "$node:/tmp/install.sh"
  docker cp "$work/read.pub" "$node:/tmp/read.pub"

  echo "e2e/debian: install"
  # Through install.sh, unattended, as dash runs it; the read key given as a file.
  expect "install.sh sets the node up" "limen is installed" \
    docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_READ_KEY=/tmp/read.pub \
    -e LIMEN_DEPLOY_KEY="$(cat "$work/deploy.pub")" -e LIMEN_REPO=none "$node" sh /tmp/install.sh
  refuse "install twice changes nothing" "write " \
    docker exec "$node" /tmp/limen install --read-key "$(cat "$work/read.pub")" --deploy-key "$(cat "$work/deploy.pub")"
  expect "sudo gives limen-read its gate and nothing else" "password is required" \
    docker exec -u limen-read "$node" sudo -n /bin/true

  # What the node lets read, a secret to redact, a check and two setup scripts.
  docker exec -i "$node" sh -c 'cat > /etc/limen/limen.toml' <<'EOF'
[files]
allow = ["/etc/hostname", "/etc/limen-e2e/**", "/var/log/e2e.log"]
deny = ["**/*.env"]
EOF
  docker exec "$node" sh -c 'mkdir -p /etc/limen-e2e && printf "user=app\npassword=hunter2\n" > /etc/limen-e2e/app.conf \
    && echo "TOKEN=x" > /etc/limen-e2e/app.env && ln -s /etc/shadow /etc/limen-e2e/shadow-link \
    && for i in $(seq 1 50); do echo "line $i"; done > /var/log/e2e.log && echo "ERROR disk full" >> /var/log/e2e.log'
  docker exec -i "$node" sh -c 'cat > /etc/limen/checks.d/disk.sh && chmod 0755 /etc/limen/checks.d/disk.sh' <<'EOF'
#!/bin/sh
#: description = "Root filesystem usage"
#: [args.threshold]
#: type = "int"
#: default = 99
#: range = [1, 100]
used=$(df --output=pcent / | tail -1 | tr -dc 0-9)
if [ "$used" -ge "$LIMEN_ARG_THRESHOLD" ]; then echo "root at ${used}%"; exit 1; fi
echo "root at ${used}%"
EOF
  docker exec -i "$node" sh -c 'cat > /etc/limen/setup.d/10-marker.sh && chmod 0755 /etc/limen/setup.d/10-marker.sh' <<'EOF'
#!/bin/sh
#: description = "Leaves a marker"
touch /var/tmp/limen-applied && echo "marker written"
EOF
  docker exec -i "$node" sh -c 'cat > /etc/limen/setup.d/20-noisy.sh && chmod 0755 /etc/limen/setup.d/20-noisy.sh' <<'EOF'
#!/bin/sh
#: description = "Writes to both streams"
echo "to stdout"; echo "to stderr" >&2
EOF

  host_key=$(docker exec "$node" cat /etc/ssh/ssh_host_ed25519_key.pub | cut -d' ' -f1,2)
  hub_config debian "$port" limen-read "$host_key"
  limen() { "$binary" "$@" --home "$work/debian"; }

  echo "e2e/debian: read role"
  expect "hello reports the catalog" '"disk"' limen call debian check_disk
  expect "status answers, disks from statvfs" '"mount": "/"' limen call debian status
  expect "an allowed file is read" '"content": "' limen call debian read_file --arg path=/etc/hostname
  expect "a secret in an allowed file is redacted" 'password=[redacted]' limen call debian read_file --arg path=/etc/limen-e2e/app.conf
  expect "a file outside the allowlist is denied" 'not in files.allow' limen call debian read_file --arg path=/etc/passwd
  expect "files.deny wins over files.allow" 'denied by files.deny' limen call debian read_file --arg path=/etc/limen-e2e/app.env
  expect "the built-in deny list can't be allowed" 'is never readable' limen call debian read_file --arg path=/etc/shadow
  expect "a symlink to a secret stays denied" 'is never readable' limen call debian read_file --arg path=/etc/limen-e2e/shadow-link
  expect "list_dir walks towards allowed files" '"name": "limen-e2e"' limen call debian list_dir --arg path=/etc
  expect "list_dir names owners without NSS" '"owner": "root"' limen call debian list_dir --arg path=/etc
  refuse "list_dir hides what is not allowed" '"passwd"' limen call debian list_dir --arg path=/etc
  expect "file logs with grep" 'ERROR disk full' limen call debian logs --arg source=file --arg name=/var/log/e2e.log --arg grep=error
  expect "a check runs with its default" '"status": "ok"' limen call debian check_disk
  expect "a check runs with an argument" '"status": "warn"' limen call debian check_disk --arg threshold=1
  expect "a bad argument never reaches the script" 'threshold must be at most 100' limen call debian check_disk --arg threshold=500
  expect "processes, from /proc" '"sshd' limen call debian processes
  expect "ports, from /proc: sshd on 22" '"port": 22' limen call debian ports
  expect "history records the client" '"client": "' limen call debian history --arg lines=3

  echo "e2e/debian: the gate is the only way in"
  read -ra read_ssh <<< "$(ssh_as read limen-read "$port")"
  expect "a command sent over ssh is ignored" 'no request on stdin' "${read_ssh[@]}" 'cat /etc/shadow' </dev/null
  expect "the read key can't apply" "not allowed for the read role" "${read_ssh[@]}" <<< '{"v":1,"request":"apply"}'
  expect "an unknown protocol version says so" '"versions":[1]' "${read_ssh[@]}" <<< '{"v":9,"request":"status"}'

  echo "e2e/debian: deploy role"
  expect "apply runs the setup scripts in order" "limen: apply finished: 2 script(s)" \
    limen call debian apply --user limen-deploy --identity "$work/deploy"
  expect "apply streams stderr too" "to stderr" \
    limen call debian apply --user limen-deploy --identity "$work/deploy" --arg from=20
  expect "the setup script ran as root" "root" docker exec "$node" stat -c %U /var/tmp/limen-applied
  docker exec -i "$node" sh -c 'cat > /etc/limen/actions.d/say.sh && chmod 0755 /etc/limen/actions.d/say.sh' <<'EOF'
#!/bin/sh
#: description = "Says a word"
#: [args.word]
#: type = "string"
echo "said $LIMEN_ARG_WORD"
EOF
  expect "an action with its own argument" "said hello" \
    limen call debian action --arg name=say --arg word=hello --user limen-deploy --identity "$work/deploy"
  expect "an action's argument is validated" "word does not match" \
    limen call debian action --arg name=say --arg "word=a b" --user limen-deploy --identity "$work/deploy"
  read -ra deploy_ssh <<< "$(ssh_as deploy limen-deploy "$port")"
  expect "the deploy key can't read" "not allowed for the deploy role" "${deploy_ssh[@]}" <<< '{"v":1,"request":"status"}'

  echo "e2e/debian: a repository"
  # A bare repository in the container, with this node's folder: a check, a setup script and node.toml.
  docker exec "$node" sh -c 'git init -q --bare -b main /srv/cloud.git && git init -q -b main /tmp/w \
    && mkdir -p /tmp/w/nodes/e2e/checks /tmp/w/nodes/e2e/setup \
    && printf "#!/bin/sh\n#: description = \"From the repository\"\necho repo check\n" > /tmp/w/nodes/e2e/checks/from-repo.sh \
    && printf "#!/bin/sh\n#: description = \"Repository setup\"\ntouch /var/tmp/repo-applied\n" > /tmp/w/nodes/e2e/setup/10-repo.sh \
    && chmod 0755 /tmp/w/nodes/e2e/checks/from-repo.sh /tmp/w/nodes/e2e/setup/10-repo.sh \
    && printf "[expect]\n" > /tmp/w/nodes/e2e/node.toml \
    && git -C /tmp/w add -A && git -C /tmp/w -c user.name=e2e -c user.email=e2e@e2e commit -qm first \
    && git -C /tmp/w push -q /srv/cloud.git main'
  expect "install --repo: readable without a token, checked out" "is readable without a token" \
    docker exec "$node" limen install --read-key "$(cat "$work/read.pub")" --deploy-key "$(cat "$work/deploy.pub")" \
    --repo file:///srv/cloud.git --path nodes/e2e
  expect "the node takes its checks from the repository" '"status": "ok"' limen call debian check_from-repo
  expect "apply syncs and runs the repository's setup" "limen: apply finished: 1 script(s), 0 stack(s)" \
    limen call debian apply --user limen-deploy --identity "$work/deploy"
  expect "state: deployed and up to date" '"up_to_date": true' limen call debian state
  docker exec "$node" sh -c 'echo two > /tmp/w/f && git -C /tmp/w add -A \
    && git -C /tmp/w -c user.name=e2e -c user.email=e2e@e2e commit -qm second && git -C /tmp/w push -q /srv/cloud.git main'
  expect "state: behind after a push" 'the node is behind main' limen call debian state
  expect "sync catches up" "<== sync:" limen call debian sync --user limen-deploy --identity "$work/deploy"
  expect "state: up to date again" '"up_to_date": true' limen call debian state

  echo "e2e/debian: hub"
  expect "mcp lists the node's check as a tool" '"name":"check_from-repo"' mcp_session debian check_from-repo
  expect "mcp calls it" 'repo check' mcp_session debian check_from-repo
  refuse "mcp exposes no action or apply" '"name":"apply"' mcp_session debian status
  sed -i "s|^host_key = .*|host_key = \"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOtherKeyOtherKeyOtherKeyOtherKeyOtherKey1\"|" "$work/debian/limen.toml"
  # A fresh control socket: a multiplexed connection would skip the host key check (AGENTS.md).
  XDG_RUNTIME_DIR="$work/rt-debian-2" && mkdir -m 0700 "$XDG_RUNTIME_DIR"
  expect "a host key that does not match is refused" "host_key_mismatch" limen call debian status

  echo "e2e/debian: uninstall"
  expect "uninstall removes the users" "limen is uninstalled" docker exec "$node" limen uninstall --purge
  expect "and the users are gone" "no such user" docker exec "$node" id limen-read
}

suite_openwrt() {
  local node=limen-e2e-openwrt-$$ port host_key
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
  # Someone already administers this router with their own key: limen must leave it alone.
  docker exec "$node" sh -c 'mkdir -p /etc/dropbear && echo "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAdminAdminAdminAdminAdminAdminAdminAdmin1 admin" > /etc/dropbear/authorized_keys'

  echo "e2e/openwrt: install"
  # Through install.sh, unattended, as busybox's ash runs it.
  expect "install.sh under ash: dropbear and root" 'user = "root"' \
    docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_READ_KEY="$(cat "$work/read.pub")" \
    -e LIMEN_DEPLOY_KEY="$(cat "$work/deploy.pub")" "$node" sh /tmp/install.sh
  expect "install warns: root without a password" "root has no password" \
    docker exec "$node" /tmp/limen install --read-key "$(cat "$work/read.pub")" --deploy-key "$(cat "$work/deploy.pub")"
  expect "the administrator's key is kept" "admin" docker exec "$node" cat /etc/dropbear/authorized_keys
  expect "sysupgrade keeps limen" "/usr/bin/limen" docker exec "$node" cat /lib/upgrade/keep.d/limen
  refuse "install twice changes nothing" "write " \
    docker exec "$node" /tmp/limen install --read-key "$(cat "$work/read.pub")" --deploy-key "$(cat "$work/deploy.pub")"
  expect "--from is refused: dropbear can't do it" "dropbear has no from=" \
    docker exec "$node" /tmp/limen install --read-key "$(cat "$work/read.pub")" --from 10.0.0.0/8

  docker exec -i "$node" sh -c 'cat > /etc/limen/limen.toml' <<'EOF'
[files]
allow = ["/etc/config/**", "/etc/openwrt_release"]
EOF
  docker exec -i "$node" sh -c 'cat > /etc/limen/setup.d/10-marker.sh && chmod 0755 /etc/limen/setup.d/10-marker.sh' <<'EOF'
#!/bin/sh
#: description = "Leaves a marker"
touch /tmp/limen-applied && echo "marker written"
EOF
  host_key=$(docker exec "$node" dropbearkey -y -f /etc/dropbear/dropbear_ed25519_host_key | grep '^ssh-ed25519' | cut -d' ' -f1,2)
  hub_config openwrt "$port" root "$host_key"
  limen() { "$binary" "$@" --home "$work/openwrt"; }

  echo "e2e/openwrt: read role"
  expect "hello: OpenWrt, procd" '"init": "procd"' limen call openwrt hello
  expect "hello: the OS" '"os": "OpenWrt' limen call openwrt hello
  expect "status: disks from statvfs" '"mount": "/"' limen call openwrt status
  expect "an allowed uci file is read" '"path": "/etc/config/' limen call openwrt read_file --arg path=/etc/config/dropbear
  expect "root's files are owned by root, without NSS" '"owner": "root"' limen call openwrt list_dir --arg path=/etc/config
  expect "processes, from /proc: dropbear" 'dropbear' limen call openwrt processes
  expect "ports, from /proc: dropbear on 22" '"name": "dropbear"' limen call openwrt ports
  expect "no logd in a container: unavailable, said so" 'unavailable' limen call openwrt logs --arg source=journal

  echo "e2e/openwrt: the gate is the only way in"
  read -ra read_ssh <<< "$(ssh_as read root "$port")"
  expect "a command sent over ssh is ignored" 'no request on stdin' "${read_ssh[@]}" 'cat /etc/shadow' </dev/null
  expect "the read key can't apply" "not allowed for the read role" "${read_ssh[@]}" <<< '{"v":1,"request":"apply"}'
  expect "apply with the deploy key" "limen: apply finished: 1 script(s)" \
    limen call openwrt apply --user root --identity "$work/deploy"
  expect "the setup script ran" "present" docker exec "$node" sh -c 'test -f /tmp/limen-applied && echo present'

  echo "e2e/openwrt: uninstall"
  expect "uninstall" "limen is uninstalled" docker exec "$node" limen uninstall --purge
  refuse "limen's keys are gone" "limen gate" docker exec "$node" cat /etc/dropbear/authorized_keys
  expect "the administrator's key is still there" "admin" docker exec "$node" cat /etc/dropbear/authorized_keys
}

case "$SUITE" in
  debian) suite_debian ;;
  openwrt) suite_openwrt ;;
  all)
    suite_debian
    suite_openwrt
    ;;
  *) echo "e2e: SUITE is debian, openwrt or all" >&2; exit 64 ;;
esac

echo
echo "e2e: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
