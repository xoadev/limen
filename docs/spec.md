# limen — specification v0.1 (draft)

An MCP server that gives an agent a bounded way into Linux machines. It can read the files a machine allows,
and run the scripts the machine offers — to look, and to change what its operator decided may be changed.
Nothing else: no shell, no command it writes itself.

**limen knows no init system, container runtime or configuration tool.** What a machine can do lives in
scripts, grouped in packs: one for systemd, one for Docker, one for the operator's own services. limen runs
them, validates their arguments, redacts their output and bounds it.

**The machine decides.** Every limit is enforced on the node, by `limen` running as an SSH forced
command. The MCP server, its clients and the model behind them are untrusted: if any of them is
compromised or deceived, the worst outcome is reading what the node allows to be read and running the
scripts it offers.

## 0. Repository

```
limen/
  README.md             # what limen is, install and use
  docs/spec.md          # this spec: the design and the reference, the single source of truth
  docs/scripts.md       # writing scripts and packs: the header, how they run, an example setup
  docs/openwrt.md       # OpenWrt as a node, and one binary for every Linux
  packs/                # example packs, to copy: systemd, debian, docker, openwrt, system
  AGENTS.md             # the working contract (CLAUDE.md points there); CONTRIBUTING.md is its short version
  SECURITY.md           # reporting vulnerabilities, and what counts as one
  install.sh            # the installer piped into `sh` on a new machine or a laptop hub
  Cargo.toml            # the Rust workspace; rust-toolchain.toml pins the compiler and its targets
  crates/limen-core/    # pure rules: protocol, schemas, configuration, script headers, policy, redaction
  crates/limen/         # the binary: node side, hub, SSH, MCP, HTTP
  etc/Dockerfile        # the hub image
  etc/e2e/              # the Debian node image of `make e2e`
  tools/                # scripts called by the Makefile
  .github/              # CI, releases, issue and pull request templates
  Makefile              # `make check` is what CI runs
  LICENSE               # Apache-2.0
```

## 1. Principles

1. **Enforced on the node.** Allowed paths, the scripts on offer, argument validation, redaction and
   output limits live in the node's `limen`. The hub only relays.
2. **Scripts are the whole surface.** Besides reading allowed files, the agent can only run scripts
   already on the node, with arguments their headers accept — never code it sends.
3. **Agnostic.** limen runs scripts; it doesn't parse what systemd, Docker or git print. A new kind of
   machine is a new pack, not a new release.
4. **Nothing listening on the nodes.** `sshd` is the only way in; `limen` starts per request and exits.
5. **Bounded output.** Every response has a size limit and says when it was truncated: an agent's
   context is finite, and an unbounded log fills it.
6. **One binary, no runtime.** Rust, linked statically; the same file is the hub and the node side.

## 2. Components

```
MCP client ──stdio or HTTP──▶ hub: limen mcp | limen serve
                                │  ssh limen@node   (request as JSON on stdin)
                                ▼
node:  sshd ──forced command──▶ sudo limen gate ──▶ allowed files, the packs' scripts
                                (answers JSON on stdout and exits)
```

- **Hub**: runs the MCP server, holds the SSH key and the list of nodes. A container (`limen serve`,
  HTTP) or a laptop (`limen mcp`, stdio). It is a directory —`$LIMEN_HOME`, `/data` in the image—, created
  by `limen init`, or by `limen serve` on its first start.
- **Node**: a machine the agent works on. Has the `limen` binary, its configuration in `/etc/limen/`, its
  packs, and the SSH and sudo wiring that `limen install` sets up.
- A hub can be a node too: it reaches itself over SSH like any other.

## 3. Access

- **One user, `limen`, and one key, the hub's.** Nothing else calls a node: a person on the machine runs
  `limen` as root directly.
- Its `authorized_keys`: `restrict,command="sudo -n /usr/local/bin/limen gate"`, plus `from="<cidr>"` when
  given. `.ssh` and the file belong to root.
- `sudoers`: `limen` may run exactly that command as root, nothing else.
- `gate` runs as root because the journal, the Docker socket and root-owned configuration need it. The
  boundary is the `gate` code, not Unix permissions: it never runs a shell, never opens a path outside the
  allowlist and never runs a file that isn't a script of a configured pack.
