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
| `docs/` | `spec.md` |
| `kotlin/` | The Kotlin Toolchain project: `project.yaml`, a `module.yaml` per module, the `kotlin` wrapper that pins the toolchain |
| `kotlin/core/` | Pure rules: protocol, request schemas, argument validation, configuration, TOML, script headers, path policy, redaction, parsers of what system programs print. No processes, files or network |
| `kotlin/cli/` | The `limen` binary: `os/` (processes, files), `node/` (gate, requests, scripts, install), `hub/` (SSH client, MCP server, transports) |
| `tools/` | Scripts the `Makefile` calls |
| `etc/` | `Dockerfile` of the hub image; `e2e/` the node image of `make e2e` |
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
| HTTP | Ktor server, CIO engine | Validated on Native by foco |

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
| `make e2e` | A Debian container with sshd; `limen install` inside; the hub against it with both keys. Needs Docker. Not in `make check` |
| `make docker` | The hub image, `limen:local` (or `IMAGE=…`) |
| `make local-install` | The binary in `~/.local/bin` |
| `make hooks` | The `pre-push` hook |

`make -k check` runs every check even when one fails.

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

## Prohibitions

- Running anything through a shell, or building a command line as a string.
- Exposing `apply`, actions or any request that changes a machine as an MCP tool, or giving the hub a key that
  opens the deploy role.
- Opening a path without resolving it and checking it with `PathPolicy`, or returning file, log, check or
  command-line text without `Redactor`.
- Reading `SSH_ORIGINAL_COMMAND`.
- Removing entries from `PathPolicy.BUILT_IN_DENY` or making it configurable.
- Adding a dependency to `core`.
