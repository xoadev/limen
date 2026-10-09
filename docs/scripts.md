# Scripts and packs

Everything an agent can do on a machine, besides reading the files it allows, is a script the machine offers.
This is what a script's author needs: the header, how limen runs it, the rules it must meet, and a setup that
takes a machine's packs from a Git repository. The design around it is in the [spec](spec.md) (§6).

## A pack

A directory listed in `scripts.packs` of `/etc/limen/limen.toml`. Its scripts are the executable files at its
top whose name is `^[a-z0-9][a-z0-9_-]{0,47}`, plus an optional extension, and that carry a header. Everything
else —a README, helpers without a header, subdirectories— is the pack's own, for its scripts to use: the
agent never sees it.

```
packs/docker/
  containers          #: header → a tool
  container_logs      #: header → a tool
  purge               #: header → a tool
  lib/format.sh       helper, not a tool
```

A name is unique across a node's packs; two scripts with one name are both refused until one goes.
`limen lint` lists every problem without running anything.

## The header

The leading comment lines starting with `#:` form a TOML document. It is parsed, never run.

```sh
#!/bin/sh
#: description = "Last lines of a unit's journal"
#: read_only = true
#: timeout = "30s"
#: [args.unit]
#: type = "string"
#: pattern = '^[A-Za-z0-9@._:-]{1,128}$'
#: description = "The unit, e.g. nginx.service"
#: [args.since]
#: type = "string"
#: default = "1h"
#: pattern = '^[0-9]{1,4}[smhd]$'
exec journalctl --no-pager -o short-iso --since "-$LIMEN_ARG_SINCE" -u "$LIMEN_ARG_UNIT"
```

| Key | | |
|---|---|---|
| `description` | required | What the model reads to choose the tool. One line, what it does and what it answers |
| `read_only` | default `false` | `true` when the script changes nothing on the machine: the hub tells MCP clients they may run it without asking. Refreshing a cache it can rebuild, such as the package lists, counts as changing nothing. When in doubt, leave it out |
| `timeout` | default `60s`, at most `1h` | `30s`, `5m`, `1h`, in whole seconds: a fraction is dropped, and under `1s` is `1s`. `SIGTERM` to the script's process group when it runs out, `SIGKILL` after a grace period |
| `[args.<name>]` | one per argument | `name` is `^[a-z][a-z0-9_]{0,31}$`; `node`, `grep` and `tail` are taken |

Each argument:

| Key | |
|---|---|
| `type` | `int`, `bool`, `enum` or `string` |
| `description` | What the model reads |
| `default` | Makes it optional: a call that leaves it out gets this value |
| `required` | `true` without a `default`: a call must give it. `required = false` makes it optional with no default, and the variable is unset when a call leaves it out. `required = true` with a `default` is refused: the default makes it optional |
| `range = [min, max]` | For `int` |
| `values = ["a", "b"]` | For `enum` |
| `pattern` | For `string`. By default `^[A-Za-z0-9_][A-Za-z0-9._-]{0,63}$`: never an option, `.` or `..` |

A `pattern` is a Rust `regex` that must match the whole value: `\w`, `\d`, `\s` and `(?i)` are there, Unicode's
`\p{…}` classes and look-around are not. Whatever the pattern, a string argument never holds a control character: a
NUL, a newline or an `ESC` is refused before the pattern is applied.

The catalog reaches every MCP session as text, and the hub doesn't take it on trust. It leaves out a script:

- named as one of its own tools or the node's requests: `nodes`, `hello`, `read_file`, `list_dir`, `history`, `run`;
- whose description, or an argument's, runs past 300 characters or holds a control character;
- with a `pattern` over 512 bytes, or that compiles to more than 1 MiB. A bounded repetition unrolls: `\w{1,300}`
  —Unicode's `\w`, hundreds of ranges, 300 times— is too big; `[A-Za-z0-9_]{1,300}` is not.

