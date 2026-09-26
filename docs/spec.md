# limen — specification v0.1 (draft)

An MCP server for inspecting Linux machines: configuration files, logs, systemd services, Docker
containers and the operator's own checks. It cannot change anything. The same binary also runs the
operator's scripts that do change a machine — setting it up from scratch, restoring it, one-off
actions — but under a role the MCP server never holds.

**The machine decides.** Every limit is enforced on the node, by `limen` running as an SSH forced
command. The MCP server, its clients and the model behind them are untrusted: if any of them is
compromised or deceived, the worst outcome is reading what the node already allows to be read.

## 0. Repository

```
limen/
  docs/spec.md          # this spec: the single source of truth
  AGENTS.md             # the working contract (CLAUDE.md points there)
  kotlin/               # Kotlin Toolchain project: core/ (pure rules), cli/ (the binary)
  etc/Dockerfile        # the hub image
  etc/e2e/              # the node image of `make e2e`
  tools/                # scripts called by the Makefile
  Makefile              # `make check` is what CI runs
  LICENSE               # Apache-2.0
```

## 1. Principles

1. **Enforced on the node.** Roles, allowed paths, argument validation, redaction and output limits
   live in the node's `limen`. The hub only relays.
2. **The MCP never changes anything.** It holds a key that opens the read role only. Changes need
   another key, held by CI or a person.
3. **Nothing listening on the nodes.** `sshd` is the only way in; `limen` starts per request and exits.
4. **Scripts are the extension point.** An executable file in a directory becomes a tool; limen needs
   no new code to learn a new check.
5. **Bounded output.** Every response has a size limit and says when it was truncated: an agent's
   context is finite, and an unbounded log fills it.
6. **One binary, no runtime.** Kotlin/Native; the same file is the hub and the node side.

## 2. Components

```
MCP client ──stdio or HTTP──▶ hub: limen mcp | limen serve
                                │  ssh limen-read@node   (request as JSON on stdin)
                                ▼
node:  sshd ──forced command──▶ sudo limen gate --role read ──▶ systemctl, journalctl, docker, files, checks
                                (answers JSON on stdout and exits)
```

- **Hub**: runs the MCP server, holds the read role's SSH key and the list of nodes. A laptop
  (`limen mcp`, stdio) or a container (`limen serve`, HTTP).
- **Node**: a machine being inspected. Has the `limen` binary, its configuration in `/etc/limen/`,
  and the SSH and sudo wiring that `limen install` sets up.
- A hub can be a node too: it reaches itself over SSH like any other.

## 3. Roles

| Role | System user | Key held by | Can |
|---|---|---|---|
| `read` | `limen-read` | the hub | Read requests (§5) and checks (§6) |
| `deploy` | `limen-deploy` | CI or a person | `sync`, `apply` and actions (§6) |
| admin | any sudoer | a person on the node | Everything, locally |

- One user per role, so a flaw in one role's wiring can't reach the other. `deploy` is optional:
  `limen install` sets it up only when given a key.
- Each user's `authorized_keys`:
  `restrict,command="sudo -n /usr/local/bin/limen gate --role <role>"`, plus `from="<cidr>"` when given.
- `sudoers`: each user may run exactly that command as root, nothing else.
- `gate` runs as root because every unit's journal, the Docker socket and root-owned configuration
  need it. The boundary is the `gate` code, not Unix permissions: it never runs a shell, never opens
  a path outside the allowlist and never runs a script outside the script directories.
- The users' login shell is `/bin/sh`: `sshd` runs forced commands through it, and with `nologin`
  nothing runs. `restrict` and the forced command are what prevent an interactive session.
- **OpenWrt** has dropbear, no sudo and no tools to add users. Both keys go in root's
  `/etc/dropbear/authorized_keys`, each with `no-port-forwarding,no-agent-forwarding,no-X11-forwarding,
  no-pty,command="/usr/bin/limen gate --role <role>"`; the other keys there are left alone. dropbear has
  no `from=`: the firewall limits who reaches it.
- A forced command only holds a login by key. Where password logins are open and root has no
  password —OpenWrt's default— anyone reaching SSH is root without a key, and `install` says so.

## 4. Node protocol

