# openwrt

A router running OpenWrt: its board, services and log. POSIX `sh`, as busybox's `ash` runs it.

| Script | Answers | Changes the machine |
|---|---|---|
| `board` | Model, hostname, kernel, OpenWrt release and target | no |
| `services` | Each service in `/etc/init.d`: whether it starts at boot, how many of its procd instances run | no |
| `logs` | The system log, of every process or of one | no |
| `restart_service` | Restarts one service with `/etc/init.d/<name> restart` | **yes** |

Needs `ubus`, `jsonfilter` and `logread`, all in a default image. The `system` pack works here too.

`restart_service` refuses `network`, which would cut the connection its answer goes back through. Narrow its
`pattern` to the services you would let the agent restart.
