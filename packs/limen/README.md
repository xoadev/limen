# limen

limen itself, on the machine that runs it.

| Script | Answers | Changes the machine |
|---|---|---|
| `limen_update` | Installs the latest release when it is newer than the installed one; otherwise says so | **yes** |

- It takes no argument, so it can only go to the latest release: whoever leads the model to call it can't pick an
  older version. It never downgrades.
- It runs the release's own `install.sh`, which downloads the binary, checks it against the release's `SHA256SUMS`
  and runs the idempotent `limen install`: the binary is replaced atomically, and the hub's key and `limen.toml`
  stay as they are. Each request starts the binary anew, so nothing needs restarting.
- The hub is not updated from here: a hub in a container changes with its image.
- Writes the installer's output to `/var/log/limen-update.log`, and shows its end only if it failed.

Needs `curl`, `awk` and what `install.sh` needs (`sha256sum`).