One request per SSH session: a JSON object on stdin, a JSON object on stdout. `SSH_ORIGINAL_COMMAND`
is ignored.

```json
{"v": 1, "request": "logs", "args": {"source": "unit", "name": "nginx.service", "since": "1h", "lines": 200}}
```

```json
{"ok": true, "data": {}, "truncated": false}
{"ok": false, "error": {"code": "denied", "message": "/etc/shadow is not readable"}}
```

- Stdin, not the command line: no word splitting, typed arguments, and `sudo` passes stdin through
  while it drops the environment `SSH_ORIGINAL_COMMAND` lives in.
- Arguments are validated against the request's schema before anything runs; unknown fields are
  rejected.
- Error codes: `bad_request`, `denied`, `not_found`, `unavailable` (e.g. no Docker on the node),
  `timeout`, `unsupported_version`, `internal`. The hub adds `unreachable` and `host_key_mismatch`
  from `ssh`'s exit status 255.
- `v` is the protocol version. A node answers `unsupported_version` with the versions it speaks, so
  the hub can say "update limen on <node>" instead of failing obscurely.
- Requests of the `deploy` role stream the scripts' output as text and exit with their result: they
  are for people and CI, not for the MCP.

## 5. Read requests and MCP tools

Each read request is an MCP tool with an extra `node` argument. Every tool is annotated
`readOnlyHint: true`.

| Tool | Returns |
|---|---|
| `nodes` | Configured nodes, reachability, limen version, OS, and each node's catalog (§6) |
| `status` | Uptime, load, memory, disk usage per mount, failed units, containers not running or unhealthy, pending reboot |
| `services` | systemd units, filtered by state and name pattern |
| `service` | One unit: state, sub-state, result, since, restarts, main PID, memory, unit file, enablement, and its last journal lines |
| `containers` | Docker containers: name, image, state, health, restarts, start time, compose project |
| `container` | One container: image and digest, state, health log, mounts, ports, networks, labels, restart policy. Environment variables by name only, never their values |
| `logs` | The journal (by unit, priority), a container's logs, or an allowed file. `since`, `until`, `lines`, `grep` |
| `read_file` | An allowed file, by line range |
| `list_dir` | An allowed directory: names, types, sizes, owners, modes, modification times |
| `processes` | Top processes by CPU or memory |
| `ports` | Listening sockets and their processes |
| `history` | The node's audit log (§8) |
| `state` | The repository commit on the node against the remote, and every service `node.toml` expects with whether it runs (§6.1) |
| `check_<name>` | One per check script (§6), with its declared arguments |

- Data comes from commands with machine-readable output (`systemctl show`, `journalctl -o json`,
  `docker … --format '{{json .}}'`, `ss`), run without a shell.
- `grep` is a fixed string, case-insensitive, and filters before `lines` applies: the answer is the
  last N matching lines in the window. The journal filters itself (`journalctl --grep`); containers and
  files are scanned over their last `logs.scan_lines` lines.
- `lines` above `logs.max_lines` is cut to it, and the answer says it was truncated.
- A `check_<name>` tool accepts only the nodes whose catalog has that check. The same check name on
  several nodes must declare the same arguments; if it doesn't, the hub reports the conflict in
  `nodes` and exposes no tool for it until it's fixed.
- Actions and setup scripts appear in `nodes`, so the agent knows they exist and can suggest them,
  but they are never tools.

## 6. Scripts

Three directories on the node, set in `/etc/limen/limen.toml`:

| Kind | Default directory | Run with | Role |
|---|---|---|---|
| check | `/etc/limen/checks.d/` | an MCP tool, or `limen check` | `read` |
| action | `/etc/limen/actions.d/` | `limen action <name>` | `deploy` |
| setup | `/etc/limen/setup.d/` | `limen apply`, all of them in order | `deploy` |

- **Setup** scripts are named `NN-name` and run in lexical order: from a bare machine to a working
  one, or to restore one. Each must be idempotent. `apply` stops at the first failure, and
  `--from NN` resumes there.
- **Checks must not change state.** limen can't verify that; what it guarantees is that the MCP only
  runs scripts already in the directory, with validated arguments — never code it sends.
