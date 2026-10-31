# system

What any Linux machine can answer, systemd or not: its state, its busiest processes, what it listens on, its network,
its disks and its clock; and, one value at a time, its CPU, memory, disk, temperature and GPU for Home Assistant.

Needs limen 0.1.3 or later: its headers use `read_only`, which older versions refuse.

| Script | Answers | Changes the machine |
|---|---|---|
| `status` | Uptime, load, memory, disk use of each real filesystem, whether a reboot is pending | no |
| `processes` | The processes using the most CPU or memory, by name: never their command lines | no |
| `ports` | Listening TCP and UDP sockets and their processes | no |
| `memory` | `/proc/meminfo` in MiB, swap use, memory pressure (PSI), and the kills for lack of memory since boot | no |
| `dns_lookup` | Resolves a name, with the machine's resolver or a DNS server you give | no |
| `network` | The default route, each interface with its state and addresses, the routes, the DNS servers in use | no |
| `reach` | Whether a host answers: a TCP connection to a port, or ping without one | no |
| `kernel_log` | The kernel's log since boot (`dmesg`), warnings and worse by default: disk errors, OOM kills, failing devices | no |
| `time` | The clock in UTC, the time zone, which NTP client keeps it and whether it is synchronised | no |
| `filesystems` | Each real file system: size, space and inodes used, read-only or not | no |
| `disks` | Each disk's size and model, SMART health and the attributes that warn of a failure, software RAID | no |
| `cpu_used` | Percent of CPU in use over one second, a whole number | no |
| `memory_used` | Percent of memory in use, a whole number | no |
| `disk_used` | Percent used of the file system that holds `path` (`/` unless given), a whole number | no |
| `temperature` | The hottest temperature sensor the kernel exposes, in °C, a whole number | no |
| `gpu` | One GPU's `utilization` or `memory_used` in percent, or `temperature` in °C, a whole number; `index` picks the GPU | no |

Needs `/proc`, `df`, `awk`, and `ss` or `netstat`; `dns_lookup` uses `dig`, else `nslookup`, else `getent`,
which can only list addresses. With procps, `ps` sorts the processes; with busybox, `top` does.

- `reach` lets the agent probe what this machine can reach, inside its network too. Leave it out of a machine where
  that matters. A port needs `nc` with `-z`, `bash` or `curl`; OpenWrt's default image has none of them, so there it
  only pings.
- `disks` needs `smartctl` (smartmontools) for health; without it, it lists the disks only. It never prints serial
  numbers.
- `kernel_log` reads what the kernel keeps in memory, so only since boot; `journal` (systemd) keeps earlier boots.
- `temperature` reads `/sys/class/thermal` and `/sys/class/hwmon`; a machine with neither, a VM most often, has none,
  and the script fails instead of printing a 0.
- `gpu` reads NVIDIA's through `nvidia-smi` and AMD's through the kernel's `amdgpu`; Intel's integrated GPUs say
  nothing a script can read without `intel_gpu_top`.

## For Home Assistant

`cpu_used`, `memory_used`, `disk_used`, `temperature` and `gpu` print one number, nothing else: what a sensor of a
hub with `[mqtt]` publishes ([how](../../README.md#home-assistant-over-mqtt)). The pack offers them all; Home
Assistant gets only the ones the hub's `limen.toml` declares:

```toml
[[mqtt.sensor]]
node = "nas"
id = "cpu"
script = "cpu_used"
discovery = { unit_of_measurement = "%", state_class = "measurement" }

[[mqtt.sensor]]
node = "nas"
id = "srv"
script = "disk_used"
args = { path = "/srv" }
every = "10m"
discovery = { name = "/srv used", unit_of_measurement = "%", state_class = "measurement" }

[[mqtt.sensor]]
node = "nas"
id = "gpu_temperature"
script = "gpu"
args = { metric = "temperature" }
discovery = { device_class = "temperature", unit_of_measurement = "°C", state_class = "measurement" }
```

They are tools for the agent too, like every script of the pack.