The hub only counts these in `nodes`; `limen lint` on the node says which and why.

## How it runs

- As root, with no shell in between: limen executes the file, and its `#!` line picks the interpreter.
- A clean environment: `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`, `LANG` and `LC_ALL`
  `C.UTF-8`, `TZ=UTC`, `HOME=/root`, no pagers or colours, and:

  | Variable | |
  |---|---|
  | `LIMEN_ARG_<NAME>` | Each argument, its name in capitals; `true`/`false` for a `bool` |
  | `LIMEN_SCRIPT` | The script's name |
  | `LIMEN_PACK` | The pack's directory, to reach its helpers |
  | `LIMEN_NODE` | The machine's hostname |

- cwd `/`, stdin `/dev/null`, umask `022`.
- What the agent gets back: the exit code, stdout and stderr, each stripped of terminal control sequences (colours
  and other control characters but newline, tab and carriage return), then redacted, and each cut to its last
  `limits.max_lines` lines. `grep` and `tail`, when the agent asks for them, apply to stdout after redaction; a script
  never sees them. Both streams together are captured up to 16 MiB: past it the script is stopped. Whatever a limit
  cuts, the answer says so, and the answer as a whole is bounded by `limits.max_response`.
- The exit code is data, not an error: `0` is success, anything else is what the script says it is.
- Every run is recorded in the audit log with its arguments, when it starts and when it ends.

## Who owns it

A script that isn't owned by root, or is writable by group or others, is refused, and so is one under such a
directory, all the way up to `/` — the same rule as `sshd`'s `StrictModes`. It holds for the pack's directory
and for `limen.toml` and where it leads, if it is a link. A checkout or a copy made as root with umask `022`
passes; one made with a looser umask, or by another user, doesn't.

## Writing one

- **Print what the agent needs, short.** A first line that sums it up, then the detail. Machine output such as
  JSON is fine; a whole `docker inspect` is not.
- **Never print a secret.** Redaction is a safety net, not a filter to rely on: `docker inspect` shows
  environment values, `ps` shows command lines. Choose fields (`--format`) and leave secrets out.
- **Take arguments as data.** Quote them (`"$LIMEN_ARG_UNIT"`), put `--` before them where the command accepts
  it, and narrow `pattern` to what the argument can be.
- **A script that changes something is safe to run twice**, and says what it did. The agent may run it again
  after a timeout.
- **One at a time, where it matters**: `flock -n /run/lock/limen-<name>.lock` refuses a second run while the
  first goes on.
- **Offer only changes you would let whoever writes to your logs trigger.** Text in a log or a file can lead the
  model to call any script on offer.
- **A script that can update itself** —a `sync` that pulls the repository it lives in— puts its body in a
  function called on the last line, `main "$@"`, so the shell has read it whole before the file changes.

## Released packs

Each pack in limen's `packs/` is released on its own as `pack-<pack>-vX.Y.Z`: a tarball of its folder and
`SHA256SUMS`, with a build provenance attestation. limen fetches nothing, so the machine's own setup —its `sync`, a
configuration manager— brings it, and the machine's `limen.toml` lists where it went:

```sh
pack=system version=X.Y.Z sha256=<from the release's SHA256SUMS, kept in your repository>
name=limen-pack-$pack-$version.tar.gz
dir=/opt/limen-packs/$pack@$version
if [ ! -d "$dir" ]; then
  curl -fsSL --proto '=https' -o "/tmp/$name" "https://github.com/xoadev/limen/releases/download/pack-$pack-v$version/$name"
  echo "$sha256  /tmp/$name" | sha256sum -c -
  umask 022 && mkdir -p "$dir.tmp" && tar -xzf "/tmp/$name" -C "$dir.tmp" --strip-components=1 --no-same-permissions
  mv "$dir.tmp" "$dir"
fi
```

```toml
[scripts]
packs = ["/opt/limen-packs/system@X.Y.Z", "/opt/state/nodes/nas"]
```