- A script's name is its file name without extension, `^[a-z0-9][a-z0-9_-]{0,47}$`. Other files are
  ignored and reported by `limen lint`.
- Each script declares itself in a header: the leading comment lines starting with `#:` form a TOML
  document. No header, no tool.

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
```

- Argument types: `int` (`range`), `bool`, `enum` (`values`), `string` (`pattern`, by default
  `^[A-Za-z0-9._-]{1,64}$`). An argument is required unless it has a `default` or says
  `required = false`. They reach the script as environment variables `LIMEN_ARG_<NAME>`, next to
  `LIMEN_KIND`, `LIMEN_SCRIPT` and `LIMEN_NODE`.
- The header is parsed, never executed: learning what an action does must not run it.
- Checks follow the Nagios plugin convention, so existing monitoring plugins work unchanged: exit
  `0` ok, `1` warn, `2` fail, `3` unknown; the first line of stdout is the summary, the rest is detail.
- Execution: no shell, a clean environment (`PATH`, `LANG=C.UTF-8`, `LIMEN_*`), cwd `/`, stdin
  `/dev/null`, `SIGTERM` on timeout and `SIGKILL` after a grace period, output capped.
- A script that isn't owned by root, or is writable by group or others, is refused, and so is one in
  such a directory — the same rule as `sshd`'s `StrictModes`. Run as another user (developing limen),
  that user's files count as root's.
- `apply` validates every setup script before running the first: a broken one halfway would leave
  the machine half done.

### 6.1 A repository per node

A node can take everything it runs from a folder of a Git repository — `[repo]` in `limen.toml`:

```
nodes/hades/
  node.toml                     # what must be running
  checks/  actions/  setup/     # the scripts, as in §6
  stacks/immich/compose.yaml    # Docker Compose stacks
```

```toml
[expect]
compose = ["immich", "passbolt"]      # stacks/<name>/compose.yaml
units = ["docker.service"]            # systemd units that must be active
procd = ["dnsmasq", "firewall"]       # OpenWrt services that must have a running instance
```

- **Scripts say how to get there; `node.toml` says what must run.** limen installs nothing itself:
  packages, unit files and uci settings come from setup scripts. The one thing it runs on its own is
  `docker compose up -d --remove-orphans` for each declared stack, because it is the same everywhere.
- `sync` leaves the checkout (`/opt/limen/repo` by default) exactly as the remote branch: fetch,
  `reset --hard`, `clean`. The repository is the source of truth; local edits are discarded.
- `apply` is sync, the setup scripts in order, the stacks, and then `state`: it fails when an
  expected service is not running, so CI sees it.
- `state` (read role, an MCP tool) compares the deployed commit with the remote (`git ls-remote`:
  nothing on the node changes) and every expected service with what runs.
- With `[repo]`, the script directories default to the node's folder; `[scripts]` still overrides.
- **Whoever can push to that branch runs code as root on the node**, and the MCP can run its checks.
  The branch must be protected.
- A private repository over `https://` needs a token (§10, `install`). It lives in
  `/etc/limen/repo-token` (root, `0600`, never readable through limen) and reaches git through its
  environment as an HTTP header, never in a URL or an argument.

## 7. Configuration

### 7.1 Node: `/etc/limen/limen.toml`

```toml
[files]
allow = ["/etc/nginx/**", "/opt/stacks/*/compose.yaml", "/var/log/nginx/*.log"]
deny = ["**/*.env"]
max_bytes = 262144

[logs]
max_lines = 2000
scan_lines = 100000

[limits]
max_response = 1048576

[redact]
patterns = ['sk-[A-Za-z0-9]{20,}', 'pin (?<secret>\d{4})']

[scripts]
checks = "/etc/limen/checks.d"
actions = "/etc/limen/actions.d"
setup = "/etc/limen/setup.d"

[audit]
path = "/var/log/limen/audit.jsonl"
max_bytes = 5242880
runs = "/var/log/limen/runs"

[repo]
url = "https://github.com/xoadev/cloud.git"
branch = "main"
path = "nodes/hades"
dir = "/opt/limen/repo"
token_file = "/etc/limen/repo-token"
```

