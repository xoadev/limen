# limen

An MCP server that lets an AI agent inspect Linux machines — configuration files, logs, systemd
services, Docker containers and your own checks — **without being able to change anything**.

**The machine decides.** Every limit lives on the machine being inspected, in `limen` running as an
SSH forced command. The MCP server, its clients and the model behind them are untrusted: if any of
them is compromised, or deceived by something it read in a log, the worst it can do is read what the
machine already allows to be read.

```
MCP client ──stdio or HTTP──▶ hub: limen mcp | limen serve
                                │  ssh limen-read@node   (one JSON request on stdin)
                                ▼
node:  sshd ──forced command──▶ sudo limen gate --role read ──▶ systemctl, journalctl, docker, files, checks
```

- **Nothing listens on the nodes.** `sshd` is the way in; `limen` starts per request and exits.
- **Nothing is readable by default.** Each node lists the paths it allows; secrets such as
  `/etc/shadow`, private keys or `/proc/*/environ` are never readable, whatever the list says.
- **Your scripts become tools.** Drop an executable with a small header into `/etc/limen/checks.d/`
  and the agent gets a `check_<name>` tool with typed arguments. Nagios-style exit codes, so existing
  monitoring plugins work as they are.
- **Changes go through another door.** Setup scripts (from a bare machine to a working one, or to
  restore it) and one-off actions run with a second key, the `deploy` role, which the MCP server never
  holds. CI or a person uses it; the agent can only suggest it.
- **A node can follow a repository.** Its scripts, Docker Compose stacks and a `node.toml` saying what
  must run live in a folder of a Git repository; `limen apply` syncs and converges, and the `state`
  tool says what is deployed against what should be.
- **One static binary, any Linux.** Kotlin/Native, `amd64` and `arm64`, carrying its own libc: the same
  file runs on Debian and on OpenWrt.

The full design is in [`docs/spec.md`](docs/spec.md).

## Tools

| Tool | What it answers |
|---|---|
| `nodes` | The machines, whether they answer, their OS and the scripts each one has |
| `status` | Uptime, load, memory, disks, failed services, unhealthy containers, pending reboot |
| `services`, `service` | systemd units or procd services; one with its state, restarts and last log lines |
| `containers`, `container` | Docker containers; one with its health, mounts, ports and labels (environment by name only) |
| `logs` | A unit, the journal, a container or an allowed file, always a bounded window, with `grep` |
| `read_file`, `list_dir` | Allowed files and the directories that lead to them |
| `processes`, `ports` | Top processes, listening sockets |
| `history` | The node's audit log of every request |
| `state` | The repository commit on the node against the remote, and whether each expected service runs |
| `check_<name>` | Your check scripts |

## Setting up a node

As root:

```sh
limen install --read-key "$(cat hub.pub)" --deploy-key "$(cat ci.pub)" \
  --repo https://github.com/you/infra.git --path nodes/$(hostname)
```

- **Debian, Ubuntu:** the binary goes to `/usr/local/bin`; the `limen-read` and `limen-deploy` users get
  a forced command in their `authorized_keys` and a `sudoers` rule for exactly that command.
  `--from 100.64.0.0/10` limits where the keys work.
- **OpenWrt:** the binary goes to `/usr/bin`; both keys go in root's dropbear `authorized_keys`, each
  with its forced command, next to the keys already there; sysupgrade keeps limen. Turn dropbear's
  password logins off: a forced command only holds a login by key, and `install` warns when root has
  no password.
- **`--repo`:** when the repository is private, `install` prints a GitHub link that fills in a
  fine-grained, read-only token (choose the repository in the form), asks for it and checks it before
  saving it. `limen token` replaces it when it expires.

`--dry-run` shows what it would do, and running it again changes nothing. At the end it prints what to
add to the hub, host key included.

Then say what may be read, in `/etc/limen/limen.toml`:

```toml
[files]
allow = ["/etc/nginx/**", "/opt/stacks/*/compose.yaml", "/var/log/nginx/*.log"]
deny = ["**/*.env"]
```

Whatever is readable ends up in the context of the model the hub talks to: list what helps diagnose
and nothing that holds a secret. Redaction of `password=…`, tokens and keys is a safety net, not the
protection.

## Setting up the hub

`~/.limen/limen.toml` (or `$LIMEN_HOME`), with the read key next to it:

```toml
[ssh]
identity = "id_ed25519"

[nodes.hades]
host = "100.64.0.2"
host_key = "ssh-ed25519 AAAA…"
```

`host_key` is required: there is no trust on first use. An OpenWrt node also takes `user = "root"`.

**stdio**, for Claude Code on the same machine:

```sh
claude mcp add limen -- limen mcp
```

**HTTP**, as a container any MCP client on your network can use:

```yaml
services:
  limen:
    image: ghcr.io/xoadev/limen
    environment:
      LIMEN_TOKEN: ${LIMEN_TOKEN}   # 16 characters or more; clients send it as a bearer token
    volumes:
      - ./limen:/data               # limen.toml and the SSH key
    ports:
      - "100.64.0.2:7341:7341"
```

```sh
claude mcp add --transport http limen http://100.64.0.2:7341/mcp --header "Authorization: Bearer $LIMEN_TOKEN"
```

Try a node by hand with `limen call hades status`.

## Writing a check

```bash
#!/usr/bin/env bash
#: description = "Free space on the backup volume"
#: timeout = "30s"
#: [args.threshold]
#: type = "int"
#: default = 90
#: range = [1, 100]
#: description = "Percent above which it warns"
set -euo pipefail
used=$(df --output=pcent /srv/backups | tail -1 | tr -dc 0-9)
if (( used >= LIMEN_ARG_THRESHOLD )); then echo "backups at ${used}%"; exit 1; fi
echo "backups at ${used}%"
```

Save it as `checks/backup-space.sh` (in the repository, or in `/etc/limen/checks.d/`), owned by root
and not writable by anyone else, and the agent gets `check_backup-space`. Exit `0` ok, `1` warn, `2` fail, `3` unknown; the first line is
the summary. `limen lint` checks every script without running any.

Setup scripts go in `setup/` as `10-packages.sh`, `20-users.sh`… and run in order with `limen apply`;
actions go in `actions/` and run with `limen action <name>`. From elsewhere, only with the deploy key:
`limen call hades apply --user limen-deploy --identity deploy_key`.

## A repository per node

```
nodes/hades/
  node.toml                     # what must be running
  checks/  actions/  setup/
  stacks/immich/compose.yaml
```

```toml
[expect]
compose = ["immich"]            # brought up by apply
units = ["docker.service"]      # systemd
procd = ["dnsmasq"]             # OpenWrt
```

Setup scripts say how to get there; `node.toml` says what must run. `limen apply` syncs the checkout to
the branch, runs the setup scripts, brings up the stacks and fails if something expected is not
running. Whoever can push to that branch runs code as root on the node: protect it.

## Building

```sh
make check   # lint, build and every test: what CI runs
make cli     # the binary, in kotlin/build/tasks/_cli_linkLinuxX64Debug/cli.kexe
make e2e     # Debian and OpenWrt containers, `limen install` inside, the hub against them (SUITE=debian|openwrt)
make help    # the rest
```

Needs a JDK for the linter; the Kotlin toolchain downloads itself. [`AGENTS.md`](AGENTS.md) is the
working contract of the repository.

## License

[Apache License 2.0](LICENSE).
