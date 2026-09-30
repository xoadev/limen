# system

What any Linux machine can answer, systemd or not: its state, its busiest processes and what it listens on.

| Script | Answers | Changes the machine |
|---|---|---|
| `status` | Uptime, load, memory, disk use of each real filesystem, whether a reboot is pending | no |
| `processes` | The processes using the most CPU or memory, by name: never their command lines | no |
| `ports` | Listening TCP and UDP sockets and their processes | no |
| `memory` | `/proc/meminfo` in MiB, swap use, memory pressure (PSI), and the kills for lack of memory since boot | no |
| `dns_lookup` | Resolves a name, with the machine's resolver or a DNS server you give | no |

Needs `/proc`, `df`, `awk`, and `ss` or `netstat`; `dns_lookup` uses `dig`, else `nslookup`, else `getent`, which can only list addresses. With procps `ps` sorts the processes; with busybox, `top` does.