- Every key has the default shown. An unknown key is an error, so a typo fails instead of being
  ignored; a broken file makes every request answer `internal` with the reason.
- An answer bigger than `limits.max_response` is replaced by a `bad_request` asking to narrow it.

- **Nothing is readable by default.** `files.allow` starts empty; `limen install` writes it with
  suggestions commented out. Everything readable ends up in a model provider's context, so the
  operator decides it path by path.
- A built-in deny list applies on top and can't be overridden: `/etc/shadow`, `/etc/gshadow`,
  `/etc/sudoers*`, SSH private keys, `/etc/ssl/private/`, `/etc/wireguard/`, NetworkManager
  connections, `/etc/limen/`, `/root/`, and the pseudo-filesystems `/proc/`, `/sys/` and `/dev/`,
  where a "file" can be a process's environment or a whole disk.
- Only regular files are read; a binary file (a NUL in its first 8 KiB) answers its size and no
  content.
- Paths are resolved (symlinks, `..`) before matching, and the resolved path is the one opened: a
  link from an allowed place to a denied one stays denied.
- Redaction applies to files, logs, check output and process and container command lines. The
  built-in patterns catch `key=value` for the usual names of secrets, `Authorization` headers,
  credentials in URLs and PEM private keys; `redact.patterns` adds to them, and a group named `secret`
  limits what is replaced. It is a safety net; the protection is not allowing files that hold secrets.
- A container's environment is never read: `container` lists variable names only.

### 7.2 Hub: `$LIMEN_HOME/limen.toml`

```toml
[ssh]
identity = "id_ed25519"
connect_timeout = "5s"
request_timeout = "60s"
per_node_concurrency = 4

[http]
listen = "127.0.0.1:7341"
origins = []

[nodes.hades]
host = "100.64.0.2"
host_key = "ssh-ed25519 AAAA…"

[nodes.persephone]
host = "100.64.0.3"
port = 2222
host_key = "ssh-ed25519 AAAA…"
```

- `host_key` is required: limen writes its own `known_hosts`, filing each key under the node's name
  (`HostKeyAlias`), and runs `ssh` with `StrictHostKeyChecking=yes`. There is no trust on first use:
  `limen install` prints the node's key, or get it with `ssh-keyscan` and verify it out of band. A
  node that changes address keeps its key.
- `user` defaults to `limen-read` and `port` to 22.
- `LIMEN_HOME` defaults to `~/.limen`; the image sets `/data`.
- Environment overrides: `LIMEN_TOKEN` (§9) and `LIMEN_LISTEN`.

## 8. Audit

- Every request `gate` receives is appended to `/var/log/limen/audit.jsonl` (root, `0600`): time,
  role, request, arguments, client address (`SSH_CONNECTION`), result code and duration. Past
  `audit.max_bytes` it becomes `audit.jsonl.1`: no logrotate, which OpenWrt doesn't have.
- `apply` and actions also keep their full output in `/var/log/limen/runs/<time>-<name>.log`.
- The hub logs each tool call to stderr (node, tool, result, duration): over stdio, stdout is the
  protocol; in the image, stderr is the container's log.

## 9. Transports

- **stdio** — `limen mcp`: newline-delimited JSON-RPC 2.0, one client. For a hub that is the
  operator's own machine.
- **HTTP** — `limen serve`: MCP Streamable HTTP on `POST /mcp`, JSON responses, no SSE (every call
  is a request and a response).
  - Requires a bearer token (`LIMEN_TOKEN`, 16 characters or more) and refuses to start without one.
    It is compared in constant time.
  - Validates `Origin`, against DNS rebinding.
  - Listens on `127.0.0.1:7341` unless told otherwise. The image listens on every interface;
    whoever publishes the port decides who gets in.
- Catalogs are fetched when a session starts and when `nodes` is called. A change is announced with
  `notifications/tools/list_changed` over stdio; over HTTP without SSE it shows up in the next
  session.

## 10. CLI

