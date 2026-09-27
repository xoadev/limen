# Security policy

limen's purpose is a security boundary: an agent may read what a machine allows and nothing more, and may change
nothing. A way around that boundary is a vulnerability, and it is treated before anything else.

## Reporting a vulnerability

**Do not open a public issue.** Report it privately through GitHub:
[Security → Report a vulnerability](https://github.com/xoadev/limen/security/advisories/new).

Include what you did, what you expected, and what happened, with the limen version (`limen --version`), the
platform (Debian, OpenWrt…) and whether it concerns the hub or a node. You will get an answer as soon as possible;
this is a small project, so please allow a few days.

## What counts

- The read role (the hub's key) running anything but `limen gate`, or reaching a shell.
- Reading a path the node's policy denies, including through symlinks or `..`.
- An argument reaching a shell, or running a script that is not in the node's script directories.
- The read role doing what only the deploy role may: `sync`, `apply`, actions.
- The MCP server over HTTP answering without the token.
- A join invitation used more than once, after it expires, or to add a node other than the one it names.
- A secret leaving a node through something limen masks (redaction) in a way the documentation says it doesn't.

## What doesn't

These are limits of the design, documented in the [threat model](docs/spec.md#13-threat-model):

- Anything a script in the node's script directories does: they belong to root and limen trusts them.
- Secrets in files the operator allowed: masking is a safety net, not the protection.
- Whatever the node allows to be read reaching the model provider the hub talks to.
- Whoever can push to a node's repository branch running code on it as root.
- A root login by password on OpenWrt's dropbear: `limen install` warns about it; turning it off is the operator's.

## Supported versions

Fixes go into the latest release. limen is pre-1.0: upgrade to the latest version before reporting.
