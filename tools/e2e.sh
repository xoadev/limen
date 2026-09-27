#!/usr/bin/env bash
# make e2e: the whole chain against real SSH servers, in throwaway containers. `limen install` sets each node up
# as root; the hub on this machine reaches it with the read key and the deploy key; every scene checks one promise
# of the spec. Not part of `make check`: it needs Docker and pulls images. CI runs it on pull requests that touch
# code (e2e.yml) and before publishing a release, with the release binary.
#
#   SUITE=debian    OpenSSH, sudo, one user per role, a repository the node follows
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

ssh-keygen -q -t ed25519 -N '' -f "$work/read" -C limen-e2e-read
ssh-keygen -q -t ed25519 -N '' -f "$work/deploy" -C limen-e2e-deploy
ssh-keygen -q -t ed25519 -N '' -f "$work/stranger" -C limen-e2e-stranger

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
  refuse "install twice changes nothing" "limen is installed" "^(write|create|add|remove|run) " \
    docker exec "$node" /tmp/limen install --read-key "$(cat "$work/read.pub")" --deploy-key "$(cat "$work/deploy.pub")"
  expect "sudo gives limen-read its gate and nothing else" "password is required" \
    docker exec -u limen-read "$node" sudo -n /bin/true
  expect "sudo gives limen-read no deploy gate" "password is required" \
    docker exec -u limen-read "$node" sudo -n /usr/local/bin/limen gate --role deploy

  # What the node lets read, a secret to redact, a check and two setup scripts.
  docker exec -i "$node" sh -c 'cat > /etc/limen/limen.toml' <<'EOF'
[files]
allow = ["/etc/hostname", "/etc/limen-e2e/**", "/var/log/e2e.log"]
deny = ["**/*.env"]
EOF
  # limen.toml as a link to a file kept elsewhere, as some operators do: read through it, and kept a link.
  docker exec "$node" sh -c 'mv /etc/limen/limen.toml /etc/limen/node.toml && ln -s node.toml /etc/limen/limen.toml'
  docker exec "$node" sh -c 'mkdir -p /etc/limen-e2e && printf "user=app\npassword=hunter2\n" > /etc/limen-e2e/app.conf \
    && echo "TOKEN=x" > /etc/limen-e2e/app.env && ln -s /etc/shadow /etc/limen-e2e/shadow-link \
    && { seq 1 50 | sed "s/^/line /"; echo "ERROR disk full"; seq 52 110 | sed "s/^/line /"; } > /var/log/e2e.log \
    && printf "a\\000b" > /etc/limen-e2e/blob'
  docker exec -i "$node" sh -c 'cat > /etc/limen/checks.d/disk.sh && chmod 0755 /etc/limen/checks.d/disk.sh' <<'EOF'
#!/bin/sh
#: description = "Root filesystem usage"
#: [args.threshold]
#: type = "int"
#: default = 100
#: range = [1, 100]
used=$(df --output=pcent / | tail -1 | tr -dc 0-9)
echo "root at ${used}%, threshold ${LIMEN_ARG_THRESHOLD}%"
[ "$used" -lt "$LIMEN_ARG_THRESHOLD" ] || exit 1
EOF
  docker exec -i "$node" sh -c 'cat > /etc/limen/checks.d/slow.sh && chmod 0755 /etc/limen/checks.d/slow.sh' <<'EOF'
#!/bin/sh
#: description = "Never finishes in time"
#: timeout = "1s"
sleep 30
EOF
  docker exec -i "$node" sh -c 'cat > /etc/limen/checks.d/loose.sh && chmod 0775 /etc/limen/checks.d/loose.sh' <<'EOF'
#!/bin/sh
#: description = "Group-writable: anyone in the group could change what root runs"
echo ran
EOF
  for ext in sh py; do
    docker exec -i "$node" sh -c "cat > /etc/limen/checks.d/twice.$ext && chmod 0755 /etc/limen/checks.d/twice.$ext" <<'EOF'
