# limen — specification v0.1 (draft)

An MCP server for inspecting Linux machines: configuration files, logs, systemd services, Docker
containers and the operator's own checks. **It is for an agent to look, not to change**: it cannot
change anything, and neither can anyone who gets hold of it. Changes are made by a person, CI or a
configuration manager such as Ansible, never by the agent directly; it proposes them.

For operators without a configuration manager, the same binary can also run the scripts that change
a machine —setting it up from scratch, restoring it, one-off actions—, under a role the MCP server
never holds (§3, §6.1). That part is optional.

**The machine decides.** Every limit is enforced on the node, by `limen` running as an SSH forced
command. The MCP server, its clients and the model behind them are untrusted: if any of them is
compromised or deceived, the worst outcome is reading what the node already allows to be read.

## 0. Repository

```
limen/
  README.md             # what limen is, install and use
  docs/spec.md          # this spec: the design and the reference, the single source of truth
  docs/openwrt.md       # OpenWrt as a node, and one binary for every Linux
  AGENTS.md             # the working contract (CLAUDE.md points there); CONTRIBUTING.md is its short version
  SECURITY.md           # reporting vulnerabilities, and what counts as one
  install.sh            # the installer piped into `sh` on a new machine or a laptop hub
  Cargo.toml            # the Rust workspace; rust-toolchain.toml pins the compiler and its targets
  crates/limen-core/    # pure rules: protocol, schemas, configuration, policy, redaction, parsers
  crates/limen/         # the binary: node side, hub, SSH, MCP, HTTP
  etc/Dockerfile        # the hub image
  etc/e2e/              # the Debian node image of `make e2e`
  tools/                # scripts called by the Makefile
  .github/              # CI, releases, issue and pull request templates
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
6. **One binary, no runtime.** Rust, linked statically; the same file is the hub and the node side.

## 2. Components

```
MCP client ──stdio or HTTP──▶ hub: limen mcp | limen serve
                                │  ssh limen-read@node   (request as JSON on stdin)
                                ▼
node:  sshd ──forced command──▶ sudo limen gate --role read ──▶ systemctl, journalctl, docker, files, checks
                                (answers JSON on stdout and exits)
```

- **Hub**: runs the MCP server, holds the read role's SSH key and the list of nodes. A container
  (`limen serve`, HTTP) or a laptop (`limen mcp`, stdio). It is a directory —`$LIMEN_HOME`, `/data` in the
  image—, created by `limen init`, or by `limen serve` on its first start.
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
{"ok": true, "data": {}, "truncated": true}
{"ok": false, "error": {"code": "denied", "message": "/etc/shadow is not readable"}}
```

- `truncated` is there only when the answer was cut.
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
| `status` | Uptime, load, memory, disk usage per mount, failed services, containers not running or unhealthy, pending reboot |
| `services` | systemd units, or procd services on OpenWrt, filtered by state and name pattern |
| `service` | One unit: state, sub-state, result, since, restarts, main PID, memory, unit file, enablement, and its last journal lines. On OpenWrt, a procd service's instances and its last `logread` lines |
| `containers` | Docker containers: name, image, state, health, restarts, start time, compose project |
| `container` | One container: image and digest, state, health log, mounts, ports, networks, labels, restart policy. Environment variables by name only, never their values |
| `logs` | The journal (by unit, priority) or `logread` on OpenWrt, a container's logs, or an allowed file. `since`, `until`, `lines`, `grep` |
| `read_file` | An allowed file, by line range |
| `list_dir` | An allowed directory: names, types, sizes, owners, modes, modification times. Only entries that are readable or lead to something readable; at most 1000 |
| `processes` | Top processes by CPU or memory |
| `ports` | Listening sockets and their processes |
| `history` | The node's audit log (§8) |
| `state` | The repository commit on the node against the remote, and every service `node.toml` expects with whether it runs (§6.1) |
| `check_<name>` | One per check script (§6), with its declared arguments |

- Data comes from commands with machine-readable output (`systemctl show`, `journalctl -o json`,
  `docker inspect`, `ubus call`), run without a shell, and from `/proc` and `statvfs` (§12).
