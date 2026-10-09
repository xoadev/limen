# Security policy

limen's purpose is a security boundary: an agent may read the files a machine allows and run the scripts it offers,
with the arguments they declare, and nothing more. A way around that boundary is a vulnerability, and it is treated
before anything else.

## Reporting a vulnerability

**Do not open a public issue.** Report it privately through GitHub:
[Security → Report a vulnerability](https://github.com/xoadev/limen/security/advisories/new).

Include what you did, what you expected, and what happened, with the limen version (`limen --version`), the
platform (Debian, OpenWrt…) and whether it concerns the hub or a node. You will get an answer as soon as possible;
this is a small project, so please allow a few days.

## What counts

- The hub's key running anything but `limen gate`, or reaching a shell.
- Reading a path the node's policy denies, including through symlinks or `..`.
- An argument reaching a shell, an argument a script's header doesn't declare reaching it, or running a file that
  is not a script of the node's packs.
- `grep` or `tail` telling apart what redaction masked.
- The MCP server over HTTP answering without the token.
- A script that `[approval]` names running without a person's yes, other than by the MCP client answering for
  them.
- A join invitation used more than once, after it expires, or to add a node other than the one it names.
- A secret leaving a node through something limen masks (redaction) in a way the documentation says it doesn't.
- Altering a release's files, the image or `install.sh` without write access to this repository, or a workflow
  handing that access to code it runs.

## What doesn't

These are limits of the design, documented in the [threat model](docs/spec.md#13-threat-model):

- Anything a script of the node's packs does, or prints: they belong to root and limen trusts them.
- The agent running a script it was led to by text in a log or a file: every script on offer is the operator's
  choice, and `[approval]` is how to have a person confirm the ones that change things.
- An MCP client, or the hub, compromised: either can answer yes to its own approval questions.
- Secrets in files the operator allowed: masking is a safety net, not the protection.
- Whatever the node allows to be read reaching the model provider the hub talks to.
- Whoever can change a node's packs, or push where one of its scripts fetches them from, running code on it as root.
- A root login by password on OpenWrt's dropbear: `limen install` warns about it; turning it off is the operator's.
- Whoever holds write access to this repository, or controls the maintainers' GitHub accounts: `SHA256SUMS` comes
  from the same release as the binaries, so it catches a broken download, not a replaced one, and `install.sh`
  is served from `main`. Each binary, the image and each pack's tarball carry a build provenance attestation
  (`gh attestation verify`), which a file replaced outside the release workflow fails.

## Supported versions

Fixes go into the latest release. limen is pre-1.0: upgrade to the latest version before reporting.
