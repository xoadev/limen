# systemd

Units and their journals, on a machine with systemd.

| Script | Answers | Changes the machine |
|---|---|---|
| `units` | Units of one type, by state or name glob, with a count of the failed ones | no |
| `unit` | One unit: state, restarts, main process, memory, unit file, last journal lines | no |
| `unit_logs` | One unit's journal over a period, by priority | no |
| `journal` | The whole journal over a period, by priority, or all of an earlier boot | no |
| `restart_unit` | Restarts one unit | **yes** |

Needs `systemctl` and `journalctl`.

`unit` shows chosen properties rather than `systemctl status`, which prints the command line of every process of the
unit. `restart_unit` takes any unit: narrow its `pattern` to the ones you would let the agent restart, e.g.
`'^(nginx|immich-server)\.service$'`.