- **Pin the hash, not only the tag**: a tag names a version, the hash is what makes it the same bytes.
- **As root, with `umask 022`**: limen runs a script only if root owns it and nobody else can write it or any
  directory above it. The tarball is made that way; extracting as root keeps it.
- **A folder per version**, named `<pack>@X.Y.Z`: moving a machine to a new one is changing `limen.toml`, and going
  back is changing it back. `limen lint` after each. The `limen` pack's `limen_packs` reads the version from that
  name and says when a newer release is published.
- The agent must not be able to change the pinned version or hash: that is choosing what runs as root.
- **limen before the packs**: a pack can use a header key an older limen doesn't know, and that limen refuses
  the script. Each pack's README says which limen it needs, and its release notes say when that changes.

## A machine from a Git repository

One way to keep a machine's packs and configuration in a private repository. limen takes no part in it beyond
running the `sync` script.

```
infra/
  packs/systemd/  packs/docker/          shared packs
  nodes/nas/limen.toml                   the machine's configuration: files.allow, scripts.packs, redact
  nodes/nas/sync                         a script: the repository to the branch, then apply
  nodes/nas/apply                        no header: a helper, reached only through sync
  nodes/nas/stacks/immich/compose.yaml
```

`nodes/nas/limen.toml`:

```toml
[files]
allow = ["/opt/state/nodes/nas/**", "/var/log/nginx/*.log"]
deny = ["**/*.env"]

[scripts]
packs = ["/opt/state/packs/systemd", "/opt/state/packs/docker", "/opt/state/nodes/nas"]
```

**Once, as root** — the operator's own bootstrap:

```sh
umask 022
curl -fsSL https://raw.githubusercontent.com/xoadev/limen/main/install.sh | LIMEN_YES=1 sh   # binary, user, sudoers
install -m 0600 /dev/null /etc/limen/repo-token && cat > /etc/limen/repo-token         # a read-only token
header="Authorization: Basic $(printf 'x-access-token:%s' "$(cat /etc/limen/repo-token)" | base64 | tr -d '\n')"
GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.https://github.com/.extraHeader GIT_CONFIG_VALUE_0="$header" \
  git clone --depth 1 https://github.com/you/infra /opt/state
ln -sf /opt/state/nodes/nas/limen.toml /etc/limen/limen.toml
limen lint
```

- The token goes under `/etc/limen/`, which `read_file` never reads, and reaches git through its environment:
  never in a URL, an argument or `.git/config`.
- Then the join line from `limen invite nas` on the hub, pasted on the machine.

**From then on, through the agent** — `nodes/nas/sync`:

```sh
#!/bin/sh
#: description = "Brings the machine to the repository's main branch and applies it"
#: timeout = "30m"
main() {
  set -eu
  exec 9> /run/lock/limen-sync.lock
  flock -n 9 || { echo "a sync is already running"; exit 1; }
  header="Authorization: Basic $(printf 'x-access-token:%s' "$(cat /etc/limen/repo-token)" | base64 | tr -d '\n')"
  export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.https://github.com/.extraHeader GIT_CONFIG_VALUE_0="$header"
  cd /opt/state
  git fetch --depth 1 origin main
  git reset --hard origin/main
  git clean -fd            # untracked files go; ignored ones, such as a stack's .env, stay
  echo "at $(git rev-parse --short HEAD)"
  exec ./nodes/"$LIMEN_NODE"/apply
}
main "$@"
```

- Whoever can push to `main` runs code as root on every machine that syncs it, and the agent can run `sync`:
  the agent must not be able to push there. Protect the branch, and change it only through reviewed pull
  requests.
- The first clone can't be a script: until it runs there are no packs. That is why the bootstrap does it.

[Our machines, through pull requests](https://xoa.dev/notes/machines-through-pull-requests/) tells how we use this setup, with the
agent's changes going through reviewed pull requests.