| Command | Where | What |
|---|---|---|
| `limen mcp` | hub | MCP over stdio |
| `limen serve` | hub | MCP over HTTP |
| `limen call <node> <request> [--arg k=v]…` | hub | One request over SSH; prints the JSON. Checks as `check_<name>`. For `apply` and `action`, with the deploy role's `--user` and `--identity`, it streams their output: what CI runs |
| `limen gate --role <role>` | node | The forced command. Not for people |
| `limen install --read-key <key> [--deploy-key <key>] [--from <cidr>] [--repo <url> [--branch b] [--path p]] [--dry-run]` | node, root | Debian: binary in `/usr/local/bin`, users, `authorized_keys`, `sudoers` (validated with `visudo -c` first). OpenWrt: binary in `/usr/bin`, root's dropbear keys, sysupgrade keep list. Both: `/etc/limen/`; with `--repo`, the repository (below). Idempotent |
| `limen uninstall [--purge]` | node, root | Undoes `install`; keeps `/etc/limen/`, the logs and the checkout unless `--purge` |
| `limen token` | node, root | Asks for the repository token again (when it expires), checks it and saves it |
| `limen sync` | node | The checkout to the remote branch |
| `limen apply [--from NN] [--no-sync] [--dry-run]` | node | Sync, setup scripts in order, stacks, `state` |
| `limen action <name> [--arg k=v]…` | node | One action |
| `limen check <name> [--arg k=v]…` | node | One check; exits with its code |
| `limen lint` | node | Script names, headers and permissions, without running anything |
| `limen version` | both | The version |

`install` also reads `sshd -T` and warns when `AllowUsers` or `AllowGroups` would keep the new
users out.

`install --repo` first tries the repository without a token. When it needs one it prints GitHub's
template link for a fine-grained token —name, owner, no expiry and read-only contents already
filled in; the repository has to be chosen in the form— asks for the token without echoing it, checks
it with `git ls-remote`, saves it, and syncs. `--path` defaults to `nodes/<hostname>`.

The deploy role over SSH (`sync`, `apply`, `action` through `limen call`) never uses the multiplexed
connection: on OpenWrt both roles log in as root, and a deploy request would ride the socket the read
key opened.

## 11. Distribution and platforms

- **One static binary per architecture**, `amd64` and `arm64`, for any Linux: it carries its own glibc,
  so it runs the same on Debian, Alpine or OpenWrt (musl), whatever their libc version.
- Hub image `ghcr.io/xoadev/limen`, one tag for `amd64` and `arm64`: `debian:trixie-slim` with
  `openssh-client` and the binary. Volume `/data` holds `limen.toml` and the SSH key.
- **Releases** follow the conventional commits: every push to `main` rewrites a draft release with the next
  `X.Y.Z` and its changes; publishing it creates the `vX.Y.Z` tag, which builds the binaries (release variant,
  checked static) and the image, starts the image on both architectures, and publishes them with
  `SHA256SUMS`, once `make check` is green on that commit. The version exists only from the tag:
  `limen --version` says `X.Y.Z · build <run> · <date>`, or `dev · <day>` for a local build.
- Nodes:
  - Debian and Ubuntu with systemd and OpenSSH.
  - OpenWrt with procd, dropbear and `logread`. `git` (the `git-http` package) only for `[repo]`.
  - `hello` says which: `init` is `systemd`, `procd` or `none`. Docker is optional everywhere:
    without it, container requests answer `unavailable`.
- Processes, sockets and filesystems are read from `/proc` and `statvfs`, never through `ps`, `ss` or
  `df`, which differ between distributions and busybox.
- Not supported: init systems other than systemd and procd, macOS, Windows.

## 12. Implementation

- Kotlin/Native (`linuxX64`, `linuxArm64`) built with the Kotlin Toolchain.
- Modules:
  - `core`: protocol types, request schemas, configuration, script headers, path policy and
    redaction. Pure, tested without a machine.
  - `cli`: the binary — processes, SSH, MCP and HTTP.
- MCP: own JSON-RPC 2.0 implementation, no SDK. HTTP: Ktor server (CIO). JSON: kotlinx.serialization.
- **Static link.** Kotlin/Native only targets glibc, and links against it dynamically. `tools/ld-static`
  turns that link into a static one (`-static`, `crtbeginT.o`, `libgcc_eh`, `libpthread` whole); the
  compiler takes it as its linker because `tools/kt` registers it among the compiler's own dependencies.