#!/bin/sh
#: description = "One name, two files"
echo which
EOF
  done
  docker exec -i "$node" sh -c 'cat > /etc/limen/setup.d/10-marker.sh && chmod 0755 /etc/limen/setup.d/10-marker.sh' <<'EOF'
#!/bin/sh
#: description = "Leaves a marker"
touch /var/tmp/limen-applied && echo 10 >> /var/tmp/limen-order && echo "marker written"
EOF
  docker exec -i "$node" sh -c 'cat > /etc/limen/setup.d/20-noisy.sh && chmod 0755 /etc/limen/setup.d/20-noisy.sh' <<'EOF'
#!/bin/sh
#: description = "Writes to both streams"
echo 20 >> /var/tmp/limen-order; echo "to stdout"; echo "to stderr" >&2
EOF

  host_key=$(docker exec "$node" cat /etc/ssh/ssh_host_ed25519_key.pub | cut -d' ' -f1,2)
  hub_config debian "$port" limen-read "$host_key"
  limen() { "$binary" "$@" --home "$work/debian"; }

  echo "e2e/debian: read role"
  expect "hello reports the catalog" '"name": "disk"' limen call debian hello
  expect "hello reports a script it won't run, and why" 'loose.sh is writable by group or others' limen call debian hello
  expect "status answers, disks from statvfs" '"mount": "/"' limen call debian status
  expect "an allowed file is read" '"content": "' limen call debian read_file --arg path=/etc/hostname
  expect "a secret in an allowed file is redacted" 'password=[redacted]' limen call debian read_file --arg path=/etc/limen-e2e/app.conf
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
  expect "file logs: the last lines" '"line 110"' limen call debian logs --arg source=file --arg name=/var/log/e2e.log --arg lines=5
  refuse "file logs: only the lines asked for" '"line 110"' 'ERROR' \
    limen call debian logs --arg source=file --arg name=/var/log/e2e.log --arg lines=5
  expect "file logs: grep searches beyond them" 'ERROR disk full' \
    limen call debian logs --arg source=file --arg name=/var/log/e2e.log --arg lines=5 --arg grep=error
  expect "more lines than logs.max_lines: cut, and said so" '"truncated": true' \
    limen call debian logs --arg source=file --arg name=/var/log/e2e.log --arg lines=5000
  expect "a check runs with its default" '"status": "ok"' limen call debian check_disk
  expect "the default reaches the script" 'threshold 100%' limen call debian check_disk
  expect "an argument reaches the script" 'threshold 1%' limen call debian check_disk --arg threshold=1
  expect "exit 1 is warn" '"status": "warn"' limen call debian check_disk --arg threshold=1
  expect "a bad argument never reaches the script" 'threshold must be at most 100' limen call debian check_disk --arg threshold=500
  expect "processes, from /proc" '"sshd' limen call debian processes
  expect "ports, from /proc: sshd on 22" '"port": 22' limen call debian ports
  expect "history records the client" '"client": "' limen call debian history --arg lines=3
  expect "a check past its timeout is stopped" '"code": "timeout"' limen call debian check_slow
  expect "a group-writable script is not run" 'writable by group or others' limen call debian check_loose
  expect "two files with one script name: neither runs" "are both 'twice'" limen call debian check_twice
  expect "and lint says so" "are both 'twice'" docker exec "$node" limen lint
  expect "limen check on the node exits 1 for warn" "exit 1" docker exec "$node" sh -c 'limen check disk --arg threshold=1 >/dev/null; echo "exit $?"'
  expect "and 3, UNKNOWN, when the check gave no answer" "exit 3" docker exec "$node" sh -c 'limen check slow >/dev/null 2>&1; echo "exit $?"'
  docker exec "$node" sh -c 'cp /etc/limen/limen.toml /tmp/limen.toml && printf "[limits]\nmax_response = 300\n" >> /etc/limen/limen.toml'
  expect "an answer over limits.max_response is refused" 'over limits.max_response' limen call debian status
  docker exec "$node" cp /tmp/limen.toml /etc/limen/limen.toml

  echo "e2e/debian: the gate is the only way in"
  read -ra read_ssh <<< "$(ssh_as read limen-read "$port")"
  expect "a command sent over ssh is ignored" 'no request on stdin' "${read_ssh[@]}" 'cat /etc/shadow' </dev/null
  expect "the read key can't apply" "not allowed for the read role" "${read_ssh[@]}" <<< '{"v":1,"request":"apply"}'
  expect "an unknown protocol version says so" '"versions":[1]' "${read_ssh[@]}" <<< '{"v":9,"request":"status"}'
  expect "and is in the audit log too" '"result": "unsupported_version"' limen call debian history --arg lines=5
  expect "a field nobody reads is an error" 'bad_request' "${read_ssh[@]}" <<< '{"v":1,"request":"status","role":"deploy"}'
  expect "a flood of arguments is refused" 'bad_request' \
    "${read_ssh[@]}" <<< "{\"v\":1,\"request\":\"status\",\"args\":{\"x\":\"$(head -c 200000 /dev/zero | tr '\0' x)\"}}"
  expect "and not copied into the audit log" '"omitted_bytes": ' limen call debian history --arg lines=3
  expect "no forwarding through the gate" 'stdio forwarding failed' "${read_ssh[0]}" -W 127.0.0.1:22 "${read_ssh[@]:1}" </dev/null
  read -ra crossed_ssh <<< "$(ssh_as read limen-deploy "$port")"
  expect "the read key doesn't open the deploy user" 'Permission denied' "${crossed_ssh[@]}" <<< '{"v":1,"request":"status"}'
  read -ra stranger_ssh <<< "$(ssh_as stranger limen-read "$port")"
  expect "an unknown key doesn't get in" 'Permission denied' "${stranger_ssh[@]}" <<< '{"v":1,"request":"status"}'
  docker exec "$node" limen install --read-key "$(cat "$work/read.pub")" --deploy-key "$(cat "$work/deploy.pub")" \
    --from 10.99.0.0/16 >/dev/null
  expect "--from: the read key only from there" 'Permission denied' "${read_ssh[@]}" <<< '{"v":1,"request":"status"}'
  docker exec "$node" limen install --read-key "$(cat "$work/read.pub")" --deploy-key "$(cat "$work/deploy.pub")" >/dev/null
  expect "install without --from lifts it" '"ok":true' "${read_ssh[@]}" <<< '{"v":1,"request":"status"}'

  echo "e2e/debian: deploy role"
  expect "apply runs every setup script" "limen: apply finished: 2 script(s)" \
    limen call debian apply --user limen-deploy --identity "$work/deploy"
  expect "in the order of their names" $'10\n20' docker exec "$node" cat /var/tmp/limen-order
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
  expect "an argument is typed by the script's header" "said 20" \
    limen call debian action --arg name=say --arg word=20 --user limen-deploy --identity "$work/deploy"
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
    && printf "# What the setup scripts do\n" > /tmp/w/nodes/e2e/setup/README.md \
    && printf "secret.env\n" > /tmp/w/nodes/e2e/.gitignore \
    && printf "[expect]\n" > /tmp/w/nodes/e2e/node.toml \
    && git -C /tmp/w add -A && git -C /tmp/w -c user.name=e2e -c user.email=e2e@e2e commit -qm first \
    && git -C /tmp/w push -q /srv/cloud.git main'
  expect "install --repo: readable without a token, checked out" "is readable without a token" \
    docker exec "$node" limen install --read-key "$(cat "$work/read.pub")" --deploy-key "$(cat "$work/deploy.pub")" \
    --repo file:///srv/cloud.git --path nodes/e2e
  expect "install wrote limen.toml through its link" "link" docker exec "$node" sh -c 'test -L /etc/limen/limen.toml && echo link'
  expect "the node takes its checks from the repository" '"status": "ok"' limen call debian check_from-repo
  expect "apply syncs and runs the repository's setup, a README aside" "limen: apply finished: 1 script(s), 0 stack(s)" \
    limen call debian apply --user limen-deploy --identity "$work/deploy"
  docker exec "$node" sh -c 'echo TOKEN=x > /opt/limen/repo/nodes/e2e/secret.env && touch /opt/limen/repo/nodes/e2e/stray' 
  expect "state: deployed and up to date" '"up_to_date": true' limen call debian state
  docker exec "$node" sh -c 'echo two > /tmp/w/f && git -C /tmp/w add -A \
    && git -C /tmp/w -c user.name=e2e -c user.email=e2e@e2e commit -qm second && git -C /tmp/w push -q /srv/cloud.git main'
  expect "state: behind after a push" 'the node is behind main' limen call debian state
  expect "sync catches up" " -> " limen call debian sync --user limen-deploy --identity "$work/deploy"
  expect "state: up to date again" '"up_to_date": true' limen call debian state
  expect "sync keeps ignored files: a stack's .env" "kept" docker exec "$node" sh -c 'test -f /opt/limen/repo/nodes/e2e/secret.env && echo kept'
  expect "and removes untracked ones" "gone" docker exec "$node" sh -c 'test -e /opt/limen/repo/nodes/e2e/stray || echo gone'
  # The repository moves: limen.toml names another one, one commit ahead.
  docker exec "$node" sh -c 'git clone -q --bare /srv/cloud.git /srv/moved.git && echo three > /tmp/w/f && git -C /tmp/w add -A \
    && git -C /tmp/w -c user.name=e2e -c user.email=e2e@e2e commit -qm moved && git -C /tmp/w push -q /srv/moved.git main \
    && sed -i "s|file:///srv/cloud.git|file:///srv/moved.git|" /etc/limen/limen.toml'
  expect "a changed [repo].url is followed" " -> " limen call debian sync --user limen-deploy --identity "$work/deploy"
  expect "state: at the new repository's head" '"subject": "moved"' limen call debian state

  echo "e2e/debian: hub"
  expect "mcp lists the node's check as a tool" '"name":"check_from-repo"' mcp_session debian check_from-repo
  expect "mcp calls it" 'repo check' mcp_session debian check_from-repo
  refuse "mcp exposes nothing that changes a machine" '"name":"status"' '"name":"(sync|apply|action[^"]*)"' mcp_session debian status
  # Someone else's host key: a real one, so ssh refuses it for not matching and for nothing else.
  sed -i "s|^host_key = .*|host_key = \"$(cut -d' ' -f1,2 "$work/stranger.pub")\"|" "$work/debian/limen.toml"
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
  expect "install.sh under ash: dropbear and root" '--user root' \
    docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_READ_KEY="$(cat "$work/read.pub")" \
    -e LIMEN_DEPLOY_KEY="$(cat "$work/deploy.pub")" "$node" sh /tmp/install.sh
  expect "install warns: root without a password" "root has no password" \
    docker exec "$node" /tmp/limen install --read-key "$(cat "$work/read.pub")" --deploy-key "$(cat "$work/deploy.pub")"
  expect "the administrator's key is kept" "admin" docker exec "$node" cat /etc/dropbear/authorized_keys
  expect "sysupgrade keeps limen" "/usr/bin/limen" docker exec "$node" cat /lib/upgrade/keep.d/limen
  refuse "install twice changes nothing" "limen is installed" "^(write|create|add|remove|run) " \
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
  expect "the Wi-Fi keys are never read" 'wireless is not readable' limen call openwrt read_file --arg path=/etc/config/wireless
  expect "root's files are owned by root, without NSS" '"owner": "root"' limen call openwrt list_dir --arg path=/etc/config
  expect "processes, from /proc: dropbear" 'dropbear' limen call openwrt processes
  expect "ports, from /proc: dropbear on 22" '"name": "dropbear"' limen call openwrt ports
  expect "no logd in a container: unavailable, said so" 'unavailable' limen call openwrt logs --arg source=journal

  echo "e2e/openwrt: the gate is the only way in"
  read -ra read_ssh <<< "$(ssh_as read root "$port")"
  expect "a command sent over ssh is ignored" 'no request on stdin' "${read_ssh[@]}" 'cat /etc/shadow' </dev/null
  expect "the read key can't apply" "not allowed for the read role" "${read_ssh[@]}" <<< '{"v":1,"request":"apply"}'
  read -ra deploy_ssh <<< "$(ssh_as deploy root "$port")"
  expect "the deploy key can't read" "not allowed for the deploy role" "${deploy_ssh[@]}" <<< '{"v":1,"request":"status"}'
  read -ra stranger_ssh <<< "$(ssh_as stranger root "$port")"
  expect "an unknown key doesn't get in" 'Permission denied' "${stranger_ssh[@]}" <<< '{"v":1,"request":"status"}'
  expect "apply with the deploy key" "limen: apply finished: 1 script(s)" \
    limen call openwrt apply --user root --identity "$work/deploy"
  expect "the setup script ran" "present" docker exec "$node" sh -c 'test -f /tmp/limen-applied && echo present'

  echo "e2e/openwrt: uninstall"
  expect "uninstall" "limen is uninstalled" docker exec "$node" limen uninstall --purge
  refuse "limen's keys are gone, the administrator's kept" "admin" "limen gate" docker exec "$node" cat /etc/dropbear/authorized_keys
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
  expect "and it runs as limen, not root" "7341" docker inspect -f '{{.Config.User}}' "$hub"
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
  expect "and nothing was installed" "no such user" docker exec "$nas" id limen-read
  expect "install.sh --join: the machine is on the hub" "nas is on the hub" \
    docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_JOIN="$line" "$nas" sh /tmp/install.sh
  expect "an invitation works once" "was used, or expired" \
    docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_JOIN="$line" "$nas" sh /tmp/install.sh
  expect "the hub reaches it, no restart" '"ok": true' hub_exec call nas status
  expect "the hub wrote it into limen.toml" "[nodes.nas]" hub_file limen.toml

  echo "e2e/join: OpenWrt joins the same way"
  line=$(join_line router)
  expect "install.sh --join under ash" "router is on the hub" \
    docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_JOIN="$line" "$router" sh /tmp/install.sh
  expect "the hub logs into it as root" 'user = "root"' hub_file limen.toml
  expect "and sees procd" '"init": "procd"' hub_exec call router hello

  echo "e2e/join: without a hub on HTTP, --hub-key and limen trust"
  key=$(hub_file id_ed25519.pub)
  trust=$(docker exec -e LIMEN_YES=1 -e LIMEN_BINARY=/tmp/limen -e LIMEN_HUB_KEY="$key" -e LIMEN_NAME=spare "$spare" sh /tmp/install.sh | grep 'limen trust')
  expect "the machine prints the line for the hub" "limen trust spare <address>" echo "$trust"
  spare_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$spare")
  read -ra trust_args <<< "$(echo "$trust" | sed "s/<address>/$spare_ip/; s/^ *limen trust //" | tr -d "'")"
  expect "limen trust on the hub" "spare added" docker exec "$hub" limen trust "${trust_args[0]}" "${trust_args[1]}" "${trust_args[2]} ${trust_args[3]}"
  expect "the hub reaches it" '"ok": true' hub_exec call spare hello

  echo "e2e/join: MCP over HTTP"
  mcp_http() {
    curl -s -X POST -H "Authorization: Bearer $token" -H 'Content-Type: application/json' "$url/mcp" -d "$1"
  }
  expect "tools/list offers the nodes that joined" '"enum":["nas","router","spare"]' \
    mcp_http '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
  expect "a tool call reaches a node" 'uptime_seconds' \
    mcp_http '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"status","arguments":{"node":"nas"}}}'
  # Text both ways with more than ASCII: Ktor's server must not go through iconv, which the static binary lacks.
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