- The login shell is `/bin/sh`: `sshd` runs forced commands through it, and with `nologin` nothing runs.
  `restrict` and the forced command are what prevent an interactive session. The password is locked.
- **OpenWrt** has dropbear, no sudo and no tools to add users. The hub's key goes in root's
  `/etc/dropbear/authorized_keys` with `no-port-forwarding,no-agent-forwarding,no-X11-forwarding,no-pty,
  command="/usr/bin/limen gate"`; the other keys there are left alone. dropbear has no `from=`: the firewall
  limits who reaches it.
- A forced command only holds a login by key. Where password logins are open and root has no
  password —OpenWrt's default— anyone reaching SSH is root without a key, and `install` says so.

## 4. Node protocol

One request per SSH session: a JSON object on stdin, a JSON object on stdout. `SSH_ORIGINAL_COMMAND`
is ignored.

```json
{"v": 1, "request": "run", "args": {"script": "unit_logs", "args": {"unit": "nginx.service"}, "grep": "error", "tail": 200}}
```

```json
{"ok": true, "data": {"exit": 0, "stdout": "…", "stderr": ""}, "truncated": true}
{"ok": false, "error": {"code": "denied", "message": "/etc/shadow is not readable"}}
```

| Request | Does |
|---|---|
| `hello` | limen's version, hostname, OS (`PRETTY_NAME` of `/etc/os-release`), kernel, architecture, and the catalog of scripts with what is wrong with the ones left out |
| `read_file` | An allowed file (§7.1), by line range or its last lines |
| `list_dir` | An allowed directory |
| `history` | The node's audit log (§8) |
| `run` | One script of the catalog (§6) |

- `truncated` is there only when the answer was cut.
- Stdin, not the command line: no word splitting, typed arguments, and `sudo` passes stdin through
  while it drops the environment `SSH_ORIGINAL_COMMAND` lives in.
- Arguments are validated against the request's schema, and a script's against its header, before
  anything runs; unknown fields are rejected. A string argument holds no control character (a NUL, a
  newline), whatever its `pattern` allows.
- A script that ran answers `ok` with its exit code, whatever it is: a failure of the script is data, not
  an error of the request.
