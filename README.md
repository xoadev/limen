# limen

```
    ╭─────╮
    │  ◉  │   limen
  ══╧═════╧══  only what the machine allows
```

[![check](https://github.com/xoadev/limen/actions/workflows/check.yml/badge.svg)](https://github.com/xoadev/limen/actions/workflows/check.yml)
[![release](https://img.shields.io/github/v/release/xoadev/limen?include_prereleases&sort=semver)](https://github.com/xoadev/limen/releases)
[![license](https://img.shields.io/github/license/xoadev/limen)](LICENSE)

Let an AI agent into your Linux machines **without giving it a shell.** It reads the files each machine allows, and
runs the scripts each machine offers —to look, and to change what you decided may be changed—. Nothing else.

limen is an [MCP](https://modelcontextprotocol.io) server —MCP is how Claude Code and other agents call tools— that
reaches every machine you join to it over SSH. Every script becomes a tool: `containers`, `unit_logs`,
`restart_unit`, `purge`, `sync`… whatever you put in the machine's packs.

*Limen* is Latin for *threshold*: the agent stands at the door of each machine, and the machine decides what crosses.

```
You:    Immich is down. Why?
Agent:  (status on nas, then container immich, then container_logs immich)
        immich-server restarts every minute: Postgres can't write, "No space left on device".
        /srv is at 100%; most of it is /srv/immich/upload/encoded-video, and 40 GB are dangling images.
You:    Clear the images.
Agent:  (purge on nas) Freed 41 GB; immich-server is up.
```

> **Status: early.** limen works end to end —its tests run it on Debian and on OpenWrt's own image, against real
> SSH servers—, and its [releases](https://github.com/xoadev/limen/releases) publish the binaries, the hub's image
> and each pack. It has not run for long on real machines yet, and until 1.0 its configuration and protocol may
> change between versions: read a release's notes before updating.

## Why

Asking an agent "why is Immich down?" is useful only if it can look, and "clear the old images" only if it can act.
The usual way is an MCP server with SSH access, and those give the agent a shell, perhaps with a list of forbidden
commands kept by the MCP server itself. That list is only as strong as the MCP server and the model behind it: a
compromised client, or an instruction hidden in a log line the agent just read, and the shell is there.

limen turns it around: **the machine decides.**

- On every machine, SSH only lets the hub's key run one program, `limen gate`, as a forced command. It reads only
  the paths the machine allows, runs only the scripts in the machine's packs with the arguments their headers
  declare, masks secrets, caps every answer and writes each request to an audit log.
- The MCP server —the *hub*— is just a relay. If the hub, its token or the model are compromised, the worst outcome
  is reading what each machine allows and running the scripts it offers: never a command of their own.
- **limen knows nothing of systemd, Docker or git.** Everything a machine can do is a script, grouped in packs: one
  for systemd, one for Docker, one for your own services. A new kind of machine is a new pack, not a new release.

## What you get

- **An agent that can diagnose and act, within what you wrote.** Status, logs of a unit or a container, a
  configuration file, restarting a service, clearing old images, bringing the machine to its repository: each a
  script you can read, on each machine.
- **Every machine alike.** One static binary per architecture runs on Debian, Ubuntu and OpenWrt, x86-64 and arm64.
- **Nothing extra to run on the machines.** No daemon, no open port: `sshd` is already there, and `limen` starts
  per request and exits.
- **A record of everything asked.** Every request and every script run, with its arguments, client and exit code,
  in each machine's audit log, readable with the `history` tool.

## How it works

```
MCP client ──stdio or HTTP──▶ hub: limen mcp | limen serve
(Claude Code…)                  │  ssh limen@node   (one JSON request on stdin)
                                ▼
node:  sshd ──forced command──▶ sudo limen gate ──▶ allowed files, the packs' scripts
                                (answers JSON on stdout and exits)

a new machine ──GET/POST /join/<one-time code>──▶ hub   (its key, then the machine's host key)
```

The hub talks to each machine (each *node*) with the system's `ssh`; nothing listens on the machines but `sshd`.

## Tools

| Tool | What it answers |
|---|---|
| `nodes` | The machines, whether they answer, their OS, limen version and the scripts each one offers |
| `read_file`, `list_dir` | Files the machine allows, and the directories that lead to them: a range of lines, or the last ones with `grep` |
| `history` | The machine's audit log |
| *each script* | One tool per script in the machines' packs, with its own typed arguments, plus `grep` and `tail` to narrow its output |

The packs in [`packs/`](packs/), each released on its own with its own version ([how](#packs)):

| Pack | Its scripts answer, or do |
|---|---|
| `system` | Status, memory, processes, ports, network, whether a host is reachable, DNS, the kernel's log, the clock, file systems, disks and SMART |
| `systemd` | Units, failed units, timers, a unit's journal, the whole journal; restarting a unit |
| `debian` | Upgradable packages, one package's versions; `apt-get upgrade`, autoremove, rebooting |
| `docker` | Containers, their stats and logs, images, disk use, Compose projects; restarting, purging, updating a Compose project you listed |
| `openwrt` | The board, services, log, interfaces, DHCP leases, Wi-Fi clients, upgradable packages; restarting a service |
| `limen` | limen's version, its packs' versions, lint; updating limen itself |

Each pack's README says what each script needs and what it never prints.

## Install

Two pieces: the **hub**, where the MCP server runs, and every **machine** it inspects —a *node*, in the tools and the
configuration—. You start the hub once; each machine then joins it with one line the hub gives you.

You need:

- For the hub: Docker on an always-on machine; or, for a hub on your laptop, OpenSSH's `ssh` and `ssh-keygen`.
- For each machine: Debian, Ubuntu or OpenWrt, x86-64 or arm64, running an SSH server the hub can reach —over a VPN
  such as Tailscale or Headscale, or your LAN—, and root to install. `curl` and `sudo` on Debian and Ubuntu (OpenWrt
  has `wget` and needs no sudo). What the packs' scripts use —systemd, Docker— is their business.

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
      - limen:/data                              # the whole hub: its key, limen.toml, the token
    ports:
      - "100.64.0.2:7341:7341"                   # only on the VPN address
    restart: unless-stopped

volumes:
  limen:
```

```sh
docker compose up -d
docker compose exec limen limen connect
# claude mcp add --transport http limen http://100.64.0.2:7341/mcp --header "Authorization: Bearer …"
```

On its first start the hub creates, in the `limen` volume, its SSH key, its `limen.toml` and the token of MCP
clients. (The image runs as distroless's `nonroot`, uid 65532: a bind mount instead of the volume needs a directory that
user owns.)
`limen connect` prints the line that connects Claude Code (or any MCP client) to it.

### 2. Add a machine

Ask the hub for an invitation:

```sh
docker compose exec limen limen invite nas
# On nas, as root:
#   curl -fsSL https://raw.githubusercontent.com/xoadev/limen/main/install.sh | sudo sh -s -- --join 'http://100.64.0.2:7341/join/…#SHA256:…'
# OpenWrt:
#   wget -qO- https://raw.githubusercontent.com/xoadev/limen/main/install.sh | sh -s -- --join 'http://100.64.0.2:7341/join/…#SHA256:…'
# Valid once, for 1h.
```

Paste the line for the machine's system on it. It downloads limen and checks it, fetches the hub's key and checks
it against the fingerprint in the line, sets the machine up and reports to the hub, which adds it and tries it:

```
nas is on the hub, at 100.64.0.5: Debian GNU/Linux 13, limen 0.1.0
```

- **Debian, Ubuntu**: a `limen` user whose key only runs `limen gate`, and a sudo rule for exactly that.
- **OpenWrt**: the hub's key in root's dropbear `authorized_keys`, held to `limen gate`, next to the keys already
  there; limen survives `sysupgrade`. **Turn off dropbear's password logins**: a forced command only holds a login
  by key, and the installer warns when root has no password.

The hub files the address the join request comes from. When the hub runs in Docker, or a NAT sits between, that is the
gateway and not the machine, and the hub fails with `host_key_mismatch`. Then tell it where the machine is, by
address or by a name the hub resolves: `sudo sh -s -- --join '…' --address 100.64.0.5` on the installer, or
`limen join '…' --address 100.64.0.5` once limen is there. Joining again with a new invitation replaces the entry.

The hub needs no restart, and the invitation is spent: the next machine gets its own. How the join is protected
against someone in between: [`docs/spec.md`](docs/spec.md#101-joining-a-node).

### From source

To run a commit that isn't released: build it (see [Develop](#develop)), then `make docker` for the hub
—`image: limen:local` in the compose file— and, on each machine, copy the binary of `make cli` and `install.sh`
and run `sudo env LIMEN_BINARY=./limen sh install.sh --join '<line>'`.

### What the agent may read and run

Nothing, until you say so, in the machine's `/etc/limen/limen.toml`:

```toml
[files]
allow = ["/etc/nginx/**", "/opt/stacks/*/compose.yaml", "/var/log/nginx/*.log"]
deny = ["**/*.env"]

[scripts]
packs = ["/opt/limen-packs/system@0.1.1", "/opt/limen-packs/docker@0.1.1", "/opt/state/nodes/nas"]

[redact]
names = ["DB_PASSWORD", "MQTT_PASS"]
```

- **Files**: whatever is readable ends up in the context of the model the hub talks to. List what helps diagnose,
  nothing that holds a secret. Some paths are never readable whatever the list says —`/etc/shadow`, private keys,
  `/proc`, `/root`, `/etc/limen`—. A link is read where it leads, so it is the target that must be allowed:
  `/etc/os-release` on Debian is `/usr/lib/os-release`.
- **Scripts**: every script of the packs listed is a tool. **Offer only changes you would let whoever writes to
  your logs trigger**: text in a log can lead the model to call any of them.
- **Secrets**: `password=…`, tokens, keys and the values of `redact.names` are masked in everything that leaves the
  machine, as a safety net.

`limen.toml` can be a link into your own repository, with the packs next to it: limen never writes it after
install. `sudo limen lint` checks the packs without running anything; `sudo limen run <script>` runs one as the hub
would.

## Packs

A pack is a directory of scripts. A script is an executable file with a header: `#:` lines that form a TOML
document, read and never run.

```sh
#!/bin/sh
#: description = "Last lines of a Docker container's log"
#: read_only = true
#: [args.name]
#: type = "string"
#: pattern = '^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$'
#: [args.since]
#: type = "string"
#: default = "1h"
#: pattern = '^[0-9]{1,4}[smhd]$'
exec docker logs --timestamps --since "$LIMEN_ARG_SINCE" "$LIMEN_ARG_NAME" 2>&1
```

The agent gets a tool with `name` and `since`; limen checks both before the script runs, and they reach it as
`LIMEN_ARG_NAME` and `LIMEN_ARG_SINCE`. `read_only = true` tells MCP clients the script changes nothing, so they may
run it without asking. The script runs as root, owned by root and writable by nobody else, like every directory
above it. The whole format, and a machine set up from a Git repository with a `sync` script the agent can run, are
in [`docs/scripts.md`](docs/scripts.md).

The packs of this repository are released on their own, as `pack-<pack>-vX.Y.Z`: a tarball and its `SHA256SUMS`,
with a build attestation. limen downloads none of them: the machine's own setup unpacks each one into a folder of its
version, `/opt/limen-packs/<pack>@X.Y.Z`, pinning its hash, as [`docs/scripts.md`](docs/scripts.md#released-packs)
shows; the `limen` pack's `limen_packs` then says when a newer one is out. **Packs 0.1.1 need limen 0.1.3 or later**:
older versions refuse a header with `read_only`.

### Other ways

- **Before a hub**, a machine can have its configuration and packs first:
  `curl -fsSL …/install.sh | sudo env LIMEN_YES=1 sh` installs limen with nothing open to SSH, and a hub joins it
  later with its line. [`docs/scripts.md`](docs/scripts.md#a-machine-from-a-git-repository) shows a bootstrap that
  clones the machine's repository first.
- **The hub on your laptop**, for Claude Code there (stdio, nothing listening):
  `curl -fsSL …/install.sh | sh -s -- --hub` installs `limen` in `~/.local/bin`, creates `~/.limen` and prints the
  `claude mcp add` line. Without an HTTP hub to call back, `limen invite nas` prints a line with the hub's key in it
  (`--hub-key`), and the machine ends printing the `limen trust nas <address> '<host key>'` to run on the laptop
  (`--address` on the installer fills the address in).
- **Unattended**, every answer comes from the environment: `sudo env LIMEN_YES=1 LIMEN_JOIN='…' sh install.sh`.
  `LIMEN_FROM` (`--from`) limits where the hub's key may connect from (not on OpenWrt), `LIMEN_ADDRESS`
  (`--address`) is where the hub reaches this machine, and `LIMEN_SSH_PORT` (`--ssh-port`) its SSH port when it isn't
  22. The whole list is at the top of [`install.sh`](install.sh).
- **By hand**: download `limen-<version>-linux-$(uname -m)` from the [releases](https://github.com/xoadev/limen/releases)
  and run `sudo ./limen-… join '<line>'`. Where there is `gh`, `gh attestation verify limen-… --repo xoadev/limen`
  checks that the release workflow built it; the image, likewise with `oci://ghcr.io/xoadev/limen:<version>`.
- `limen forget nas` takes a machine off the hub; `limen uninstall --purge` on the machine removes limen from it.

## Use it

Ask the agent as you would ask a colleague with access to the machines' scripts:

- *"What's wrong on the NAS?"* — `status`, then `failed_units`, and `unit_logs` of what failed.
- *"Why does Immich restart?"* — `container immich`, then `container_logs name=immich grep=error`.
- *"Bring the NAS to the repository."* — `sync`, if the machine's pack offers it.

Without the agent, `docker compose exec limen limen call nas status` asks a machine the same by hand.

### A person approves the changes

With `limen mcp` (stdio), the hub can ask you before each run of a script that changes things. Add this to the hub's
`limen.toml`:

```toml
[approval]
scripts = "changes"   # every script that isn't read-only; or a list: ["upgrade", "reboot"]
except = ["restart_container"]   # with "changes": these run without asking
timeout = "5m"
```

The question appears in the MCP client's own window (MCP elicitation), with the machine, the script and its
arguments: the model can't see it or answer it. Only a yes runs the script, once. A no, no answer, or a client that
can't ask means nothing runs. Over HTTP (`limen serve`) the hub can't ask yet, so those scripts are refused there.

## Security, in short

| If this happens | Then |
|---|---|
| The hub, its token or the MCP client are compromised | Reads what the machines allow and runs the scripts they offer, with the arguments they declare. No shell |
| An instruction is injected through a log or a file | The same: the model can be led to run any script on offer. Offer only changes you'd let anyone writing your logs trigger, or [have a person approve them](#a-person-approves-the-changes) |
| Arguments are crafted to escape | They are typed, validated on the machine, and reach the script as variables, never a shell |
| A symlink points from an allowed path to a secret | Paths are walked as the kernel does before they are checked |
| Someone can change a pack, or push where a script fetches packs from | **They are root on the machine.** Keep the agent out of it, and protect the branch |
| A script is malicious or prints secrets | Not covered: scripts belong to root, and limen trusts them. Masking catches the usual shapes |

What is readable or printed reaches the model provider, by design. The full threat model is in
[`docs/spec.md`](docs/spec.md#13-threat-model); to report a vulnerability, see [`SECURITY.md`](SECURITY.md).

## Platforms

- Machines: any Linux with OpenSSH and sudo, and OpenWrt with dropbear. What the scripts need is the packs' business.
- Binaries: Linux x86-64 and arm64, static (musl), about 3 MB; the same file runs on any distribution. OpenWrt
  in detail: [`docs/openwrt.md`](docs/openwrt.md).
- Hub: anything that runs the binary and OpenSSH's `ssh`; the image is `ghcr.io/xoadev/limen`, amd64 and arm64.

## Documentation

| | |
|---|---|
| [`docs/spec.md`](docs/spec.md) | The design and the reference: access, protocol, tools, packs, configuration, CLI, joining, threat model, decisions |
| [`docs/scripts.md`](docs/scripts.md) | Writing scripts and packs, and a machine set up from a Git repository |
| [`docs/openwrt.md`](docs/openwrt.md) | OpenWrt as a node, and one binary for every Linux |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | How to build, test and send a change |
| [`AGENTS.md`](AGENTS.md) | The full working contract, for people and coding agents |
| [`SECURITY.md`](SECURITY.md) | How to report a vulnerability, and what counts as one |

Releases and their notes are on [GitHub](https://github.com/xoadev/limen/releases).

## Develop

```sh
make check   # lint, build and every test: what CI runs
make cli     # the static binary, in target/<arch>-unknown-linux-musl/debug/limen
make e2e     # Debian and OpenWrt containers with a real SSH server, the installer and the hub (SUITE=debian|openwrt|join)
make help    # the rest
```

Needs Linux, [rustup](https://rustup.rs) and Docker for `make e2e`; the pinned Rust version installs itself.
Contributions are welcome: [`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

limen is licensed under the [Apache License 2.0](LICENSE).

The release binaries are static, so they contain third-party code under its own permissive licences: musl (MIT),
Rust's standard library and the crates listed in `Cargo.lock` (MIT or Apache-2.0).
