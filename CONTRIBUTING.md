# Contributing to limen

Thanks for wanting to help. limen is small and opinionated, and it guards machines: a change is welcome when it keeps
the one promise the project makes —**the machine decides**— and comes with what proves it.

## Before you start

- **Bugs and ideas** go in [issues](https://github.com/xoadev/limen/issues). For anything bigger than a fix, open one
  first: the design lives in [`docs/spec.md`](docs/spec.md), and a change of behaviour starts there.
- **Security problems never go in a public issue**: see [`SECURITY.md`](SECURITY.md).
- [`AGENTS.md`](AGENTS.md) is the full working contract —layout, rules, known pitfalls, prohibitions—, for people and
  coding agents alike. This file is the short version.

## Set up

You need Linux x86-64 (the Kotlin/Native compiler runs there), `make`, a JDK for the linter, and Docker for the
end-to-end tests. The Kotlin toolchain downloads itself on the first build.

```sh
make check   # lint, build and every test: what CI runs, and what must be green
make cli     # the binary: kotlin/build/tasks/_cli_linkLinuxX64Debug/cli.kexe
make e2e     # Debian and OpenWrt containers with a real SSH server (SUITE=debian|openwrt|join for one)
make help    # the rest
```

Always go through `make`: the targets set up the static linker (`tools/ld-static`) that a direct `kotlin build`
would miss.

## Making a change

1. **Spec first.** If what you are changing is not in `docs/spec.md`, or is wrong there, change the spec in the same
   pull request.
2. **Tests with it.** Rules and parsers go in `core`, tested with captured samples and no machine. Anything touching
   the gate, `install`, `join`, SSH or sudo also needs a scene in `tools/e2e.sh`.
3. **`make check` green**, and `make e2e` when the change reaches a real machine.
4. **Keep the boundary where it is.** No shell anywhere, every path through the path policy, every argument through
   its schema, nothing read by the hub that the node didn't allow. A change that moves a limit from the node to the
   hub will not be merged.
5. **Ktor first.** Own code instead of a library only where the library is shown not to work in the static binary,
   with a test that notices when it would ([`docs/openwrt.md`](docs/openwrt.md)).

## Pull requests

- Titles follow [Conventional Commits](https://www.conventionalcommits.org): `fix(gate): …`, `feat(hub): …`,
  `docs: …`. Pull requests are merged with squash, so the title is the commit that stays and what the release
  notes are made of.
- Code, comments and documentation are in English. Comments say *why*, not what the next line does.
- Describe what changed and paste the end of `make check` (and `make e2e` when it applies).

By contributing you agree that your contribution is licensed under the [Apache License 2.0](LICENSE).