- `grep` is a fixed string, case-insensitive, matched against the redacted text —never the raw one, or a
  guess at a secret would be told apart by whether a line comes back— and filters before `lines` applies:
  the answer is the last N matching lines in the window. The journal narrows the search itself
  (`journalctl --grep` over its last `logs.scan_lines` matches); containers and files are scanned over
  their last `logs.scan_lines` lines, and a file over its last 16 MiB at most.
- `lines` above `logs.max_lines` is cut to it, and the answer says it was truncated.
- A node's catalog is not trusted: a check whose name, description, arguments or patterns aren't plain and
  bounded, or that declares a `node` argument, is left out and reported in `nodes`. Its text reaches the model
  as it is, so it is kept short.
- A `check_<name>` tool accepts only the nodes whose catalog has that check; the hub refuses the others
  itself. The same check name on
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
- A script's name is its file name without extension, `^[a-z0-9][a-z0-9_-]{0,47}$`. Other files —a
  README, a misspelt name— are ignored and reported by `limen lint`; dotfiles such as `.gitkeep` aren't
  even listed. Two files with one name (`disk.sh`, `disk.py`) are refused, both, until one goes.
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
  `^[A-Za-z0-9_][A-Za-z0-9._-]{0,63}$`: never an option, `.` or `..`). An argument is required unless it has a `default` or says
  `required = false`. They reach the script as environment variables `LIMEN_ARG_<NAME>`, next to
  `LIMEN_KIND`, `LIMEN_SCRIPT` and `LIMEN_NODE` (the machine's hostname).
- A `pattern`, like `redact.patterns` (§7.1), is a Rust `regex` that must match the whole value: `\w`, `\d`,
  `\s` and `(?i)` are there, Unicode's `\p{…}` classes and look-around are not.
- `timeout` defaults to 60 s for checks and 1 h for actions and setup scripts. Through the MCP, the
  hub waits for a check's own timeout and a margin, not `[ssh].request_timeout`.
- The header is parsed, never executed: learning what an action does must not run it.
- Checks follow the Nagios plugin convention, so existing monitoring plugins work unchanged: exit
  `0` ok, `1` warn, `2` fail, `3` unknown; the first line of stdout is the summary, the rest is detail.
- Execution: no shell, a clean environment (a fixed `PATH`, `LANG` and `LC_ALL` `C.UTF-8`, `TZ=UTC`,
  `HOME=/root`, no pagers or colours, `LIMEN_*`), cwd `/`, stdin `/dev/null`, `SIGTERM` on timeout and
  `SIGKILL` after a grace period. A check's output is capped at 64 KiB; past it the check is stopped
  and answers `unknown`.
- A script that isn't owned by root, or is writable by group or others, is refused, and so is one
  under such a directory, all the way up to `/` — the same rule as `sshd`'s `StrictModes`. Run as another user (developing limen),
  that user's files count as root's. The same rule holds for what root reads and acts on without running it:
  `limen.toml` (where a link leads, if it is one), `node.toml` and each stack's compose file. The gate and
  `install` run with umask `022`.
- `apply` validates every setup script before running the first: a broken one halfway would leave
  the machine half done.

### 6.1 A repository per node

A node can take everything it runs from a folder of a Git repository — `[repo]` in `limen.toml`:

```
nodes/nas/
  node.toml                     # what must be running
  checks/  actions/  setup/     # the scripts, as in §6
  stacks/immich/compose.yaml    # Docker Compose stacks
```

```toml
[expect]
compose = ["immich", "passbolt"]      # stacks/<name>/compose.yaml
units = ["docker.service"]            # systemd units that must be active
procd = ["dnsmasq", "firewall"]       # OpenWrt services that must exist and not have failed
```

- **Optional.** A node managed by Ansible or the like leaves `[repo]` and the deploy role out, or has
  a setup script run `ansible-pull`. `state` and the checks work either way.
- **Scripts say how to get there; `node.toml` says what must run.** limen installs nothing itself:
  packages, unit files and uci settings come from setup scripts. The one thing it runs on its own is
  `docker compose up -d --remove-orphans` for each declared stack, because it is the same everywhere.
  The project is named after the stack; its file is `compose.yaml` (or `compose.yml`, `docker-compose.*`). A stack runs when every service has a container and each is
  running and not unhealthy, or exited with 0: a one-shot job that finished is not a stack that is down.
- `sync` leaves the checkout (`/opt/limen/repo` by default) as the remote branch: a shallow fetch from
  the URL in `limen.toml` (changing it moves the node), `reset --hard`, and `clean` of untracked files.
  The first clone takes `repo.dir` only if it doesn't exist or is empty: anything else there is not limen's to
  remove, and `uninstall --purge` removes it only if it is a checkout. `repo.dir` is a plain path, without
  `.` or `..`, and `repo.url` carries no credentials.
  The repository is the source of truth; local edits are discarded. **Ignored files stay**: a stack's
  `.env` with its secrets lives next to its `compose.yaml`, listed in `.gitignore`.
- `apply` is sync, the setup scripts in order, the stacks, and then `state`: it fails when an
  expected service is not running, so CI sees it. Each setup script's output is also kept in
  `/var/log/limen/runs/`.
- `state` (read role, an MCP tool) compares the deployed commit with the remote (`git ls-remote`:
  nothing on the node changes) and every expected service with what runs.
- With `[repo]`, the script directories default to the node's folder; `[scripts]` still overrides.
- **Whoever can push to that branch runs code as root on the node**, and the MCP can run its checks.
  The branch must be protected.
- A private repository over `https://` needs a token (§10, `install`). It lives in
  `/etc/limen/repo-token` (root, `0600`, never readable through limen) and reaches git through its
  environment as an HTTP header, never in a URL or an argument. That needs git 2.32 or later; an older one is
  refused rather than silently sending nothing.

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
concurrency = 8

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
url = "https://github.com/you/infra.git"
branch = "main"
path = "nodes/nas"
dir = "/opt/limen/repo"
token_file = "/etc/limen/repo-token"
```

- The limits, directories and paths show their defaults. `files`, `redact` and `repo.path` are
  examples: they default to empty, and `[repo]` needs its `url`. `deny = ["**/*.env"]` is what
  `limen install` writes, not a default. With `[repo]`, `scripts.*` default to the node's folder (§6.1).
- An unknown key is an error, so a typo fails instead of being ignored; a broken file makes every
  request answer `internal` with the reason.
- `limen.toml` may be a link to a file kept elsewhere; limen reads and rewrites the file it leads to.
- An answer bigger than `limits.max_response` is replaced by a `bad_request` asking to narrow it.
- The node bounds its read requests itself, whatever hub sends them: at most `limits.concurrency` at once (a lock
  on one of `/run/limen/slot-<n>`; one more gets `unavailable`), ten seconds to send the request, and two minutes
  in all —a check, its own timeout on top—. A request out of time has what it started killed, answers `timeout`
  and is audited as such. `read_file` goes at most 64 MiB into a file to reach its first line; `logs` with
  `source = file` reads the end.

- **Nothing is readable by default.** `files.allow` starts empty; `limen install` writes it with
  suggestions commented out. Everything readable ends up in a model provider's context, so the
  operator decides it path by path.
- A built-in deny list applies on top and can't be overridden: `/etc/shadow`, `/etc/gshadow` and their
  backups in `/var/backups/`, `/etc/sudoers*`, SSH and dropbear private keys, `/etc/ssl/private/`,
  `/etc/wireguard/`, NetworkManager connections, OpenWrt's `/etc/config/wireless`, systemd's
  `/run/credentials/`, `/etc/limen/`, `/var/log/limen/`, `/root/`, and the pseudo-filesystems `/proc/`,
  `/sys/` and `/dev/`, where a "file" can be a process's environment or a whole disk. So are the audit log,
  the runs' directory and the repository token wherever `[audit]` and `[repo]` put them.
- Only regular files with a single hard link are read: another name for the same file could be anywhere,
  and a hard link into an allowed directory would carry a denied file with it. A binary file (a NUL in its
  first 8 KiB) answers its size and no content.
- Paths are walked one component at a time, as the kernel does: links followed, `..` taken from where
  the walk is. Every step must be allowed or lead to something allowed, so nothing is learnt of a place
  the policy hides, not even whether it exists; past a missing component the walk goes on as text. The
  walked path is the one opened, once, without following links and checked through `/proc/self/fd`, and
  the type, links and keys are checked on what was opened: a directory swapped for a link between the
  check and the open is caught. A refusal names the path as asked and gives no reason, which would tell
  where it leads. `list_dir` reads its entries through the directory it opened.
- A file of up to 1 MiB that holds a PEM private key is not read at all: a range of lines can fall
  between the key's markers, where redaction can't recognise it. In larger files, logs, a window is
  redacted as a whole, so a key it holds entire is masked.
- Redaction applies to files, logs, check output, process and container command lines, and
  `history`, and to unit descriptions and container errors. The built-in patterns catch `key=value` for the
  usual names of secrets (a quoted value up to its closing quote), UCI's `option key '…'`, `Authorization`
  headers, credentials in URLs, `curl -u`, `sshpass -p`, `mysql -p`, PEM private keys and lines of base64
  alone, as a key's body is written;
  `redact.patterns` adds to them, and a group named `secret` limits what is replaced. It is a safety
  net; the protection is not allowing files that hold secrets.
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

[nodes.nas]
host = "100.64.0.2"
host_key = "ssh-ed25519 AAAA…"

[nodes.router]
host = "100.64.0.3"
port = 2222
host_key = "ssh-ed25519 AAAA…"
```

- The directory holds `id_ed25519` (the hub's key, made with `ssh-keygen` by `init`), `limen.toml`, `token`
  (for HTTP clients, unless `LIMEN_TOKEN` is set) and `invites/`. `limen.toml` is read again whenever it changes: a
  node that joins is there for the next request. `[http]` is read once, when `serve` starts. A broken
  `limen.toml` makes the MCP answer errors, not stop.
- `[http] public_url` (or `LIMEN_PUBLIC_URL`) is where nodes reach the hub to join: an address, not a name,
  because the static binary resolves no names.
- `host_key` is required: limen writes its own `known_hosts`, filing each key under the node's name
  (`HostKeyAlias`), and runs `ssh` with `StrictHostKeyChecking=yes`. There is no trust on first use:
  `limen install` prints the node's key, or get it with `ssh-keyscan` and verify it out of band. A
  node that changes address keeps its key.
- `user` defaults to `limen-read` and `port` to 22.
- `LIMEN_HOME` defaults to `~/.limen`; the image sets `/data`.
- Environment overrides: `LIMEN_TOKEN` (§9) and `LIMEN_LISTEN`.

## 8. Audit

- Every request `gate` receives is appended to `/var/log/limen/audit.jsonl` (root, `0600`): time,
  role, request, arguments, client address (`SSH_CONNECTION`), sudo's user, result code and duration.
  That includes malformed ones, those of an unknown version, and those that come while the
  configuration is broken (to the default path then). Arguments over 4 KiB are replaced by their size.
  Past `audit.max_bytes` it becomes `audit.jsonl.1`: no logrotate, which OpenWrt doesn't have.
- `history` shows it redacted, like everything that leaves the node. Action arguments are recorded:
  don't pass secrets as arguments.
- Each setup script and action also keeps its full output in `/var/log/limen/runs/<time>-<name>.log`.
- The hub logs each tool call to stderr (node, tool, result, duration): over stdio, stdout is the
  protocol; in the image, stderr is the container's log.

## 9. Transports

- **stdio** — `limen mcp`: newline-delimited JSON-RPC 2.0, one client. For a hub that is the
  operator's own machine.
- **HTTP** — `limen serve`: MCP Streamable HTTP on `POST /mcp`, JSON responses, no SSE (every call
  is a request and a response).
  - Requires a bearer token, compared in constant time. `serve` creates one on its first start, or takes
    `LIMEN_TOKEN`, which must have 16 characters or more.
  - Its own small HTTP/1.1 server, one request per connection: at most 64 connections, 16 KiB of head
    and 15 seconds for the whole request. The token and `Origin` are checked before the body is read, and a body
    is read only with its `Content-Length`, up to 1 MiB (16 KiB for a join): `Transfer-Encoding` gets `411`.
  - Validates `Origin`, against DNS rebinding.
  - Listens on `127.0.0.1:7341` unless told otherwise. The image listens on every interface;
    whoever publishes the port decides who gets in.
- `GET /join/<code>` and `POST /join/<code>` (§10.1) need no token: the one-time code is the authorisation.
- Catalogs are fetched when the set of nodes changes, and when a session starts (`initialize`) or `nodes`
  is called, at most once every ten seconds. A change is announced with `notifications/tools/list_changed` over stdio; over HTTP
  without SSE it shows up in the next session.

## 10. CLI

| Command | Where | What |
|---|---|---|
| `limen init [--serve]` | hub | The hub's directory: key, `limen.toml`, and with `--serve` the token. Keeps what exists |
| `limen mcp` | hub | MCP over stdio |
| `limen serve` | hub | MCP over HTTP and the join endpoints; `init --serve` first if needed |
| `limen connect [--url]` | hub | The `claude mcp add` line: HTTP when there is a public URL and a token, stdio otherwise |
| `limen invite <name> [--ttl 1h]` | hub | The line that joins a machine (§10.1) |
| `limen trust <name> <address> <host-key> [--user] [--port]` | hub | A node added by hand |
| `limen forget <name>` | hub | A node taken off the hub |
| `limen join <line> \| --hub-key <key> --name <name> [--repo …] [--deploy-key …] [--from …] [--address …] [--ssh-port …]` | node, root | Joins the hub (§10.1): install with the hub's key, then report. `--path` defaults to `nodes/<its name on the hub>` |
| `limen call <node> <request> [--arg k=v]…` | hub | One request over SSH; prints the JSON. Checks as `check_<name>`; a script's arguments are typed by its header, from the node's catalog. For `apply` and `action`, with the deploy role's `--user` and `--identity`, it streams their output: what CI runs |
| `limen gate --role <role>` | node | The forced command. Not for people |
| `limen install --read-key <key> [--deploy-key <key>] [--from <cidr>] [--repo <url> [--branch b] [--path p]] [--dry-run]` | node, root | Debian: binary in `/usr/local/bin`, users, `authorized_keys`, `sudoers` (validated with `visudo -c` first). OpenWrt: binary in `/usr/bin`, root's dropbear keys, sysupgrade keep list. Both: `/etc/limen/`; with `--repo`, the repository (below). Idempotent |
| `limen uninstall [--purge]` | node, root | Undoes `install`; keeps `/etc/limen/`, the logs and the checkout unless `--purge` |
| `limen token` | node, root | Asks for the repository token again (when it expires), checks it and saves it |
| `limen sync` | node | The checkout to the remote branch |
| `limen apply [--from NN] [--no-sync] [--dry-run]` | node | Sync, setup scripts in order, stacks, `state` |
| `limen action <name> [--arg k=v]…` | node | One action |
| `limen check <name> [--arg k=v]…` | node | One check; exits 0, 1 or 2 by its status, and 3 when it gave none (killed, timed out, not found) |
| `limen lint` | node | Script names, headers and permissions, without running anything; files that aren't scripts are listed as skipped |
| `limen version` | both | The version |

`install` also reads `sshd -T` and warns when `AllowUsers` or `AllowGroups` would keep the new
users out.

`install.sh`, at the root of the repository, is the way in (`curl … | sudo sh`, or `wget` on OpenWrt). POSIX
`sh`, because OpenWrt has only busybox's `ash`. It downloads the binary of the latest release for `uname -m` and
checks it against `SHA256SUMS`, then:

- `--join <line>`: asks only for the repository (optional), offers git if that needs it, and runs `limen join` with
  the terminal as its input, so the token prompt works under a pipe.
- `--hub-key <key> --name <name>`: the same, for a hub with no HTTP; it ends with the `limen trust` line.
- `--hub`: the binary in `~/.local/bin` (or `/usr/local/bin` as root) and `limen init`, for a laptop hub.

Every answer can come from a `LIMEN_*` variable instead, for unattended installs; the list is at the top of
`install.sh`.

### 10.1 Joining a node

```
hub:   limen invite nas  →  …/install.sh | sudo sh -s -- --join 'http://<hub>:7341/join/<code>#SHA256:<hub key>.<secret>'
node:  GET  /join/<code>  →  {"name": "nas", "hub_key": "ssh-ed25519 …", "proof": "<HMAC>"}   key against the fingerprint, both with the secret
       limen install with that key
       POST /join/<code>  ←  {"host_key": "ssh-ed25519 …", "user": "limen-read", "port": 22, "proof": "<HMAC>"}
hub:   [nodes.nas] with the request's source address, then `hello`  →  {"reachable": true, "detail": "…"}
```

- The code is 26 characters of base32 (130 bits), kept in `invites/<code>.json` with the node's name and a secret
  of the same size; it lasts one hour by default and one successful `POST`.
- The secret travels only in the line's fragment, never on the wire: the node signs its arrival with it
  (HMAC-SHA256 over host key, user, port and address). Whoever sees the code in transit can't arrive in the node's
  place or change what it sends; a forged arrival is refused without spending the invitation. Joins are handled
  one at a time.
- The invitation is signed with the same secret (HMAC-SHA256 over name and key): the name picks the node's
  folder in the repository, and so what it runs as root, so nobody between the two can change it either.
- The fingerprint in the line is `SHA256:<base64>` of the key, as `ssh-keygen -lf` prints it. The key itself is not
  secret; what matters is that it arrives unchanged, and a mismatch stops the node before anything is installed.
- The hub trusts the host key that arrives with a valid code: trust on first use, bound to an invitation a person
  gave out.
- The node's address is where the `POST` comes from, unless it sends `--address`. The hub edits only that node's
  table in `limen.toml`, edited as TOML and not as text, and parses the result before writing it.
- The node doesn't trust the hub more than the hub trusts it: the name it is given must be a node name, and what
  it writes to its own `limen.toml` is written as TOML values, which can't be more than values.
- `limen join` talks HTTP to an address, never a name, so a join line means the same wherever it is pasted.
- Without an HTTP hub, `invite` prints the hub's key in the line (`--hub-key`) and the node prints
  `limen trust <name> <address> '<host key>'` for the hub.

`install --repo` first tries the repository without a token. When it needs one it prints GitHub's
template link for a fine-grained token —name, owner, no expiry and read-only contents already
filled in; the repository has to be chosen in the form— asks for the token without echoing it, checks
it with `git ls-remote`, saves it, and syncs. Only a refusal of credentials asks for a token; any other failure
(network, TLS, a wrong URL) is reported as it is. The repository is checked before anything on the machine
changes. `install --path` defaults to `nodes/<hostname>`.

The deploy role over SSH (`sync`, `apply`, `action` through `limen call`) never uses the multiplexed
connection: on OpenWrt both roles log in as root, and a deploy request would ride the socket the read
key opened.

## 11. Distribution and platforms

- **One static binary per architecture**, `amd64` and `arm64`, for any Linux: linked against musl, it
  needs nothing of the machine but the kernel, and runs the same on Debian, Alpine or OpenWrt. About 3 MB.
- Hub image `ghcr.io/xoadev/limen`, one tag for `amd64` and `arm64`: distroless —no shell, no package manager—
  with the binary and OpenSSH's `ssh` and `ssh-keygen`, taken from Alpine with the libraries they load. It runs as
  distroless's `nonroot` (uid 65532). Volume `/data` holds `limen.toml` and the SSH key.
- **Releases** follow the conventional commits: every push to `main` rewrites a draft release with the next
  `X.Y.Z` and its changes; publishing it creates the `vX.Y.Z` tag, which builds the binaries (release variant,
  checked static) and the image, starts the image on both architectures, and publishes them with
  `SHA256SUMS`, once `make check` is green on that commit. The version exists only from the tag:
  `limen --version` says `X.Y.Z · build <run> · <date>`, or `dev` for a local build.
- No job that runs someone else's code —convco, the compiler and the crates' build scripts, QEMU, BuildKit— holds
  a token that writes, nor keeps credentials on disk, and the release builds without caches. The jobs that write
  run only `gh`, `skopeo` and GitHub's `attest` on what the others handed over; QEMU's and BuildKit's images are
  pinned by digest, the linters by checksum.
- Every binary and the image carry a build provenance attestation, signed keyless through Sigstore:
  `gh attestation verify limen-<version>-linux-x86_64 --repo xoadev/limen` (or `oci://ghcr.io/xoadev/limen:<version>`)
  says they were built by this repository's release workflow, from which commit and tag. `install.sh` can't check
  it —nodes have no `gh`— and checks `SHA256SUMS`.
- `install.sh` downloads only over https, with a client that checks certificates (curl, OpenWrt's
  `uclient-fetch`, GNU wget; never busybox's wget), and runs only once it has been read whole.
- Nodes:
  - Debian and Ubuntu with systemd and OpenSSH.
  - OpenWrt with procd, dropbear and `logread`. `git` (the `git-http` package) only for `[repo]`.
  - `hello` says which: `init` is `systemd`, `procd` or `none`. Docker is optional everywhere:
    without it, container requests answer `unavailable`.
- Processes, sockets and filesystems are read from `/proc` and `statvfs`, never through `ps`, `ss` or
  `df`, which differ between distributions and busybox.
- Not supported: init systems other than systemd and procd, macOS, Windows; 32-bit ARM and MIPS (many routers),
  which have no release binary.

## 12. Implementation

- Rust, for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, linked statically by the
  toolchain's own `rust-lld`: no C compiler, and no cross compiler for arm64.
- Crates:
  - `limen-core`: protocol types, request schemas, configuration, script headers, path policy,
    redaction, the join's formats and the parsers of what system programs print. Pure, tested without a
    machine.
  - `limen`: the binary — processes, files, SSH, MCP and HTTP.
- MCP: own JSON-RPC 2.0 implementation, no SDK. HTTP: `tiny_http` for the hub; `join`'s two requests over
  `std::net`. Neither has TLS. JSON: `serde_json`. CLI: `clap`. System calls: `rustix`; no `unsafe` code.
- Users and groups come from `/etc/passwd` and `/etc/group`, read by limen itself; nodes resolve no host
  names (`git` and `ssh` do).
- Processes: `std::process::Command` with an argument array, never a shell; own process group, a clean
  environment, stdout and stderr on separate pipes read with a cap; timeouts send `SIGTERM` to the group,
  then `SIGKILL`.
- SSH: the system `ssh` binary with `BatchMode=yes`, `IdentitiesOnly=yes` and connection
  multiplexing (`ControlMaster`, sockets in `$XDG_RUNTIME_DIR/limen-<uid>/<hub>` or `/tmp/limen-<uid>/<hub>`, directories of mode `0700` that must be the user's; `-F none`, and no agent or port forwarding).
- TOML: `toml_edit`. Files are read into `#[derive(Deserialize)]` types that refuse unknown keys, so a typo
  fails with its line; the hub's and node's `limen.toml` are edited as documents, comments kept.

## 13. Threat model

| Threat | Outcome |
|---|---|
| Hub, token or MCP client compromised | Reads what the nodes allow. Changes nothing: it holds no key for that |
| A compromised node | Answers what it likes about itself. Its catalog can't name tools or arguments beyond plain, bounded text, nor reach other nodes |
| Someone who reaches the HTTP port without the token | Gets `401` before any body is read. Connections, heads and bodies are bounded; a flood denies service, it doesn't stop the hub |
| Prompt injection through logs or files | The same: the model can only ask for more reads |
| Argument injection | Arguments are typed, validated on the node and never reach a shell |
| Symlink from an allowed path to a secret | Resolved before matching (§7.1) |
| Expensive requests | Timeouts, output caps, and on the node itself a deadline per request and `limits.concurrency` |
| An invitation leaks | Whoever uses it first adds *one* machine, with that name, to the hub —a machine the agent will then read—; one hour, one use |
| Someone between a joining node and the hub | Can't swap the hub's key —the fingerprint in the line stops the node— nor put their machine in the node's place: the arrival is signed with a secret that never travels. The source address is not signed: replaying the arrival from elsewhere files a wrong address, which `StrictHostKeyChecking` then refuses. Denial of service; join again |
| The token leaks | It is in `$LIMEN_HOME/token`, printed by `connect`, and in the container's environment if given as `LIMEN_TOKEN` (`docker inspect`). Reads what the nodes allow; rotate it by replacing the file or the variable |
| A malicious hub at join time | Installs its key for the read role, as the node's owner asked, and names the node, which picks its folder in the repository. Can't write the node's configuration beyond its own values; what it prints is stripped of control characters |
| A key reused for more than limen | `install` refuses a read key equal to the deploy key, or one that already opens root's or dropbear's `authorized_keys` without limen's forced command |
| A hub that floods a node with requests | Gets `unavailable` past `limits.concurrency`. Can rotate old entries out of the node's audit log, which is bounded. Ship it elsewhere if it must outlive that |
| A local user racing the gate | Links swapped in, directories swapped for links, FIFOs and hard links put in allowed directories: the walk, the single open and the checks on the open file refuse them |
| A malicious or buggy check script | **Not covered.** Scripts belong to root; limen trusts them |
| Secrets inside allowed files | Partly: redaction is best-effort |
| Data leaving the machine | **By design**: whatever is readable reaches the model provider |
| A compromised release, image or `install.sh` | Partly: a binary or image replaced by hand, or by a leaked token, fails `gh attestation verify`. `install.sh` checks only `SHA256SUMS`, from the same release, and is served from `main`: whoever can write to the repository, or run its release workflow, can still do this |

## 14. Decisions

| Decision | Alternative | Why |
|---|---|---|
| Limits on the node | Policy in the MCP server | Existing SSH MCP servers filter commands on the client side: a compromised or deceived client then has a shell |
| SSH with a forced command | An agent daemon per node | `sshd` already authenticates and encrypts; a daemon adds a port, its own auth and its own updates |
| Request on stdin | Arguments in `SSH_ORIGINAL_COMMAND` | No word splitting, and `sudo` keeps stdin but drops that variable |
| The system `ssh` | An SSH library | The system client brings agent support, multiplexing and the operator's configuration, and is already on every hub |
| The MCP holds no key that changes anything | Actions as tools behind approval | An instruction injected in a log can't become a change if no key allows one |
| The agent proposes changes; people, CI or Ansible make them | Letting the agent run Ansible | Ansible's key is root on every machine: whoever runs a playbook runs anything |
| An optional, minimal deploy role | A full configuration manager, or none | Rebuilding a machine from a repository matters where nothing else does it; where Ansible does, limen stays out of the way |
| Nagios exit codes for checks | An own format | Existing monitoring plugins work as they are |
| Empty allowlist by default | A broad default such as `/etc/**` | `/etc` holds Wi-Fi passwords, VPN keys and TLS keys |
| Rust | Kotlin/Native (the first implementation), Go | Static musl binaries of about 3 MB built by the toolchain itself, arm64 without a cross compiler, and memory safety without a garbage collector in what runs as root. Kotlin/Native had no musl target: a static glibc needed its own linker script, no NSS, and an own HTTP client where glibc's iconv was missing |
| A static binary | A package per distribution | One file runs on any Linux, and OpenWrt has no package for it |
| Configuration as `serde` types, edited with `toml_edit` | Reading and editing TOML by hand | `deny_unknown_fields` turns a typo into an error with its line; an edited document can't gain a table from a value, and keeps the operator's comments |
| `/proc` and `statvfs` | `ps`, `ss`, `df` | busybox's versions lack the options, and the formats differ between distributions |
| Scripts converge, `node.toml` declares | limen installing packages and services | limen would become a configuration manager for every distribution; scripts already know how |
| Joining with a one-time invitation | Copying keys by hand, or the hub logging into nodes with an administrator's SSH | Nothing to carry but one line; the hub never holds more than its read key |
| A fine-grained token for private repositories | A deploy key per node | One link fills in the token; a deploy key needs an SSH client for git on OpenWrt and a manual step in GitHub per node |

## 15. Open questions

- Authentication of the HTTP hub beyond one shared token: named tokens per client, with the nodes each
  may see; OAuth 2.1 only if the hub is ever reachable from outside the VPN.
- The `arm64` binary runs under qemu-user, children included, but has not run on hardware yet.
- 32-bit routers: an ARMv7 build (`armv7-unknown-linux-musleabihf`, 2.4 MB) links and runs under qemu; MIPS
  needs Rust's nightly. Neither is released.
- `state` of a stack compares services and running containers, not images.
- Signed commits required for `[repo]`.
- Following logs (`follow`): v1 only answers bounded windows.
- Podman besides Docker.
- A `.deb` package besides `limen install`.
