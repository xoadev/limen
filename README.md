# limen

Let an AI agent look inside your Linux machines —configuration, logs, services, containers, your own checks—
**without giving it the power to change anything**, and describe each machine in a Git repository so it can be
rebuilt from scratch.

*Limen* is Latin for *threshold*: the agent stands at the door of each machine and sees what the machine lets it
see, and nothing crosses the other way.

## The idea

Asking an agent "why is Immich down?" or "is the backup disk filling up?" is useful only if it can look. The usual
way to let it look is an MCP server with SSH access, and those give the agent a shell, perhaps with a list of
forbidden commands kept by the MCP server itself. That list is only as strong as the MCP server and the model
behind it: a compromised client, or an instruction hidden in a log line the agent just read, and the shell is
there.

limen turns it around: **the machine decides.**

- On every machine, SSH only lets the agent's key run one program, `limen gate`, as a forced command. That program
  answers a fixed set of read-only questions, checks every argument, reads only the paths the machine allows,
  masks secrets, caps every answer and writes each request to an audit log.
- The MCP server —the *hub*— is just a relay. It holds a key that opens nothing but that door. If the hub, its
  token or the model are compromised, the worst outcome is reading what each machine already agreed to show.
- Changing a machine is a different door, with a different key the hub never has: the **deploy** role, used by CI
  or by a person, runs scripts that live in a Git repository.

## What we get

- **An agent that can diagnose, and can't break.** Status, failed services, logs of a unit or a container, a
  configuration file, who listens on which port, your own checks — for every machine, from one MCP server. When it
  finds the fix, it says what should be run; a person or CI runs it.
- **Machines described in a repository.** Each machine has a folder: setup scripts that take a bare machine to a
  working one, the Docker Compose stacks it runs, and `node.toml` saying what must be running. `limen apply` syncs
  the folder and converges; the `state` tool says what is deployed against what should be. Restoring a machine is
  installing its OS, then `limen install` and `limen apply`.
- **Every machine alike.** One static binary per architecture runs on Debian, Ubuntu and OpenWrt, x86-64 and arm64:
  the NAS, the always-on server and the router answer the same questions.
- **Nothing extra to run on the machines.** No daemon, no open port: `sshd` is already there, and `limen` starts
  per request and exits.
- **A record of everything asked.** Every request, with its role, arguments, client and result, in each machine's
  audit log, readable with the `history` tool.

## How it works

```
MCP client ──stdio or HTTP──▶ hub: limen mcp | limen serve
(Claude Code…)                  │  ssh limen-read@node   (one JSON request on stdin)
                                ▼
node:  sshd ──forced command──▶ sudo limen gate --role read ──▶ /proc, systemctl, journalctl, docker, files, checks
                                (answers JSON on stdout and exits)

CI or a person ──ssh limen-deploy@node──▶ limen gate --role deploy ──▶ sync, apply, actions
```

| Role | Key held by | Can |
|---|---|---|
| `read` | the hub | Everything in the tools below; run check scripts |
| `deploy` | CI or a person | Sync the repository, run setup scripts, bring up stacks, run actions |
| admin | whoever has root | Everything, on the machine |

## Tools

| Tool | What it answers |
|---|---|
| `nodes` | The machines, whether they answer, their OS, limen version and the scripts each one has |
| `status` | Uptime, load, memory, disks, failed services, unhealthy containers, pending reboot. Start here |
| `services`, `service` | systemd units or OpenWrt's procd services; one with its state, restarts and last log lines |
| `containers`, `container` | Docker containers; one with its health, mounts, ports and labels (environment variables by name only) |
| `logs` | A unit, the whole journal, a container or an allowed file: always a bounded window, with `since`, `until` and `grep` |
| `read_file`, `list_dir` | Files the machine allows, and the directories that lead to them |
| `processes`, `ports` | Top processes by CPU or memory; listening sockets and who holds them |
| `state` | The repository commit on the machine against the remote, and whether each expected service runs |
| `history` | The machine's audit log |
| `check_<name>` | Your check scripts, with their own typed arguments |

## Install on a machine

