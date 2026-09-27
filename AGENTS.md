# AGENTS.md — working contract

This repository is **limen**: an MCP server for inspecting Linux machines, where every limit is enforced on the
machine itself. This file is the working contract for any agent (Claude Code or another) and for people.
`CLAUDE.md` points here.

## Sources of truth

| What | Where | Rule |
|---|---|---|
| Product | `docs/spec.md` | The only source of truth. If code and spec disagree, the spec wins; if the spec is wrong, change the spec first |
| Protocol, requests and their arguments | `kotlin/core/src/limen/core/Requests.kt`, `Protocol.kt` | The same values validate on the node and become MCP tool schemas on the hub. The spec's tables describe them |
| What a user needs | `README.md` | Install, configure, use. Written from the spec, never the other way round |

What an agent learns working here is written here, in the spec or in a skill — never in an agent's private
memory, where the next session, another agent or a person can't read it.

## Layout

| Folder | What |
|---|---|
| `docs/` | `spec.md`; `openwrt.md`, why the binary is static and what that costs |
| `kotlin/` | The Kotlin Toolchain project: `project.yaml`, a `module.yaml` per module, the `kotlin` wrapper that pins the toolchain |
| `kotlin/core/` | Pure rules: protocol, request schemas, argument validation, configuration, TOML, script headers, path policy, redaction, parsers of what system programs print. No processes, files or network |
| `kotlin/cli/` | The `limen` binary: `os/` (processes, files), `node/` (gate, requests, platforms, repository, scripts, install), `hub/` (SSH client, MCP server, transports) |
| `install.sh` | The installer a user pipes into `sh` on a new machine. POSIX `sh` (OpenWrt has no bash); `make e2e` runs it under dash and ash |
| `tools/` | Scripts the `Makefile` calls, and `ld-static`, the linker that makes the binary static |
| `etc/` | `Dockerfile` of the hub image; `e2e/` the Debian node image of `make e2e` (OpenWrt's is the official one) |
| `.github/workflows/` | CI, always through `Makefile` targets |

## Modules

| Module | Type | May depend on | Never on |
|---|---|---|---|
| `core` | KMP library, `linuxX64` and `linuxArm64` | `kotlinx-serialization-json` | processes, files, network, `platform.*` |
| `cli` | Linux binary, same targets | `core`, `kotlinx-coroutines`, `clikt-core`, Ktor server (CIO) | — |

Rules that hold:

- **Kotlin/Native only.** No JVM target, no GraalVM: a binary that starts without a runtime is the point.
- **Anything testable without a machine goes in `core`.** A new request's argument rules, a new output format to
  parse: `core`, with a test that feeds it a captured sample. `cli` only runs things and glues.
- **A new dependency must publish Kotlin/Native artifacts for both Linux targets.** foco (`../foco/docs/native-deps.md`)
  keeps the register of what has been validated; the versions here are the ones validated there.

## Technical choices

Each one had an alternative. Changing one is changing this table and the spec's §14 in the same PR.

| What | With | Why this one |
|---|---|---|
| Limits | On the node, in `gate` | A client-side policy is bypassed by a compromised or deceived client |
| Transport to nodes | The system `ssh` with a forced command | No daemon on the nodes; no SSH library exists for Kotlin/Native |
| Request | One JSON line on stdin | No word splitting; `sudo` keeps stdin and drops `SSH_ORIGINAL_COMMAND` |
| Child processes | `posix_spawn` (`platform.linux`), argument arrays | After `fork`, the Kotlin/Native runtime is not async-signal-safe; never a shell |
| MCP | Own JSON-RPC 2.0, no SDK | Same as foco: a few hundred lines, fully under control |
| TOML | Own parser of the subset used | Operator-named tables (`[nodes.<name>]`) and errors that name the key |
| CLI | `clikt-core` | The `clikt` artifact with markdown duplicates symbols when linking |
| libc | glibc, linked statically (`tools/ld-static`) | Kotlin/Native has no musl target; static, one file runs on glibc and musl alike. A glibc bundle next to the binary also worked, with a directory of libraries to carry |
| System facts | `/proc`, `statvfs`, `/etc/passwd` | `ps`, `ss` and `df` differ between distributions and busybox; NSS is out of reach of a static glibc |
| HTTP | **Ktor first**: the server is Ktor (CIO) with its ordinary text APIs | Validated on Native by foco. Own code only where Ktor is shown not to work in the static binary, with a test that notices when it would: today only `HttpLite` for `limen join`, watched by `KtorCharsetTest` (docs/openwrt.md) |

## Conventions

- **Everything in English**: code, comments, documentation, commit messages, tool descriptions.
- **Comments only when they add something**: the *why* the code can't say — a rule of the spec, a platform
  constraint, a decision with an alternative. Never a narration of the line below.
- **Security rules are code, not habits.** Every read goes through `PathPolicy` with the resolved path; every
  argument through `Args.validate`; every program through `Proc` with an argument array; every text leaving the
  node through `Redactor`.
- **Errors say what to do**: `no SSH key at /data/id_ed25519 ([ssh].identity in /data/limen.toml)`, not
  `file not found`.

## Mandatory loop

```
spec → change → make check → commit
```

1. Find what you are implementing in `docs/spec.md`. If it is not there, or is wrong, change the spec **first**.
2. Write or adjust the tests first: `core` with samples, `cli` with fakes, `tools/e2e.sh` for what only a real
   sshd proves.
3. Implement.
4. `make check`. Green means correct; there is no other criterion. Anything touching `gate`, `install`, SSH or
   sudo also needs `make e2e`.
5. Small commits. One goal per branch; the PR includes the output of `make check`.

**Commits follow [Conventional Commits](https://www.conventionalcommits.org)**: `feat(gate): answer ports`. Types:
`feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `build`, `ci`; scopes: `core`, `gate`, `hub`, `install`,
`spec`, `harness`. **PRs are merged with squash**, so the PR title is the commit that stays.

**Nothing is committed straight to `main`.** Everything through a PR, one-line changes included. `make hooks`
installs a `pre-push` that refuses pushes to `main`.

If `make check` can't go green for a reason outside your goal, say so in the PR; never disable or relax a check.

## Makefile targets only

Never launch the build by hand (`kotlin build`, `kotlinc`). If no target covers what you need, add the target or
the script in `tools/`.

| Target | What it does |
|---|---|
| `make check` | `lint build test`. What CI runs |
| `make lint` | ktlint over `kotlin/`, shellcheck over `tools/`, actionlint over the workflows. `FIX=1` fixes what can be fixed |
| `make build` / `make test` | Every module, for this machine's Linux |
| `make cli` | Only the binary. `VARIANT=release` for the optimised one; `ARCH="x86_64 aarch64"` for both architectures |
| `make e2e` | Containers with a real SSH server —Debian with OpenSSH and a repository, OpenWrt's image with dropbear—, `limen install` inside, the hub against them with both keys. `SUITE=debian` or `openwrt` for one. Needs Docker. Not in `make check` |
| `make docker` | The hub image, `limen:local` (or `IMAGE=…`, `TAGS=…`, `LABELS=…`), from the binaries of `make cli` (or `BINARY_AMD64=…`, `BINARY_ARM64=…`). `PLATFORMS=linux/amd64,linux/arm64 PUSH=1` pushes both under one tag; without `PUSH`, one platform, because Docker loads one per tag. The release goes through this same target |
| `make stamp` | `BuildStamp.kt`: version, build date and number. Not committed; everything that compiles depends on it |
| `make local-install` | The binary in `~/.local/bin` |
| `make hooks` | The `pre-push` hook |

`make -k check` runs every check even when one fails.

## CI

Every workflow calls `Makefile` targets: what is checked is defined once, and CI can't drift from a laptop. Every
third-party action is pinned by commit SHA with its version in a comment (`actions/checkout@3d3c42e… # v7.0.1`):
a tag can move, a SHA can't. Secrets reach a step through `env:`, never interpolated into `run:`.

| Workflow | When | What |
|---|---|---|
| `check.yml` | Every push to `main` and every PR | `make -k check`: the one mandatory gate. On `main`, a red run opens (or comments) the `main-red` issue |
| `release.yml` | Every push to `main`; publishing a draft release; or by hand | On a push, [convco-version](https://github.com/xoadev/convco-version) reads the conventional commits that touched what goes into the binary or the image and rewrites **one draft** release, `vX.Y.Z`, with what went in. **Publishing it** —a person, from the releases page— creates the tag, and that builds and publishes: waits for `check.yml` green on that commit, stamps the version (`limen --version`), builds both static binaries in release, checks they are static, builds the image per architecture and starts it, pushes it to `ghcr.io/<repo>` as `X.Y.Z`, `latest` and `build<run>`, and attaches the binaries and `SHA256SUMS`. By hand it builds everything as `dev` and publishes nothing |
| `cli.yml` | Label `cli` on a PR, or by hand | Both binaries (debug) as an artifact, with the link commented on the PR: for trying a change on a real node |
| `e2e.yml` | Label `e2e` on a PR, or by hand | `make e2e`, both suites |

The version is never written in the code: the commits decide it, and it only exists once a release is published.
`tools/stamp.sh` writes it into `BuildStamp.kt` from `LIMEN_VERSION`, `LIMEN_BUILD_DATE` and `LIMEN_BUILD_NUMBER`;
a local build says `dev` and the day.

## What has already failed

Mistakes made here, with what avoids them. `make check` does not see them.

- **`/**` inside a KDoc opens a nested comment.** A glob like `/etc/nginx/**` in a doc comment leaves the file
  with an unclosed comment and every declaration after it unresolved. Write the glob without the leading slash
  or describe it in words.
- **`posix_spawn` is in `platform.linux`, not `platform.posix`.** The compiler only says "unresolved reference".
- **`O_NOFOLLOW` refuses symlinks, `/etc/os-release` included.** `Fs.read` opens what the caller already resolved
  and checked; resolve with `Fs.realPath` first.
- **SIGPIPE kills the process.** Writing to a child that exited without reading its stdin ends limen silently
  (exit 141). `Proc` ignores SIGPIPE before any run that writes to a child; keep it that way.
- **A multiplexed SSH connection skips the host key check.** With `ControlMaster`, a second connection to the
  same node reuses the first one's socket, verified or not by the current `known_hosts`. `make e2e` changes
  `XDG_RUNTIME_DIR` before testing a wrong host key; a test of anything SSH-level must do the same.
- **"0 tests failed" is not "compiled".** The toolchain prints per module; filter its output by
  `ERROR|error:|FAIL|failed`, and confirm a new test by its name in `Passed <name>`.
- **Script trust depends on who runs it.** A script is trusted when it and every parent directory are owned by
  root (or the user limen runs as) and not writable by group or others. `/tmp` and a group-writable checkout fail
  that on purpose; tests that run scripts live in `make e2e`, where they are root's.

- **A C function that keeps a pointer gets a string that outlives the call.** Kotlin/Native frees the C copy
  of a `String` argument as soon as the call returns. glibc before 2.29 —the one the static binary carries—
  keeps `posix_spawn_file_actions_addopen`'s path until the spawn, so the child opened garbage and exited 127,
  depending on what had reused that memory. `Proc` opens `/dev/null` itself and dups it.
- **`/proc/mounts` is a symlink**, and `Fs.read` opens with `O_NOFOLLOW`: read `/proc/self/mounts`.
- **Two roles as the same user share a multiplexed SSH connection.** On OpenWrt both keys log in as root; a
  deploy request through `ControlMaster` rode the socket the read key had opened and landed in the read role.
  Deploy requests never multiplex.
- **A forced command only holds a login by key.** OpenWrt's image has root without a password and dropbear
  accepts it: the e2e "passed" a session that never used the key. Its dropbear runs with password logins off
  (`-s`), and `install` warns about an empty root password.
- **glibc's iconv has only UTF-8 built in in the static binary**, and Ktor's client encodes text through UTF-16:
  it failed with `Failed to open iconv for charset UTF-8`, a misleading name. Creating the encoder works; converting
  doesn't, so a test of this must convert real text. Ktor's server text APIs don't go through iconv and work
  (`make e2e` sends `ñandú` both ways). docs/openwrt.md has the whole list.
- **`inet_pton` and `statvfs` are in `platform.linux`**, like `posix_spawn`.
- **The static linker is registered on every `tools/kt` run.** Changing `tools/ld-static` needs nothing
  else; changing the Kotlin version may change the dependency names in `kotlin/cli/module.yaml`.

## Prohibitions

- Running anything through a shell, or building a command line as a string.
- Exposing `apply`, actions or any request that changes a machine as an MCP tool, or giving the hub a key that
  opens the deploy role.
- Opening a path without resolving it and checking it with `PathPolicy`, or returning file, log, check or
  command-line text without `Redactor`.
- Reading `SSH_ORIGINAL_COMMAND`.
- Calling glibc's NSS from the binary: `getpwnam`, `getpwuid`, `getgrgid`, `getaddrinfo` of a name. The static
  binary can't load its plugins; use `Fs.accounts()` and friends.
- Replacing a Ktor piece with own code without showing, in the static binary, that Ktor fails there, and without a
  test that notices when it stops failing.
- Reading what `/proc` or `statvfs` say through `ps`, `ss` or `df`.
- Removing entries from `PathPolicy.BUILT_IN_DENY` or making it configurable.
- Adding a dependency to `core`.
