# AGENTS.md — working contract

This repository is **limen**: an MCP server for inspecting Linux machines, where every limit is enforced on the
machine itself. This file is the working contract for any agent (Claude Code or another) and for people.
`CLAUDE.md` points here.

## Sources of truth

| What | Where | Rule |
|---|---|---|
| Product | `docs/spec.md` | The only source of truth. If code and spec disagree, the spec wins; if the spec is wrong, change the spec first |
| Protocol, requests and their arguments | `crates/limen-core/src/requests.rs`, `protocol.rs` | The same values validate on the node and become MCP tool schemas on the hub. The spec's tables describe them |
| What a user needs | `README.md` | Install, configure, use. Written from the spec, never the other way round |

What an agent learns working here is written here or in the spec — never in an agent's private memory, where the
next session, another agent or a person can't read it.

## Layout

| Folder | What |
|---|---|
| `docs/` | `spec.md`; `openwrt.md`, OpenWrt as a node and the one binary for every Linux |
| `Cargo.toml`, `rust-toolchain.toml` | The workspace and the Rust version it is built with, targets included; `.cargo/config.toml` links the musl targets with `rust-lld` |
| `crates/limen-core/` | Pure rules: protocol, request schemas, argument validation, configuration, TOML reading, script headers, path policy, redaction, the join's formats, parsers of what system programs print. No processes, files or network |
| `crates/limen/` | The `limen` binary: `os/` (processes, files), `node/` (gate, requests, platforms, repository, scripts, install, join), `hub/` (SSH client, MCP server, transports, the hub's directory) |
| `install.sh` | The installer a user pipes into `sh` on a new machine. POSIX `sh` (OpenWrt has no bash); `make e2e` runs it under dash and ash |
| `tools/` | Scripts the `Makefile` calls: `cargo.sh` builds, `lint.sh`, `e2e.sh`, `docker.sh` |
| `etc/` | `Dockerfile` of the hub image; `e2e/` the Debian node image of `make e2e` (OpenWrt's is the official one) |
| `.github/workflows/` | CI, always through `Makefile` targets |

## Crates

| Crate | Type | May depend on | Never on |
|---|---|---|---|
| `limen-core` | Library | Pure crates: `serde`, `serde_json`, `toml_edit`, `indexmap`, `regex`, `sha2`, `hmac`, `base64` | processes, files, network, system calls |
| `limen` | The binary, static musl for `x86_64` and `aarch64` | `limen-core`, `clap`, `rustix`, `tiny_http` | — |

Rules that hold:

- **Anything testable without a machine goes in `limen-core`.** A new request's argument rules, a new output format
  to parse: there, with a test that feeds it a captured sample. The binary only runs things and glues.
- **A new dependency must build for both musl targets without a C compiler** —`make cli ARCH="x86_64 aarch64"`—
  and earn its size: the binary goes on routers with a few megabytes of flash. `make e2e` runs the result on Debian
  and OpenWrt.
- **No `unsafe` code**: `#![forbid(unsafe_code)]` in both crates. What `std` lacks (`statvfs`, `poll`, `kill` of a
  process group, termios, `uname`) comes from `rustix`'s safe API.

## Technical choices

Each one had an alternative. Changing one is changing this table and the spec's §14 in the same PR.

| What | With | Why this one |
|---|---|---|
| Limits | On the node, in `gate` | A client-side policy is bypassed by a compromised or deceived client |
| Transport to nodes | The system `ssh` with a forced command | No daemon on the nodes; the system client brings agent support, multiplexing and the operator's configuration |
| Request | One JSON line on stdin | No word splitting; `sudo` keeps stdin and drops `SSH_ORIGINAL_COMMAND` |
| Language | Rust | One small static binary per architecture, no runtime, and memory safety in a program that runs as root at a security boundary. Kotlin/Native was the first implementation; docs/openwrt.md says what it cost |
| Child processes | `std::process::Command`, argument arrays, own process group | A timeout reaches what a script started; never a shell |
| MCP | Own JSON-RPC 2.0, no SDK | A few hundred lines, fully under control; the SDKs bring an async runtime |
| TOML | `toml_edit`: `#[derive(Deserialize)]` with `deny_unknown_fields` to read, `DocumentMut` to edit | A typo is an error with its line; editing a document keeps the operator's comments, and a value can't become a table |
| CLI | `clap` (derive) | Help and usage from the definitions |
| HTTP | `tiny_http` for the hub; `join`'s two requests over `std::net` (`os/http.rs`) | Synchronous and small; the hub is reached over a VPN or the LAN, and a client library was ten crates for two requests to our own server |
| libc | musl, linked statically by Rust's own targets and `rust-lld` | One file per architecture runs on any Linux, and building for arm64 needs no cross compiler |
| System facts | `/proc`, `statvfs`, `/etc/passwd` | `ps`, `ss` and `df` differ between distributions and busybox |

## Conventions

- **Everything in English**: code, comments, documentation, commit messages, tool descriptions.
- **Comments only when they add something**: the *why* the code can't say — a rule of the spec, a platform
  constraint, a decision with an alternative. Never a narration of the line below.
- **Security rules are code, not habits.** Every read goes through `PathPolicy` with the resolved path; every
  argument through `params::validate`; every program through `proc::run` with an argument array; every text leaving
  the node through `Redactor`.
- **Errors say what to do**: `no SSH key at /data/id_ed25519 ([ssh].identity in /data/limen.toml)`, not
  `file not found`.

## Mandatory loop

```
spec → change → make check → commit
```

1. Find what you are implementing in `docs/spec.md`. If it is not there, or is wrong, change the spec **first**.
2. Write or adjust the tests first: `core` with samples, `cli` with fakes, `tools/e2e.sh` for what only a real
   sshd proves. A test must fail when the promise breaks: break the code on purpose and watch it go red. An e2e
   scene that checks for an absence (`refuse`) also names something the answer must contain, or an empty answer
   —a crash— passes it.
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

Never launch the build by hand: `cargo` alone builds for this machine's libc, not the static binaries. If no target
covers what you need, add the target or the script in `tools/`.

| Target | What it does |
|---|---|
| `make check` | `lint build test`. What CI runs |
| `make lint` | rustfmt and clippy (every warning an error) over `crates/`, shellcheck over `tools/` and `install.sh`, actionlint over the workflows. `FIX=1` formats instead of checking |
| `make build` / `make test` | Every crate, tests included, for this machine |
| `make cli` | Only the static binary. `VARIANT=release` for the optimised one; `ARCH="x86_64 aarch64"` for both architectures |
| `make e2e` | Containers with a real SSH server —Debian with OpenSSH and a repository, OpenWrt's image with dropbear—, `limen install` inside, the hub against them with both keys. `SUITE=debian`, `openwrt` or `join` for one. Needs Docker. Not in `make check` |
| `make docker` | The hub image, `limen:local` (or `IMAGE=…`, `TAGS=…`, `LABELS=…`), from the binaries of `make cli` (or `BINARY_AMD64=…`, `BINARY_ARM64=…`). `PLATFORMS=linux/amd64,linux/arm64 PUSH=1` pushes both under one tag (after `make cli ARCH="x86_64 aarch64"`); without `PUSH`, one platform, because Docker loads one per tag. The release goes through this same target |
| `make local-install` | The binary in `~/.local/bin` |
| `make hooks` | The `pre-push` hook |

`make -k check` runs every check even when one fails.

## CI

Every workflow calls `Makefile` targets: what is checked is defined once, and CI can't drift from a laptop. Every
third-party action is pinned by commit SHA with its version in a comment (`actions/checkout@3d3c42e… # v7.0.1`):
a tag can move, a SHA can't. Secrets reach a step through `env:`, never interpolated into `run:`.

| Workflow | When | What |
|---|---|---|
| `check.yml` | Every push to `main` and every PR | `make -k check`, and both binaries linked and checked static: the one mandatory gate. On `main`, a red run opens (or comments) the `main-red` issue |
| `release.yml` | Every push to `main`; publishing a draft release; or by hand | On a push, [convco-version](https://github.com/xoadev/convco-version) reads the conventional commits that touched what goes into the binary or the image and rewrites **one draft** release, `vX.Y.Z`, with what went in. **Publishing it** —a person, from the releases page— creates the tag, and that builds and publishes: waits for `check.yml` green on that commit, stamps the version (`limen --version`), builds both static binaries in release, runs `make e2e` with the release binary, checks they are static, builds the image per architecture and starts it, pushes it to `ghcr.io/<repo>` as `X.Y.Z`, `latest` and `build<run>`, and attaches the binaries and `SHA256SUMS`. By hand it builds everything as `dev` and publishes nothing |
| `cli.yml` | Label `cli` on a PR, or by hand | Both binaries (debug) as an artifact, with the link commented on the PR: for trying a change on a real node |
| `e2e.yml` | Every PR that touches code (`crates/`, the Cargo files, `tools/`, `etc/`, `install.sh`, `Makefile`); weekly; by hand | `make e2e`, every suite. Weekly because the Debian and OpenWrt images it runs move upstream |
| `dependabot.yml` | Weekly | Pull requests that update the pinned actions, SHA and version comment together |

The version is never written in the code: the commits decide it, and it only exists once a release is published.
The build reads it from `LIMEN_VERSION`, `LIMEN_BUILD_DATE` and `LIMEN_BUILD_NUMBER` (`tools/cargo.sh` checks their
shape); a local build says `dev`.

## What has already failed

Mistakes made here, with what avoids them. `make check` does not see them.

- **`O_NOFOLLOW` refuses symlinks, `/etc/os-release` included.** `fs::read` opens what the caller already resolved
  and checked; resolve with `fs::real_path` first, or use `fs::read_following` for a configuration file.
- **A child can close its stdin before reading it.** Writing to it then fails with EPIPE (Rust ignores SIGPIPE);
  `proc::run` treats that as the end of the input, not an error. A test sends a megabyte to `/bin/true`.
- **A multiplexed SSH connection skips the host key check.** With `ControlMaster`, a second connection to the
  same node reuses the first one's socket, verified or not by the current `known_hosts`. `make e2e` changes
  `XDG_RUNTIME_DIR` before testing a wrong host key; a test of anything SSH-level must do the same.
- **Script trust depends on who runs it.** A script is trusted when it and every parent directory are owned by
  root (or the user limen runs as) and not writable by group or others. `/tmp` and a group-writable checkout fail
  that on purpose; tests that run scripts live in `make e2e`, where they are root's.

- **`/proc/mounts` is a symlink**, and `Fs.read` opens with `O_NOFOLLOW`: read `/proc/self/mounts`.
- **Two roles as the same user share a multiplexed SSH connection.** On OpenWrt both keys log in as root; a
  deploy request through `ControlMaster` rode the socket the read key had opened and landed in the read role.
  Deploy requests never multiplex.
- **A forced command only holds a login by key.** OpenWrt's image has root without a password and dropbear
  accepts it: the e2e "passed" a session that never used the key. Its dropbear runs with password logins off
  (`-s`), and `install` warns about an empty root password.
- **A join is input from both sides.** The hub's name for a node became the node's repository folder, pasted
  into its `limen.toml`: a hostile hub could write `[scripts]` or `[repo].dir` there. Values from the other side
  are checked against their pattern and written as TOML values through `toml_edit`, never pasted into text; the
  same holds for what a node sends the hub.
- **An e2e scene that only checked for an absence passed on a dead hub.** `limen forget` from another process made
  the running `serve` recurse until its stack overflowed; "and it is gone" found nothing, as it expected. `refuse`
  now takes a needle the answer must contain.
- **`serde_json`'s `Map::remove` moves the last key into the hole** when key order is kept (`preserve_order`):
  arguments reached a node reordered. `shift_remove` keeps the order.
- **Rust's `regex` has no lookaround.** A redaction pattern that needs "not preceded by" matches that character
  instead and keeps it outside the `secret` group; operators' `redact.patterns` follow the same syntax.

## Prohibitions

- Running anything through a shell, or building a command line as a string.
- Exposing `apply`, actions or any request that changes a machine as an MCP tool, or giving the hub a key that
  opens the deploy role.
- Opening a path without resolving it and checking it with `PathPolicy`, or returning file, log, check or
  command-line text without `Redactor`.
- Reading `SSH_ORIGINAL_COMMAND`.
- Resolving users, groups or host names through libc: `getpwnam` and friends read other files on other libcs.
  Use `fs::accounts()` and addresses.
- Reading what `/proc` or `statvfs` say through `ps`, `ss` or `df`.
- Removing entries from `path_policy::built_in_deny` or making it configurable.
- Adding a crate to `limen-core` that does I/O, or a C dependency anywhere.
