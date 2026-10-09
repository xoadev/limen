# docker

Containers, their logs, images and Compose projects, and cleaning up after them.

Needs limen 0.1.3 or later: its headers use `read_only`, which older versions refuse.

| Script | Answers | Changes the machine |
|---|---|---|
| `containers` | Every container with its image, state and health | no |
| `container` | One container: image, state, health, restarts, mounts, ports, networks, labels | no |
| `container_stats` | What each running container uses right now: CPU, memory and processes, the hungriest first | no |
| `container_logs` | One container's log over a period, stdout and stderr together | no |
| `images` | Every image with its size and age, and whether a container uses it: in use, unused or dangling | no |
| `disk_usage` | What images, containers, volumes and build cache take, what could be freed, and each volume's size | no |
| `compose_projects` | Each Compose project: directory, containers and their state, and whether `compose_update` may update it | no |
| `restart_container` | Restarts one container as it is | **yes** |
| `compose_update` | Pulls the newest images of one Compose project and recreates what changed, then shows its state. Only the projects the operator lists | **yes** |
| `purge` | Deletes stopped containers, unused networks, dangling images and build cache; with `images`, every unused image. Never volumes | **yes** |

Needs the `docker` CLI and daemon.

`container` never prints the environment or the command line, where secrets live, and hides the value of a label
whose name mentions a password, token or auth. Narrow `restart_container`'s `pattern` to the
containers you would let the agent restart. It doesn't apply compose changes: `compose_update` does, for the projects
listed in `/etc/limen/compose-projects`, one name per line; with no file it updates nothing. An update can bring
breaking changes, and text in a log can lead the model to call any script, so which projects follow their images is
the operator's choice, written where only root writes and `read_file` never reads:

```sh
printf '%s\n' immich paperless > /etc/limen/compose-projects
```

`compose_update` runs `docker compose pull` and `up --detach` with the project's own directory and files, as Compose
recorded them on its containers, so the project must have been started once by hand. It never removes orphans or
volumes. It ignores `HUP` and `PIPE` and writes to `/var/log/limen-compose-update.log`, so a dropped connection
doesn't leave a project half recreated.