- Error codes: `bad_request`, `denied`, `not_found`, `unavailable` (the node is at `limits.concurrency`, or the
  script is refused: not root's, two with its name),
  `timeout`, `unsupported_version`, `internal`. The hub adds `unreachable` and `host_key_mismatch` from
  `ssh`'s exit status 255.
- `v` is the protocol version. A node answers `unsupported_version` with the versions it speaks, so
  the hub can say "update limen on <node>" instead of failing obscurely.

## 5. MCP tools

Every tool takes a `node` argument.

| Tool | Returns |
|---|---|
| `nodes` | Configured nodes, reachability, limen version, OS, and each node's catalog and its problems |
| `read_file` | An allowed file: a range, `from` and `lines`; or its end, the last `tail` lines, or those that hold `grep` |
| `list_dir` | An allowed directory: names, types, sizes, owners, modes, modification times. Only entries that are readable or lead to something readable; at most 1000 |
| `history` | The node's audit log |
| `<script>` | One per script in the nodes' catalogs, with its declared arguments plus `grep` and `tail` |

- `nodes`, `read_file`, `list_dir` and `history` are annotated `readOnlyHint: true`. Scripts carry no
  annotation: MCP clients treat them as tools that may change things.
- `grep` is a fixed string, case-insensitive, matched against the redacted text —never the raw one, or a
  guess at a secret would be told apart by whether a line comes back— and filters before `tail` applies:
  the answer is the last N matching lines. A script never sees `grep` or `tail`, and can't declare
  arguments with those names.
- `lines` (500 unless given) and `tail` above `limits.max_lines` are cut to it, and the answer says it was truncated.
  `read_file` with `grep` or `tail` scans at most the last `limits.scan_lines` lines, and 16 MiB.
- A node's catalog is not trusted: a script whose name, description, arguments or patterns aren't plain and
  bounded, that declares a `node`, `grep` or `tail` argument, or whose name is a built-in tool's or request's, is
  left out and counted in `nodes`; `limen lint` on the node names each and says why, by the same rules
  ([scripts.md](scripts.md)). Its text reaches the model as it is, so it is kept short.
- A script's tool accepts only the nodes whose catalog has it; the hub refuses the others itself. The same
  name on several nodes must declare the same arguments; if it doesn't, the hub reports the conflict in
  `nodes` and exposes no tool for it until it's fixed.

## 6. Scripts and packs

A **pack** is a directory of scripts. A node offers the scripts of the packs listed in `scripts.packs`
(§7.1), and nothing else.

```
/opt/state/packs/systemd/   units  unit  unit_logs  restart_unit
/opt/state/packs/docker/    containers  container_logs  purge
/opt/state/nodes/nas/       sync  backup_status
```

- A script is an executable file at the top of a pack, named `^[a-z0-9][a-z0-9_-]{0,47}` plus an optional
  extension, with a header: leading comment lines starting with `#:` that form a TOML document. No header,
  no script; other files and subdirectories are the pack's own, for its scripts to use.
- A name is unique across a node's packs: two scripts with one name, in one pack or two, are both refused
  until one goes.
- The header's format, the arguments' types, the environment a script runs in, its limits and the rules
  on who owns it are in [scripts.md](scripts.md): what a script's author needs, in one place.
- The packs under `packs/` in this repository are examples to copy, not installed with limen: a pack changes
  with the machines it runs on, not with limen's releases.
- **Where the packs come from is the operator's.** A Git repository kept in sync by a script of its own, a
  configuration manager, files copied by hand: limen reads what is there. [scripts.md](scripts.md) shows the
  setup of a node from a repository.
- **Whoever can change a pack runs code as root on the node, and the agent can run it.** If a script can
  fetch the packs —a `sync`—, the agent must not be able to write where it fetches from: a protected branch,
  changed only through reviewed pull requests.

## 7. Configuration

### 7.1 Node: `/etc/limen/limen.toml`

```toml
[files]
allow = ["/etc/nginx/**", "/opt/stacks/*/compose.yaml", "/var/log/nginx/*.log"]
deny = ["**/*.env"]
max_bytes = 262144

[scripts]
packs = ["/opt/state/packs/systemd", "/opt/state/packs/docker", "/opt/state/nodes/nas"]

[redact]
names = ["DB_PASSWORD", "MQTT_PASS"]
patterns = ['sk-[A-Za-z0-9]{20,}', 'pin (?<secret>\d{4})']

[limits]
max_lines = 2000
scan_lines = 100000
max_response = 1048576
concurrency = 8

[audit]
path = "/var/log/limen/audit.jsonl"
max_bytes = 5242880
```

- The limits and paths show their defaults. `files`, `scripts` and `redact` are examples: they default to
  empty. `install` writes the file when there is none, with suggestions commented out, and never rewrites it:
  it is the operator's, and may come from their repository.
- An unknown key is an error, so a typo fails instead of being ignored; a broken file makes every
  request answer `internal` with the reason.
- `limen.toml` may be a link to a file kept elsewhere; the file it leads to must pass the same ownership
  rule as a script (§6).
- Sizes are bytes: `files.max_bytes`, `limits.max_response` and `audit.max_bytes`, which is 1024 at least. Every
  other limit counts lines (`max_lines`, `scan_lines`) or requests (`concurrency`), and none may be zero.
  `audit.path` and every entry of `scripts.packs` are absolute paths.
- An answer bigger than `limits.max_response` is replaced by a `bad_request` asking to narrow it.
- The node bounds its requests itself, whatever hub sends them: at most `limits.concurrency` at once (a lock
  on one of `/run/limen/slot-<n>`; one more gets `unavailable`), ten seconds to send the request, and two minutes
  in all —a script, its own timeout and a margin—. A request out of time has what it started killed, answers
  `timeout` and is audited as such. `read_file` goes at most 64 MiB into a file to reach its first line.

- **Nothing is readable by default.** `files.allow` starts empty. Everything readable ends up in a model
  provider's context, so the operator decides it path by path.
- `files.allow` and `files.deny` are path patterns, each starting with `/` or `**`. `**` as a whole segment is any
  number of segments, none included, so `/etc/nginx/**` matches `/etc/nginx` too; `*` is any run of characters within
  a segment, a leading dot included; `?` is one character. Nothing else is special: `{a,b}` and `[…]` are literal
  text. A pattern for a directory doesn't reach inside it; `/dir/**` does. A path is readable when an `allow` pattern
  matches it and no `deny` pattern does: `deny` wins. Both match the resolved target —links followed, `..` taken, as
  below—, never the path as asked.
- `files.max_bytes`, 256 KiB by default, caps what one range (`from` and `lines`) returns: bytes of text as read,
  newlines included. The range stops before the line that would pass it and says it was truncated. `grep` and `tail`
  don't go by it: they read the file's end, at most `limits.scan_lines` lines and 1 KiB for each line looked for, up
  to 16 MiB, and answer at most `limits.max_lines` lines (§5). Every answer is bounded by `limits.max_response`.
- A built-in deny list applies on top and can't be overridden: `/etc/shadow`, `/etc/gshadow` and their
  backups in `/var/backups/`, `/etc/sudoers*`, SSH and dropbear private keys, `/etc/ssl/private/`,
  `/etc/wireguard/`, NetworkManager connections, OpenWrt's `/etc/config/wireless`, systemd's
  `/run/credentials/`, `/etc/limen/`, `/var/log/limen/`, `/root/`, and the pseudo-filesystems `/proc/`,
  `/sys/` and `/dev/`, where a "file" can be a process's environment or a whole disk. So is the audit log,
  wherever `[audit]` puts it. Secrets the packs use —a repository's token— belong under `/etc/limen/`.
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
  between the key's markers, where redaction can't recognise it. In larger files a window is redacted as
  a whole, so a key it holds entire is masked.
- **Redaction** replaces a secret's value with `[redacted]` and keeps what names it: `DB_PASSWORD=[redacted]`.
  It applies to files, scripts' output and `history`. The built-in patterns catch `key=value` for the usual
  names of secrets (a quoted value up to its closing quote), UCI's `option key '…'`, `Authorization` headers,
  credentials in URLs, `curl -u`, `sshpass -p`, `mysql -p`, the tokens whose provider gives them a shape of
  their own wherever they appear (GitHub's `ghp_…` and `github_pat_…`, JWTs, Slack's `xox?-…`, AWS access key
  ids `AKIA…`/`ASIA…`, Stripe's `sk_live_…`/`rk_live_…`, Google API keys `AIza…`), PEM private keys and lines of
  base64 alone, as a key's body is written, unless they are all hexadecimal: those are hashes and IDs, such as a
  container's.
  - `redact.names` adds names, case-insensitive: the value after `NAME=`, `NAME: ` or `"NAME": ` is replaced.
  - `redact.patterns` adds regular expressions; a group named `secret` limits what is replaced.
  - It is a safety net; the protection is not allowing files that hold secrets, and not writing scripts
    that print them.
- **Control characters** never leave the node in a file's content or a script's output: `ESC` with the CSI or
  OSC sequence after it (colours, a window's title) and every other control character but the newline, the tab
  and the carriage return are removed, DEL and C1 included. It happens before redaction, so a sequence can't split
  a secret's name, and before `grep` and `tail`.

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
user = "root"
host_key = "ssh-ed25519 AAAA…"
```

- The directory holds `limen.toml`, the hub's SSH key, `token` (for HTTP clients, unless `LIMEN_TOKEN` is set) and
  `invites/`.
- `ssh.identity` is the hub's SSH key: a path relative to the hub's directory (`--home`, else `LIMEN_HOME`), or
  absolute. `init` makes it with `ssh-keygen` when it is missing. Its public key is the same path with `.pub`: the
  one `invite` hands out, whose fingerprint `init`, `serve` and the join line show.
- `limen.toml` is read again whenever it changes: a node that joins is there for the next request. `[http]` is read
  once, when `serve` starts. A broken `limen.toml` makes the MCP answer errors, not stop.
- `[http] public_url` (or `LIMEN_PUBLIC_URL`) is where nodes reach the hub to join: an address, not a name,
  because the static binary resolves no names.
- `http.origins` lists the web pages that may call `/mcp`, each compared whole with the request's `Origin` header:
  scheme, host and port as a browser sends them, `http://localhost:6274`, with no path or trailing slash. Empty by
  default: a request that carries an `Origin` gets `403`, and one without —an MCP client that isn't a browser—
  passes. The join endpoints don't look at it.
- `host_key` is required: limen writes its own `known_hosts`, filing each key under the node's name
  (`HostKeyAlias`), and runs `ssh` with `StrictHostKeyChecking=yes`. There is no trust on first use:
  `limen install` prints the node's key, or get it with `ssh-keyscan` and verify it out of band. A
  node that changes address keeps its key.
- `user` defaults to `limen` (`root` on OpenWrt, which the join fills in) and `port` to 22.
- `ssh.connect_timeout` is how long ssh may take to connect to a node: ssh's `ConnectTimeout`, in whole seconds
  rounded up. `ssh.request_timeout` is how long the hub waits for the answer to a request that isn't a script's run
  —`hello`, `read_file`, `list_dir`, `history`—; a script's run waits for the script's own timeout and 45 seconds
  instead. Past two minutes `request_timeout` changes nothing: the node ends such a request then (§7.1). Both are
  durations above zero: `500ms`, `5s`, `1m`.
- `ssh.per_node_concurrency`, 1 to 64, is how many requests the hub sends one node at once; the rest wait on the
  hub for their turn. The node's own `limits.concurrency` (§7.1) counts the requests of every hub together and
  answers `unavailable` past it instead of waiting: keep the hub's at or below it.
- `http.listen` is where `serve` listens, as `host:port`. `--listen` wins over `LIMEN_LISTEN`, and that over the
  file; each is checked the same way. The image's command is `serve --listen 0.0.0.0:7341`, so in the image the
  address changes by overriding the command: `LIMEN_LISTEN` and the file don't reach it.
- What the hub reads from its environment, where a blank value counts as unset:

  | Variable | |
  |---|---|
  | `LIMEN_HOME` | The hub's directory when there is no `--home`; `$HOME/.limen` without either. The image sets `/data` |
  | `LIMEN_TOKEN` | The HTTP clients' token, instead of the `token` file (§9) |
  | `LIMEN_PUBLIC_URL` | Over `http.public_url`, checked the same way |
  | `LIMEN_LISTEN` | Over `http.listen`, under `--listen` |
  | `XDG_RUNTIME_DIR` | Where ssh's control sockets go (§12) |

## 8. Audit

- Every request `gate` receives is appended to `/var/log/limen/audit.jsonl` (root, `0600`): time, request,
  script and arguments, client address (`SSH_CONNECTION`), result code, the script's exit code and duration.
  That includes malformed ones, those of an unknown version, and those that come while the configuration is
  broken (to the default path then). Arguments over 4 KiB are replaced by their size. Past `audit.max_bytes`
  it becomes `audit.jsonl.1`: no logrotate, which OpenWrt doesn't have.
- A script's run is written when it starts too, with `started` as its result: it may change the machine, and a run
  killed halfway still leaves a trace.
- `history` shows it redacted, like everything that leaves the node. Arguments are recorded: a script
  doesn't take secrets as arguments.
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
  - Validates `Origin` against `http.origins` (§7.2), against DNS rebinding.
  - Listens on `127.0.0.1:7341` unless told otherwise (§7.2). The image listens on every interface;
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
| `limen call <node> <request \| script> [--arg k=v]… [--grep] [--tail]` | hub | One request over SSH; prints the JSON. A script's arguments are typed by its header, from the node's catalog |
| `limen join <line> \| --hub-key <key> --name <name> [--from …] [--address …] [--ssh-port …]` | node, root | Joins the hub (§10.1): install with the hub's key, then report |
| `limen install [--hub-key <key>] [--from <cidr>] [--dry-run]` | node, root | Debian: binary in `/usr/local/bin`, the `limen` user, `authorized_keys`, `sudoers` (validated with `visudo -c` first) and, where systemd runs, the user's `user@<uid>.service` masked. OpenWrt: binary in `/usr/bin`, root's dropbear keys, sysupgrade keep list. Both: `/etc/limen/` and a `limen.toml` if there is none. Without `--hub-key` nothing opens to SSH yet. Idempotent |
| `limen uninstall [--purge]` | node, root | Undoes `install`; keeps `/etc/limen/` and the logs unless `--purge` |
| `limen gate` | node | The forced command. Not for people |
| `limen run <script> [--arg k=v]… [--grep] [--tail]` | node, root | One script, as the hub would run it; prints its output and exits with its exit code |
| `limen lint` | node | Packs, script names, headers and permissions, without running anything |
| `limen version` | both | The version |

The `limen` user needs no user manager. Without the mask, `pam_systemd` starts one for every login of the hub, and
with it whatever the machine starts for each user: on a machine with a desktop, sound servers, which fail and fill the
journal. logind takes a masked `user@<uid>.service` as none. `uninstall` unmasks it before removing the user, so the
uid is left as it was for whoever gets it next.

`install` refuses a hub key that already opens root's `authorized_keys`, or dropbear's, without limen's forced
command, and reads `sshd -T` to warn when `AllowUsers` or `AllowGroups` would keep the `limen` user out.

`install.sh`, at the root of the repository, is the way in (`curl … | sudo sh`, or `wget` on OpenWrt). POSIX
`sh`, because OpenWrt has only busybox's `ash`. It downloads the binary of the latest release for `uname -m` and
checks it against `SHA256SUMS`, then:

- no argument: `limen install`, for a machine whose packs and configuration come before the hub.
- `--join <line>`: `limen join` with that line; `--address <address>` passes on where the hub reaches the machine.
- `--hub-key <key> --name <name>`: the same, for a hub with no HTTP; it ends with the `limen trust` line.
- `--hub`: the binary in `~/.local/bin` (or `/usr/local/bin` as root) and `limen init`, for a laptop hub.

Every answer can come from a `LIMEN_*` variable instead, for unattended installs; the list is at the top of
`install.sh`.

### 10.1 Joining a node

```
hub:   limen invite nas  →  …/install.sh | sudo sh -s -- --join 'http://<hub>:7341/join/<code>#SHA256:<hub key>.<secret>'
node:  GET  /join/<code>  →  {"name": "nas", "hub_key": "ssh-ed25519 …", "proof": "<HMAC>"}   key against the fingerprint, both with the secret
       limen install with that key
       POST /join/<code>  ←  {"host_key": "ssh-ed25519 …", "user": "limen", "port": 22, "proof": "<HMAC>"}
hub:   [nodes.nas] with the request's source address, then `hello`  →  {"reachable": true, "detail": "…"}
```

- The code is 26 characters of base32 (130 bits), kept in `invites/<code>.json` with the node's name and a secret
  of the same size; it lasts one hour by default and one successful `POST`.
- The secret travels only in the line's fragment, never on the wire: the node signs its arrival with it
  (HMAC-SHA256 over host key, user, port and address). Whoever sees the code in transit can't arrive in the node's
  place or change what it sends; a forged arrival is refused without spending the invitation. Joins are handled
  one at a time.
- The invitation is signed with the same secret (HMAC-SHA256 over name and key), so nobody between the two can
  change what the node is told either.
- The fingerprint in the line is `SHA256:<base64>` of the key, as `ssh-keygen -lf` prints it. The key itself is not
  secret; what matters is that it arrives unchanged, and a mismatch stops the node before anything is installed.
- The hub trusts the host key that arrives with a valid code: trust on first use, bound to an invitation a person
  gave out.
- The node's address is where the `POST` comes from, unless it sends `--address`. The hub edits only that node's
  table in `limen.toml`, edited as TOML and not as text, and parses the result before writing it.
- The node writes nothing the hub sends into its configuration: the join adds the hub's key to
  `authorized_keys`, and `limen.toml` stays the operator's.
- `limen join` talks HTTP to an address, never a name, so a join line means the same wherever it is pasted.
- Without an HTTP hub, `invite` prints the hub's key in the line (`--hub-key`) and the node prints
  `limen trust <name> <address> '<host key>'` for the hub.

## 11. Distribution and platforms

- **One static binary per architecture**, `amd64` and `arm64`, for any Linux: linked against musl, it
  needs nothing of the machine but the kernel, and runs the same on Debian, Alpine or OpenWrt. About 3 MB.
- Hub image `ghcr.io/xoadev/limen`, one tag for `amd64` and `arm64`: distroless —no shell, no package manager—
  with the binary and OpenSSH's `ssh` and `ssh-keygen`, taken from Alpine with the libraries they load. It runs as
  distroless's `nonroot` (uid 65532). Volume `/data` holds `limen.toml` and the SSH key.
- **Releases** follow the conventional commits: every push to `main` rewrites a draft release with the next
  `X.Y.Z` and its changes. Running the release workflow by hand publishes it: once `make check` is green on the
  draft's commit, it builds the binaries (release variant, checked static) and the image, starts the image on both
  architectures, attaches the binaries and `SHA256SUMS` to the draft and only then publishes it, which creates the
  `vX.Y.Z` tag. A release is never public without its files, and releases are immutable: once published, neither
  its files nor its tag change. The version exists only from the tag: `limen --version` says
  `X.Y.Z · build <run> · <date>`, or `dev` for a local build.
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
- Nodes: any Linux with OpenSSH and sudo, or OpenWrt with dropbear. What else a node needs —systemd, Docker,
  git— is its packs' business.
- Not supported: macOS, Windows; 32-bit ARM and MIPS (many routers), which have no release binary.

## 12. Implementation

- Rust, for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, linked statically by the
  toolchain's own `rust-lld`: no C compiler, and no cross compiler for arm64.
- Crates:
  - `limen-core`: protocol types, request schemas, configuration, script headers, path policy,
    redaction and the join's formats. Pure, tested without a machine.
  - `limen`: the binary — processes, files, SSH, MCP and HTTP.
- MCP: own JSON-RPC 2.0 implementation, no SDK. HTTP: an own bounded HTTP/1.1 server for the hub (§9);
  `join`'s two requests over `std::net`. Neither has TLS. JSON: `serde_json`. CLI: `clap`. System calls: `rustix`; no `unsafe` code.
- Users and groups come from `/etc/passwd` and `/etc/group`, read by limen itself; nodes resolve no host
  names.
- Processes: `std::process::Command` with an argument array, never a shell; own process group, a clean
  environment, stdout and stderr on separate pipes read with a cap; timeouts send `SIGTERM` to the group,
  then `SIGKILL`.
- SSH: the system `ssh` binary with `BatchMode=yes`, `IdentitiesOnly=yes` and connection
  multiplexing (`ControlMaster`, sockets in `$XDG_RUNTIME_DIR/limen-<uid>/<hub>`, or under `/tmp` when that path
  would not fit a Unix socket; directories of mode `0700` that must be the user's), `-F none`, and no agent or port
  forwarding.
- TOML: `toml_edit`. Files are read into `#[derive(Deserialize)]` types that refuse unknown keys, so a typo
  fails with its line; the hub's `limen.toml` is edited as a document, comments kept.

## 13. Threat model

| Threat | Outcome |
|---|---|
| Hub, token or MCP client compromised | Reads what the nodes allow and runs the scripts they offer, with arguments their headers accept. No shell, no code of its own |
| Prompt injection through logs or files | The same: text in a log can lead the model to run any script on offer, the ones that change things included. **Offer only changes you would let whoever writes to your logs trigger** |
| Whoever can change a pack, or where a script fetches packs from | Runs code as root on the node. The agent must not be able to write there (§6) |
| A compromised node | Answers what it likes about itself. Its catalog can't name tools or arguments beyond plain, bounded text, nor reach other nodes |
| Someone who reaches the HTTP port without the token | Gets `401` before any body is read. Connections, heads and bodies are bounded; a flood denies service, it doesn't stop the hub |
| Argument injection | Arguments are typed, validated on the node, reach the script as environment variables and never a shell |
| Symlink from an allowed path to a secret | Resolved before matching (§7.1) |
| Expensive requests | Timeouts, output caps, and on the node itself a deadline per request and `limits.concurrency` |
| An invitation leaks | Whoever uses it first adds *one* machine, with that name, to the hub —a machine the agent will then use—; one hour, one use |
| Someone between a joining node and the hub | Can't swap the hub's key —the fingerprint in the line stops the node— nor put their machine in the node's place: the arrival is signed with a secret that never travels. The source address is not signed: replaying the arrival from elsewhere files a wrong address, which `StrictHostKeyChecking` then refuses. Denial of service; join again |
| The token leaks | It is in `$LIMEN_HOME/token`, printed by `connect`, and in the container's environment if given as `LIMEN_TOKEN` (`docker inspect`). The same as a compromised hub; rotate it by replacing the file or the variable |
| A malicious hub at join time | Installs its key, as the node's owner asked, and names the node on its side. Writes nothing into the node's configuration; what it prints is stripped of control characters |
| A key reused for more than limen | `install` refuses a hub key that already opens root's or dropbear's `authorized_keys` without limen's forced command |
| A hub that floods a node with requests | Gets `unavailable` past `limits.concurrency`. Can rotate old entries out of the node's audit log, which is bounded. Ship it elsewhere if it must outlive that |
| A local user racing the gate | Links swapped in, directories swapped for links, FIFOs and hard links put in allowed directories: the walk, the single open and the checks on the open file refuse them |
| A malicious or buggy script | **Not covered.** Scripts belong to root; limen trusts them, and redacts what they print |
| Secrets in allowed files or in a script's output | Partly: redaction is best-effort |
| Data leaving the machine | **By design**: whatever is readable or printed reaches the model provider |
| A compromised release, image or `install.sh` | Partly: a binary or image replaced by hand, or by a leaked token, fails `gh attestation verify`. `install.sh` checks only `SHA256SUMS`, from the same release, and is served from `main`: whoever can write to the repository, or run its release workflow, can still do this |

## 14. Decisions

| Decision | Alternative | Why |
|---|---|---|
| Limits on the node | Policy in the MCP server | Existing SSH MCP servers filter commands on the client side: a compromised or deceived client then has a shell |
| SSH with a forced command | An agent daemon per node | `sshd` already authenticates and encrypts; a daemon adds a port, its own auth and its own updates |
| Request on stdin | Arguments in `SSH_ORIGINAL_COMMAND` | No word splitting, and `sudo` keeps stdin but drops that variable |
| The system `ssh` | An SSH library | The system client brings agent support, multiplexing and the operator's configuration, and is already on every hub |
| The agent changes things only through the node's scripts | A read-only agent, and changes by CI with another key | An agent that can't act on what it finds is half useful. The scripts bound what it can change as tightly as the allowlist bounds what it can read; the price is that an injected instruction can run any of them (§13) |
| One user and one key, the hub's | A read role for the hub and a deploy role for CI | With every change a script the node offers, a second role would guard nothing the scripts don't |
| Everything but files lives in scripts | Built-in readers for systemd, procd and Docker | limen stays one mechanism with no init system, runtime or tool to follow; supporting one more is a pack, not a release |
| Files stay built in | A script that reads files | The walk, the single open and the checks on the open file are what stop a local user's links; a script would redo them, or not |
| limen fetches nothing | A built-in `sync` from Git | How a machine gets its packs is the operator's: Git, rsync, a configuration manager. The first fetch comes before limen can run any script, so the operator's setup does it anyway |
| Example packs, copied | Packs installed with limen | A pack changes with the machines it runs on, not with limen's releases |
| Filters after redaction, in limen | Scripts taking `grep` | A filter on the raw text tells a secret apart letter by letter by whether a line comes back |
| Empty allowlist by default | A broad default such as `/etc/**` | `/etc` holds Wi-Fi passwords, VPN keys and TLS keys |
| Rust | Kotlin/Native (the first implementation), Go | Static musl binaries of about 3 MB built by the toolchain itself, arm64 without a cross compiler, and memory safety without a garbage collector in what runs as root. Kotlin/Native had no musl target: a static glibc needed its own linker script, no NSS, and an own HTTP client where glibc's iconv was missing |
| A static binary | A package per distribution | One file runs on any Linux, and OpenWrt has no package for it |
| Configuration as `serde` types, edited with `toml_edit` | Reading and editing TOML by hand | `deny_unknown_fields` turns a typo into an error with its line; an edited document can't gain a table from a value, and keeps the operator's comments |
| Joining with a one-time invitation | Copying keys by hand, or the hub logging into nodes with an administrator's SSH | Nothing to carry but one line; the hub never holds more than its own key |

## 15. Open questions

- Redacting the values of known secrets wherever they appear: `redact.values_from = ["/opt/stacks/*/.env"]`, the
  literal values of those files masked in every output, whatever names them.
- Packs per key, if one day more than the hub calls a node.
- Authentication of the HTTP hub beyond one shared token: named tokens per client, with the nodes each
  may see; OAuth 2.1 only if the hub is ever reachable from outside the VPN.
- The `arm64` binary runs under qemu-user, children included, but has not run on hardware yet.
- 32-bit routers: an ARMv7 build (`armv7-unknown-linux-musleabihf`, 2.4 MB) links and runs under qemu; MIPS
  needs Rust's nightly. Neither is released.
- Following a script's output (`follow`): v1 only answers once it has finished.
- A `.deb` package besides `limen install`.