- **No NSS.** Static glibc can't load the plugins behind `getpwnam`, `getpwuid`, `getgrgid` or name
  resolution: limen reads `/etc/passwd` and `/etc/group` itself and resolves no names on the nodes
  (`git` and `ssh` do).
- Processes: `posix_spawn` with an argument array, never `popen` or a shell; own process group, a
  clean environment, stdout and stderr on separate pipes read with a cap; timeouts send `SIGTERM` to
  the group, then `SIGKILL`.
- SSH: the system `ssh` binary with `BatchMode=yes`, `IdentitiesOnly=yes` and connection
  multiplexing (`ControlMaster`, sockets in `$XDG_RUNTIME_DIR/limen-<uid>` or `/tmp/limen-<uid>`).
- TOML: own parser of the subset limen uses (tables, dotted and quoted keys, strings, integers,
  booleans, arrays), so every error names its key and line.

## 13. Threat model

| Threat | Outcome |
|---|---|
| Hub, token or MCP client compromised | Reads what the nodes allow. Changes nothing: it holds no key for that |
| Prompt injection through logs or files | The same: the model can only ask for more reads |
| Argument injection | Arguments are typed, validated on the node and never reach a shell |
| Symlink from an allowed path to a secret | Resolved before matching (§7.1) |
| Expensive requests | Timeouts, output caps and per-node concurrency |
| A malicious or buggy check script | **Not covered.** Scripts belong to root; limen trusts them |
| Secrets inside allowed files | Partly: redaction is best-effort |
| Data leaving the machine | **By design**: whatever is readable reaches the model provider |

## 14. Decisions

| Decision | Alternative | Why |
|---|---|---|
| Limits on the node | Policy in the MCP server | Existing SSH MCP servers filter commands on the client side: a compromised or deceived client then has a shell |
| SSH with a forced command | An agent daemon per node | `sshd` already authenticates and encrypts; a daemon adds a port, its own auth and its own updates |
| Request on stdin | Arguments in `SSH_ORIGINAL_COMMAND` | No word splitting, and `sudo` keeps stdin but drops that variable |
| The system `ssh` | An SSH library | None exists for Kotlin/Native; the system client brings agent support, multiplexing and configuration |
| The MCP holds no key that changes anything | Actions as tools behind approval | An instruction injected in a log can't become a change if no key allows one |
| Nagios exit codes for checks | An own format | Existing monitoring plugins work as they are |
| Empty allowlist by default | A broad default such as `/etc/**` | `/etc` holds Wi-Fi passwords, VPN keys and TLS keys |
| Kotlin/Native | JVM, Go | One binary without a runtime |
| `posix_spawn` | `fork` + `execve` | After a fork only async-signal-safe calls are allowed until `exec`, and the Kotlin/Native runtime is not one of them |
| Own TOML parser | ktoml | Tables named by the operator (`[nodes.<name>]`, `[args.<name>]`) map badly onto a deserializer, and errors must name the key |
| A static binary | A glibc bundle (loader and libraries next to the binary) | Both run on musl; the static one is one file, and the node needs no directory of libraries |
| `/proc` and `statvfs` | `ps`, `ss`, `df` | busybox's versions lack the options, and the formats differ between distributions |
| Scripts converge, `node.toml` declares | limen installing packages and services | limen would become a configuration manager for every distribution; scripts already know how |
| A fine-grained token for private repositories | A deploy key per node | One link fills in the token; a deploy key needs an SSH client for git on OpenWrt and a manual step in GitHub per node |

## 15. Open questions

- Authentication of the HTTP hub beyond one shared token: named tokens per client, with the nodes each
  may see; OAuth 2.1 only if the hub is ever reachable from outside the VPN.
- The static `arm64` binary on hardware: under qemu-user, glibc's `posix_spawn` fails (qemu's `clone`),
  so it is only tested to start.
- `state` of a stack compares services and running containers, not images.
- Signed commits required for `[repo]`.
- Two scripts with the same name and different extensions: today both are listed and `find` takes
  the first.
- Following logs (`follow`): v1 only answers bounded windows.
- Podman besides Docker.
- A `.deb` package besides `limen install`.
