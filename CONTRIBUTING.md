# Contributing to limen

Thanks for wanting to help. limen is small and opinionated, and it guards machines: a change is welcome when it keeps
the one promise the project makes —**the machine decides**— and comes with what proves it.

## Before you start

- **Bugs and ideas** go in [issues](https://github.com/xoadev/limen/issues). For anything bigger than a fix, open one
  first: the design lives in [`docs/spec.md`](docs/spec.md), and a change of behaviour starts there.
- **Security problems never go in a public issue**: see [`SECURITY.md`](SECURITY.md).
- [`AGENTS.md`](AGENTS.md) is the full working contract, for people and coding agents alike. Its shared head holds
  the rules every change follows —English everywhere, names, comments, checks, dependencies, pull requests—; below it
  are limen's layout, rules, known pitfalls and prohibitions. This file is the short version and repeats none of it.

## Set up

You need Linux (x86-64 or arm64), [rustup](https://rustup.rs), `make`, and Docker for the end-to-end tests.
`rust-toolchain.toml` pins the Rust version; rustup installs it, and the musl targets, on the first build.

```sh
make check   # lint, build and every test: what CI runs, and what must be green
make cli     # the static binary: target/<arch>-unknown-linux-musl/debug/limen
make e2e     # Debian and OpenWrt containers with a real SSH server (SUITE=debian|openwrt|join for one)
make help    # the rest
```

Go through `make`: a plain `cargo build` builds for this machine's libc, not the static binaries that ship.

## Making a change

1. **Tests with it.** Rules and parsers go in `limen-core`, tested with captured samples and no machine. Anything touching
   the gate, `install`, `join`, SSH or sudo also needs a scene in `tools/e2e.sh`.
2. **Documentation with it**: `docs/spec.md` first, then the README, a pack's README, `docs/scripts.md`,
   `SECURITY.md`, whatever the change makes wrong.
3. **`make check` green**, and `make e2e` when the change reaches a real machine. CI runs both on your pull request:
   `check` always, `e2e` when it touches code.
4. **Keep the boundary where it is.** No shell anywhere, every path through the path policy, every argument through
   its schema, nothing read by the hub that the node didn't allow. A change that moves a limit from the node to the
   hub will not be merged.
5. **Small and pure dependencies.** A new crate must build for both musl targets without a C compiler, and earn
   its size: the binary goes on routers.

## Pull requests

- Titles follow [Conventional Commits](https://www.conventionalcommits.org) with limen's types and scopes
  ([`AGENTS.md`](AGENTS.md#mandatory-loop)): `fix(gate): …`, `feat(hub): …`. The title is also what the release
  notes are made of.
- Say why the change exists and paste the end of `make check` (and `make e2e` when it applies).

By contributing you agree that your contribution is licensed under the [Apache License 2.0](LICENSE).
