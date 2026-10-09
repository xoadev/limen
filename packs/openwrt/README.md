# openwrt

A router running OpenWrt: its board, services, log, network, Wi-Fi clients, DHCP leases and packages. POSIX `sh`, as busybox's `ash` runs it.

Needs limen 0.1.3 or later: its headers use `read_only`, which older versions refuse.

| Script | Answers | Changes the machine |
|---|---|---|
| `board` | Model, hostname, kernel, OpenWrt release and target | no |
| `services` | Each service in `/etc/init.d`: whether it starts at boot, how many of its procd instances run | no |
| `logs` | The system log, of every process or of one | no |
| `interfaces` | Each logical interface (wan, lan…): up or down and for how long, protocol, device, addresses, gateway, DNS | no |
| `dhcp_leases` | The devices given an address: host name, address, MAC, when the lease ends, static or not; odhcpd's IPv6 leases | no |
| `wifi_clients` | The devices on each Wi-Fi network: MAC, host name, signal, when last heard, rates. Never the keys | no |
| `upgradable` | Packages with a newer version in the feeds, with `opkg` or, from OpenWrt 25, `apk` | no (it refreshes the lists first, in RAM, unless `refresh` is false) |
| `restart_service` | Restarts one service with `/etc/init.d/<name> restart` | **yes** |

Needs `ubus`, `jsonfilter` and `logread`, all in a default image; `wifi_clients` needs `iwinfo`, there on any router
with Wi-Fi. The `system` pack works here too.

`upgradable` only lists: upgrading packages one by one can leave a router that doesn't boot, and a new release goes
through `sysupgrade`.

`restart_service` refuses `network`, which would cut the connection its answer goes back through. Narrow its
`pattern` to the services you would let the agent restart.
