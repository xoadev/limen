# debian

apt packages and rebooting, on a Debian (or derived) machine with systemd.

| Script | Answers | Changes the machine |
|---|---|---|
| `updates` | Packages that can be upgraded, the security ones, those apt keeps back, whether a reboot is pending, the age of the package lists | no (with `refresh`, only the lists) |
| `upgrade` | Upgrades the packages with `apt-get upgrade --with-new-pkgs`: a new package only when an upgrade needs one (a kernel), none removed, configuration files kept | **yes** |
| `reboot` | Schedules a reboot 1 to 60 minutes away; refuses while apt or dpkg run | **yes** |
| `cancel_reboot` | Cancels a scheduled reboot or shutdown | **yes** |

- `upgrade` does not reboot: it says whether one is needed. **A package may restart its own services**: upgrading
  Docker restarts its containers. It ignores `HUP` and `PIPE` and writes to `/var/log/limen-upgrade.log`, so a dropped
  connection doesn't kill `dpkg` half way.
- `reboot` waits so the answer arrives first; the machine is unreachable until it is back.
- `updates` simulates with the flags `upgrade` uses: what it lists is what `upgrade` would do.

Needs `apt-get`, `awk`, `pgrep` and `shutdown` (systemd).
