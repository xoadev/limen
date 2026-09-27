# OpenWrt, and one binary for every Linux

limen ships one file per architecture that runs on Debian, Ubuntu, Alpine and OpenWrt alike. Getting there took
working around what Kotlin/Native does not support yet. This is what was done, why, what it costs, and what to undo
when upstream catches up.

## The problem

- **Kotlin/Native only targets glibc.** `linuxX64` and `linuxArm64` link against glibc's shared libraries. OpenWrt
  and Alpine use musl, so a normal Kotlin/Native binary doesn't even start there: its loader, `ld-linux`, is
  missing. Open upstream since 2020: [KT-38876](https://youtrack.jetbrains.com/issue/KT-38876) (Alpine) and
  [KT-38900](https://youtrack.jetbrains.com/issue/KT-38900) (OpenWrt).
- **Linking against musl instead is no way out.** The Kotlin/Native runtime deadlocks in its garbage collector under
  musl's threads ([KT-85658](https://youtrack.jetbrains.com/issue/KT-85658)), and JetBrains proposes declaring musl
  unsupported.
- **So there is no Ktor for musl either.** Ktor's native artifacts are built on the same targets.

## The solution: glibc inside the binary

The binary is linked **statically against glibc**: it carries its own libc and uses nothing of the machine's but
the kernel, so musl or an older glibc on the machine don't matter.

- **`tools/ld-static`** is the linker the compiler calls for `limen`. It turns Kotlin/Native's dynamic link into a
  static one: `-static` and no interpreter, `crtbeginT.o`, the static unwinder `libgcc_eh`, and `libpthread` linked
  whole, because libstdc++ reaches pthread through weak symbols that a static link would otherwise leave null.
- **`tools/kt` registers it** as one of the compiler's own dependencies on every run. The compiler only accepts
  another linker by absolute path or as a dependency, and `kotlin/cli/module.yaml` can't hold an absolute path.
  Upstream: [KT-89362](https://youtrack.jetbrains.com/issue/KT-89362), with a patch in
  [JetBrains/kotlin#8127](https://github.com/JetBrains/kotlin/pull/8127) that makes `-linker-option -static` work;
  if it lands, most of `tools/ld-static` goes.
- **The release checks it.** `release.yml` refuses a binary that `file` doesn't call statically linked. A release
  build is about 7 MB (x86-64) and 6 MB (arm64).

## What a static glibc can't do, and what limen does instead

glibc loads some of its own parts at run time with `dlopen`, and a static binary has nothing to load them from.

| glibc part | What it needs at run time | limen |
|---|---|---|
| NSS: `getpwnam`, `getpwuid`, `getgrgid`, name resolution | `libnss_*.so` plugins | Reads `/etc/passwd` and `/etc/group` itself. Nodes and the hub are addressed by IP; `ssh` and `git`, separate programs, resolve names |
| iconv: converting text between encodings | `gconv` modules; only UTF-8 itself is built in | Ktor's **server** works: its text paths don't convert through iconv (`make e2e` sends `ñandú` both ways). Ktor's **client** doesn't: it encodes through UTF-16, a gconv module, and fails with `Failed to open iconv for charset UTF-8`. `limen join` uses `HttpLite` instead |
| `posix_spawn_file_actions_addopen`, before glibc 2.29 | the path pointer, kept until the spawn | `Proc` opens `/dev/null` itself and passes the descriptor: Kotlin/Native frees a `String`'s C copy when the call returns, and the child opened garbage |

## Ktor first

HTTP goes through Ktor: the hub's server is Ktor (CIO), with its ordinary text APIs. Own code only where Ktor is
**shown** not to work in the static binary, and with a test that notices the day it would:

- **Today there is one exception, `HttpLite`**: the two requests of `limen join` (`GET` and `POST /join/<code>`),
  over a plain socket, to an address.
- **The tripwire is `KtorCharsetTest`.** It encodes text with Ktor's UTF-8 encoder in the static test binary and
  expects it to fail. When a Ktor upgrade makes it pass, Ktor no longer needs iconv for UTF-8: switch `limen join` to
  Ktor's client and delete `HttpLite`.
- **Upstream**, Ktor made ISO-8859-1 and UTF-16 lazy for devices without them
  ([KTOR-7016](https://youtrack.jetbrains.com/issue/KTOR-7016)), but `Charsets.UTF_8` still converts through iconv
  on Linux ([`CharsetLinux.kt`](https://github.com/ktorio/ktor/blob/main/ktor-io/linux/src/CharsetLinux.kt)).
  Others who built static Kotlin/Native images hit it and carried glibc and gconv in the image instead
  ([youndie/katcher#56](https://github.com/youndie/katcher/pull/56)).

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
- The **arm64** binary is only tested to start: under qemu-user, glibc's `posix_spawn` fails on qemu's `clone`. It
  needs a run on real hardware, such as a Banana Pi.
- **Garbage-collector load** under OpenWrt is not tested. The musl deadlock of KT-85658 comes from musl's threads,
  which this binary doesn't use; and on a node every `limen` process lives one request.

## When upstream moves

| If | Then |
|---|---|
| Kotlin/Native links static executables itself (KT-89362) | Most of `tools/ld-static` and its registration in `tools/kt` go |
| Ktor encodes UTF-8 without iconv on Linux | `KtorCharsetTest` fails: `limen join` moves to Ktor's client, `HttpLite` goes |
| Kotlin/Native gets a musl target without the GC deadlock | Nothing needed; the static glibc binary keeps working |
