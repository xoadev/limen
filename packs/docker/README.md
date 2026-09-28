# docker

Containers, their logs, and cleaning up after them.

| Script | Answers | Changes the machine |
|---|---|---|
| `containers` | Every container with its image, state and health | no |
| `container` | One container: image, state, health, restarts, mounts, ports, networks, labels | no |
| `container_logs` | One container's log over a period, stdout and stderr together | no |
| `restart_container` | Restarts one container as it is | **yes** |
| `purge` | Deletes stopped containers, unused networks, dangling images and build cache; with `images`, every unused image. Never volumes | **yes** |

Needs the `docker` CLI and daemon.

`container` never prints the environment or the command line, where secrets live, and hides the value of a label
whose name mentions a password, token or auth. `restart_container` doesn't apply compose changes: that is a
deployment, which belongs to a script of your own. Narrow its `pattern` to the containers you would let the agent
restart.