As root, on Debian, Ubuntu or OpenWrt:

```sh
curl -fsSL https://raw.githubusercontent.com/xoadev/limen/main/install.sh | sudo sh
# OpenWrt, as root:
wget -qO- https://raw.githubusercontent.com/xoadev/limen/main/install.sh | sh
```

The script downloads the binary for this machine from the latest release, checks it against the release's
`SHA256SUMS`, and asks:

1. **The hub's public key**, for the read role (paste it, or give the path of a `.pub` file).
2. **A key for the deploy role**, CI's or yours, or `none`.
3. **Where the keys may connect from**, such as `100.64.0.0/10` for a Tailscale or Headscale network (not on OpenWrt,
   whose dropbear can't; use the firewall there).
4. **The repository this machine follows**, or `none`; then its **branch** and **this machine's folder** in it
   (`nodes/<hostname>` by default). If git is missing it offers to install it.

If the repository is private, you get a link that opens GitHub's form for a new token with everything filled in —a
fine-grained token, read-only access to contents, no expiry—; choose the repository under *Repository access*, paste
the token (it is not shown), and limen checks that it reads the repository before saving it where only root can
read it. `limen token` replaces it the day it expires. Nothing on the machine changes until the repository is
readable.

Then it sets the machine up and prints what to add to the hub, host key included:

- **Debian, Ubuntu**: users `limen-read` and `limen-deploy`, each with its key held to its forced command, and a
  sudo rule for exactly that command. `/etc/limen/`, the audit log, the repository checkout.
- **OpenWrt**: both keys in root's dropbear `authorized_keys`, each held to its forced command, next to the keys
  already there; the binary is kept across `sysupgrade`. **Turn off dropbear's password logins**: a forced command
  only holds a login by key, and the installer warns when root has no password.

Unattended, every answer comes from the environment:

```sh
LIMEN_YES=1 LIMEN_READ_KEY="ssh-ed25519 AAAA… hub" LIMEN_DEPLOY_KEY=none \
LIMEN_REPO=https://github.com/you/infra.git LIMEN_PATH=nodes/nas LIMEN_REPO_TOKEN=github_pat_… \
  sh install.sh
```

Or by hand: download `limen-<version>-linux-$(uname -m)` from the [releases](https://github.com/xoadev/limen/releases)
and run `sudo ./limen-… install --read-key … [--deploy-key …] [--repo … --path …]`. `--dry-run` shows what it
would do; running it again changes nothing.

### What the agent may read

Nothing, until you say so, in `/etc/limen/limen.toml`:

```toml
[files]
allow = ["/etc/nginx/**", "/opt/stacks/*/compose.yaml", "/var/log/nginx/*.log"]
deny = ["**/*.env"]
```

Whatever is readable ends up in the context of the model the hub talks to: list what helps diagnose, nothing that
holds a secret. Some paths are never readable whatever the list says —`/etc/shadow`, private keys, `/proc`,
`/root`, `/etc/limen`— and `password=…`, tokens and keys are masked in everything that leaves the machine, as a
safety net.

## Set up the hub

The hub is wherever the MCP client runs, or a container on your network. It needs the same `limen` binary, a key
pair and `limen.toml`:

```sh
mkdir -p ~/.limen && ssh-keygen -t ed25519 -N '' -f ~/.limen/id_ed25519   # its .pub is the read key
```

```toml
# ~/.limen/limen.toml (or $LIMEN_HOME/limen.toml)
[ssh]
identity = "id_ed25519"

[nodes.nas]
host = "100.64.0.2"
host_key = "ssh-ed25519 AAAA…"      # printed by the installer; there is no trust on first use

[nodes.router]
host = "100.64.0.1"
user = "root"                       # OpenWrt
host_key = "ssh-ed25519 AAAA…"
```

Try it with `limen call nas status`. Then connect an MCP client:

```sh
# stdio, for Claude Code on the same machine
claude mcp add limen -- limen mcp
```

```yaml
# HTTP, as a container any MCP client on your network can use
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

## Use it

Ask the agent as you would ask a colleague with read access:

- *"What's wrong on the NAS?"* — `status`, then `service` and `logs` of what failed.
- *"Why does Immich restart?"* — `container immich`, its health log, `logs source=container name=immich grep=error`.
- *"Is the router's DHCP running, and what's in its config?"* — `service dnsmasq`, `read_file /etc/config/dhcp`.
- *"Is every machine on the latest commit?"* — `state` on each node.

When the answer is a change, the agent says what to run; you or CI run it with the deploy key:

```sh
limen call nas apply --user limen-deploy --identity ~/.ssh/deploy      # sync, setup scripts, stacks, state
limen call nas action --arg name=restart-immich --user limen-deploy --identity ~/.ssh/deploy
```

## A repository per machine

```
nodes/nas/
  node.toml                     # what must be running
  setup/10-packages.sh          # from a bare machine to a working one, in order
  setup/20-docker.sh
  stacks/immich/compose.yaml    # Docker Compose stacks
  checks/backup-space.sh        # tools for the agent
  actions/restart-immich.sh     # one-off changes, deploy role only
```

```toml
# node.toml
[expect]
compose = ["immich"]            # stacks/<name>/compose.yaml, brought up by apply
units = ["docker.service"]      # systemd units that must be active
procd = ["dnsmasq"]             # OpenWrt services that must run
```

Setup scripts say **how** to get there, `node.toml` says **what** must run. `limen apply` leaves the checkout
exactly as the branch —local edits are discarded—, runs the setup scripts in order (each must be idempotent),
brings up the stacks, and fails if something expected is not running, so CI notices. **Whoever can push to that
branch runs code as root on the machine: protect it.**

### Writing a check

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

As `checks/backup-space.sh`, the agent gets `check_backup-space` with a `threshold` argument. The `#:` lines are the
header, read and never run: description, timeout and typed arguments (`int`, `bool`, `enum`, `string`), which reach
the script as `LIMEN_ARG_<NAME>`. Exit `0` ok, `1` warn, `2` fail, `3` unknown, first line the summary: Nagios
plugins work as they are. Setup scripts (`setup/NN-name.sh`) and actions (`actions/name.sh`) have the same header.

A script must be owned by root and not writable by anyone else, like every directory above it; `limen lint` checks
them all without running anything.

## Security, in short

| If this happens | Then |
|---|---|
| The hub, its token or the MCP client are compromised | Reads what the machines allow. Changes nothing: no key for it |
| An instruction is injected through a log or a file | The same: the model can only ask for more reads |
| Arguments are crafted to escape | They are typed, validated on the machine, and never reach a shell |
| A symlink points from an allowed path to a secret | Paths are resolved before they are checked |
| Someone can push to the repository's branch | **They are root on the machine.** Protect the branch |
| A check script is malicious | Not covered: scripts belong to root, and limen trusts them |
| An allowed file holds a secret | Partly covered: masking catches the usual shapes only |

What is readable reaches the model provider, by design. The full threat model is in [`docs/spec.md`](docs/spec.md).

## Platforms

- Machines: Debian and Ubuntu (systemd, OpenSSH, sudo) and OpenWrt (procd, dropbear, `logread`). Docker optional.
- Binaries: Linux x86-64 and arm64, static; the same file runs on glibc and musl.
- Hub: anything that runs the binary and OpenSSH's `ssh`; the image is `ghcr.io/xoadev/limen`, amd64 and arm64.

## Develop

```sh
make check   # lint, build and every test: what CI runs
make cli     # the binary, in kotlin/build/tasks/_cli_linkLinuxX64Debug/cli.kexe
make e2e     # Debian and OpenWrt containers with a real SSH server, the installer and the hub (SUITE=debian|openwrt)
make help    # the rest
```

Kotlin/Native; the toolchain downloads itself, and the linter needs a JDK. The design is in
[`docs/spec.md`](docs/spec.md), the working contract in [`AGENTS.md`](AGENTS.md). Releases follow the conventional
commits: every push to `main` prepares a draft, and publishing it builds and attaches the binaries and the image.

## License

[Apache License 2.0](LICENSE).
