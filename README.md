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
  installing its OS, joining it to the hub and running `limen apply`.
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

a new machine ──GET/POST /join/<one-time code>──▶ hub   (its key, then the machine's host key)
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

## Install

Two pieces: the **hub**, where the MCP server runs, and every **machine** it inspects. You start the hub once; each
machine then joins it with one line the hub gives you.

### 1. Start the hub

On an always-on machine of your network or VPN, with Docker:

```yaml
# compose.yaml
services:
  limen:
    image: ghcr.io/xoadev/limen
    environment:
      LIMEN_PUBLIC_URL: http://100.64.0.2:7341   # where machines reach the hub: an address, not a name
    volumes:
      - ./limen:/data
    ports:
      - "100.64.0.2:7341:7341"                   # only on the VPN address
    restart: unless-stopped
```

```sh
docker compose up -d
docker compose exec limen limen connect
# claude mcp add --transport http limen http://100.64.0.2:7341/mcp --header "Authorization: Bearer …"
```

On its first start the hub creates, in `./limen`, its SSH key, its `limen.toml` and the token of MCP clients.
`limen connect` prints the line that connects Claude Code (or any MCP client) to it.

### 2. Add a machine

Ask the hub for an invitation:

```sh
docker compose exec limen limen invite nas
# On nas, as root:
#   curl -fsSL https://raw.githubusercontent.com/xoadev/limen/main/install.sh | sudo sh -s -- --join 'http://100.64.0.2:7341/join/…#SHA256:…'
# Valid once, for 1h.
```

Paste that line on the machine —Debian, Ubuntu or OpenWrt (with `wget -qO-` and `sh` there)— and it:

1. Downloads the binary for the machine from the latest release and checks it against `SHA256SUMS`.
2. Fetches the hub's key and checks it against the fingerprint in the line: nobody in between can slip in theirs.
3. Asks the only optional thing: **the repository the machine follows**, its branch and its folder
   (`nodes/<name>` by default), or none. If git is missing it offers to install it. If the repository is private,
   you get a link that opens GitHub's form for a new token with everything filled in —fine-grained, read-only
   contents, no expiry—: choose the repository, paste the token (it is not shown), and limen checks it reads the
   repository before saving it where only root can read it.
4. Sets the machine up:
   - **Debian, Ubuntu**: a `limen-read` user whose key only runs `limen gate`, and a sudo rule for exactly that.
   - **OpenWrt**: the hub's key in root's dropbear `authorized_keys`, held to `limen gate`, next to the keys already
     there; limen survives `sysupgrade`. **Turn off dropbear's password logins**: a forced command only holds a login
     by key, and the installer warns when root has no password.
5. Tells the hub its host key. The hub adds it to `limen.toml`, tries it, and the installer says
   `nas is on the hub, at 100.64.0.5: Debian GNU/Linux 13, limen 0.1.0`.

That is all: the agent sees the machine in `nodes`, with no restart. The invitation is spent; the next machine gets
its own.

### What the agent may read

Nothing, until you say so, in the machine's `/etc/limen/limen.toml`:

```toml
[files]
allow = ["/etc/nginx/**", "/opt/stacks/*/compose.yaml", "/var/log/nginx/*.log"]
deny = ["**/*.env"]
```

Whatever is readable ends up in the context of the model the hub talks to: list what helps diagnose, nothing that
holds a secret. Some paths are never readable whatever the list says —`/etc/shadow`, private keys, `/proc`,
`/root`, `/etc/limen`— and `password=…`, tokens and keys are masked in everything that leaves the machine, as a
safety net.

### Other ways

- **The hub on your laptop**, for Claude Code there (stdio, nothing listening):
  `curl -fsSL …/install.sh | sh -s -- --hub` installs `limen` in `~/.local/bin`, creates `~/.limen` and prints the
  `claude mcp add` line. Without an HTTP hub to call back, `limen invite nas` prints a line with the hub's key in it
  (`--hub-key`), and the machine ends printing the `limen trust nas <address> '<host key>'` to run on the laptop.
- **Unattended**, every answer comes from the environment:
  `LIMEN_YES=1 LIMEN_JOIN='…' LIMEN_REPO=https://github.com/you/infra.git LIMEN_REPO_TOKEN=github_pat_… sh install.sh`.
  `LIMEN_DEPLOY_KEY` adds the deploy role; `LIMEN_FROM` limits where keys may connect from (not on OpenWrt).
- **By hand**: download `limen-<version>-linux-$(uname -m)` from the [releases](https://github.com/xoadev/limen/releases)
  and run `sudo ./limen-… join '<line>'`.
- `limen forget nas` takes a machine off the hub; `limen uninstall --purge` on the machine removes limen from it.

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
- Binaries: Linux x86-64 and arm64, static; the same file runs on glibc and musl. How and why:
  [`docs/openwrt.md`](docs/openwrt.md).
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
