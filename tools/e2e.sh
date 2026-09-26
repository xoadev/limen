#!/usr/bin/env bash
# make e2e: the whole chain against a real sshd, in a throwaway Debian container. `limen install` sets the node up
# as root; the hub on this machine reaches it with the read key and the deploy key; every scene checks one promise
# of the spec. Not part of `make check`: it needs Docker and pulls an image.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
command -v docker >/dev/null || { echo "e2e: needs Docker; skipped" >&2; exit 0; }
binary=$("$ROOT/tools/kt" artifact)

work=$(mktemp -d)
node=limen-e2e-$$
cleanup() {
  docker rm -f "$node" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
# Control sockets of this run live here, not with the hub of whoever runs it.
export XDG_RUNTIME_DIR="$work/rt"
mkdir -m 0700 "$XDG_RUNTIME_DIR"

passed=0
failed=0
scene() { printf '%-60s' "$1"; }
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
  if [[ "$out" == *"$needle"* ]]; then ok; else ko "expected: $needle" "got: ${out:0:600}"; fi
}
# refuse <description> <needle> <command...>: the output must NOT contain <needle>.
refuse() {
  local what=$1 needle=$2 out
  shift 2
  scene "$what"
  out=$("$@" 2>&1) || true
  if [[ "$out" != *"$needle"* ]]; then ok; else ko "did not expect: $needle" "got: ${out:0:600}"; fi
}

echo "e2e: node image"
docker build -q -t limen-e2e-node -f "$ROOT/etc/e2e/node.Dockerfile" "$ROOT/etc/e2e" >/dev/null
docker run -d --name "$node" -p 127.0.0.1::22 limen-e2e-node >/dev/null
port=$(docker port "$node" 22/tcp | head -1 | sed 's/.*://')

ssh-keygen -q -t ed25519 -N '' -f "$work/read" -C limen-e2e-read
ssh-keygen -q -t ed25519 -N '' -f "$work/deploy" -C limen-e2e-deploy
docker cp "$binary" "$node:/tmp/limen"

echo "e2e: install"
expect "install sets the node up" "limen is installed" \
  docker exec "$node" /tmp/limen install --read-key "$(cat "$work/read.pub")" --deploy-key "$(cat "$work/deploy.pub")"
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
cat > "$work/limen.toml" <<EOF
[ssh]
identity = "read"

[nodes.e2e]
host = "127.0.0.1"
port = $port
host_key = "$host_key"
EOF
limen() { "$binary" "$@" --home "$work"; }

echo "e2e: read role"
expect "hello reports the catalog" '"disk"' limen call e2e check_disk
expect "status answers" '"ok": true' limen call e2e status
expect "an allowed file is read" '"content": "' limen call e2e read_file --arg path=/etc/hostname
expect "a secret in an allowed file is redacted" 'password=[redacted]' limen call e2e read_file --arg path=/etc/limen-e2e/app.conf
expect "a file outside the allowlist is denied" 'not in files.allow' limen call e2e read_file --arg path=/etc/passwd
expect "files.deny wins over files.allow" 'denied by files.deny' limen call e2e read_file --arg path=/etc/limen-e2e/app.env
expect "the built-in deny list can't be allowed" 'is never readable' limen call e2e read_file --arg path=/etc/shadow
expect "a symlink to a secret stays denied" 'is never readable' limen call e2e read_file --arg path=/etc/limen-e2e/shadow-link
expect "list_dir walks towards allowed files" '"name": "limen-e2e"' limen call e2e list_dir --arg path=/etc
refuse "list_dir hides what is not allowed" '"passwd"' limen call e2e list_dir --arg path=/etc
expect "file logs with grep" 'ERROR disk full' limen call e2e logs --arg source=file --arg name=/var/log/e2e.log --arg grep=error
expect "a check runs with its default" '"status": "ok"' limen call e2e check_disk
expect "a check runs with an argument" '"status": "warn"' limen call e2e check_disk --arg threshold=1
expect "a bad argument never reaches the script" 'threshold must be at most 100' limen call e2e check_disk --arg threshold=500
expect "processes" '"pid"' limen call e2e processes
expect "ports shows sshd" '"sshd"' limen call e2e ports
expect "history records the client" '"client": "' limen call e2e history --arg lines=3

echo "e2e: the gate is the only way in"
ssh_read=(ssh -T -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR -i "$work/read" -p "$port" limen-read@127.0.0.1)
expect "a command sent over ssh is ignored" 'no request on stdin' "${ssh_read[@]}" 'cat /etc/shadow' </dev/null
expect "the read key can't apply" "not allowed for the read role" \
  "${ssh_read[@]}" <<< '{"v":1,"request":"apply"}'
expect "an unknown protocol version says so" '"versions":[1]' "${ssh_read[@]}" <<< '{"v":9,"request":"status"}'

echo "e2e: deploy role"
expect "apply runs the setup scripts in order" "limen: apply finished, 2 script(s)" \
  limen call e2e apply --user limen-deploy --identity "$work/deploy"
expect "apply streams stderr too" "to stderr" \
  limen call e2e apply --user limen-deploy --identity "$work/deploy" --arg from=20
expect "the setup script ran as root" "limen-applied" docker exec "$node" ls /var/tmp/limen-applied
expect "the deploy key can't read" "not allowed for the deploy role" \
  ssh -T -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
  -i "$work/deploy" -p "$port" limen-deploy@127.0.0.1 <<< '{"v":1,"request":"status"}'

echo "e2e: hub"
mcp_session() {
  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"check_disk","arguments":{"node":"e2e"}}}' \
    | "$binary" mcp --home "$work" 2>/dev/null
}
expect "mcp lists the node's check as a tool" '"name":"check_disk"' mcp_session
expect "mcp calls it" 'root at' mcp_session
refuse "mcp exposes no action or apply" '"name":"apply"' mcp_session
sed -i "s|^host_key = .*|host_key = \"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOtherKeyOtherKeyOtherKeyOtherKeyOtherKey1\"|" "$work/limen.toml"
XDG_RUNTIME_DIR="$work/rt2" && mkdir -m 0700 "$XDG_RUNTIME_DIR"
expect "a host key that does not match is refused" "host_key_mismatch" limen call e2e status

echo "e2e: uninstall"
expect "uninstall removes the users" "limen is uninstalled" docker exec "$node" limen uninstall --purge
expect "and the users are gone" "no such user" docker exec "$node" id limen-read

echo
echo "e2e: $passed passed, $failed failed"
[[ $failed -eq 0 ]]
