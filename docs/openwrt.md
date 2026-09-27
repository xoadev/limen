# OpenWrt, and one binary for every Linux

limen ships one file per architecture that runs on Debian, Ubuntu, Alpine and OpenWrt alike.

## One binary

- **Static, against musl.** Rust's `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` targets carry
  musl's C library inside the binary, which then needs nothing of the machine but the kernel: glibc or musl, old or
  new, doesn't matter. About 3 MB for x86-64 and 2.5 MB for arm64 in release.
- **No cross compiler.** `.cargo/config.toml` links both targets with `rust-lld`, the linker the Rust toolchain
  ships, and no dependency needs a C compiler: `make cli ARCH="x86_64 aarch64"` builds both on any Linux.
- **The release checks it.** `release.yml` refuses a binary that `file` doesn't call statically linked, and runs
  every end-to-end scene with the release binary before publishing it.
- **Nothing resolved through the C library.** Users and groups come from `/etc/passwd` and `/etc/group`, read by
  limen; processes, sockets and filesystems from `/proc` and `statvfs`. The same code answers on glibc and musl
  machines, and with busybox's tools or without them.

The first implementation was Kotlin/Native, which only targets glibc. Making it static took an own linker script,
an own HTTP client where glibc's iconv was missing, and parsing `/etc/passwd` because glibc's NSS can't load in a
static binary; its arm64 build couldn't start processes under qemu. The Rust one needs none of that.

## OpenWrt as a node

| What | How limen handles it |
|---|---|
| SSH is dropbear, no sudo, no tools to add users | Both keys go in root's `/etc/dropbear/authorized_keys`, each held to `limen gate` by its forced command. Only lines with `limen gate` are ever touched; the administrator's keys stay |
| dropbear has no `from=` | `--from` is refused; limit who reaches port 22 with the firewall |
| A forced command only holds a login **by key** | OpenWrt ships root without a password, and dropbear lets anyone in without a key then. `install` warns, and says how to turn password logins off (`uci set dropbear.@dropbear[0].PasswordAuth=off`, `RootPasswordAuth=off`) |
| `sysupgrade` wipes what it isn't told to keep | `/lib/upgrade/keep.d/limen` keeps the binary and `/etc/limen` |
| Services are procd's | `ubus call service list`, `/etc/rc.d` for enablement |
| Logs are `logread` | Parsed and filtered by limen; `TZ=UTC` makes its times UTC |
| busybox's `ps`, `ss` and `df` lack the options | Processes, sockets and filesystems come from `/proc` and `statvfs` on every platform |
| `/var/log` is RAM | The audit log rotates at 5 MB and doesn't survive a reboot |
| A repository needs git | The `git-http` package; the installer offers it |
| The hub logs in as root | Its entry says `user = "root"`; a join sets it |

## What is tested, and what is not

- `make e2e` runs OpenWrt's official image (`openwrt/rootfs:x86-64`, busybox, musl, dropbear): installing, reading,
  the gate as the only way in, the deploy role, joining a hub, uninstalling.
- The **arm64** binary runs under qemu-user, child processes included, but has not run on hardware yet, such as a
  Banana Pi.
- **32-bit routers** (ARMv7, MIPS) have no release binary. ARMv7 builds and runs under qemu
  (`armv7-unknown-linux-musleabihf`, 2.4 MB); MIPS needs Rust's nightly.
