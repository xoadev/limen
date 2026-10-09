# limen

limen itself, on the machine that runs it.

Needs limen 0.1.3 or later: its headers use `read_only`, which older versions refuse.

| Script | Answers | Changes the machine |
|---|---|---|
| `limen_version` | The installed version, the latest release, and whether there is something to update | — |
| `limen_lint` | Every problem `limen lint` finds in this machine's packs: names, headers, permissions | — |
| `limen_packs` | The packs `limen.toml` lists, how many files each has, the release each one was unpacked from, and whether a newer one is published | — |
| `limen_update` | Installs the latest release when it is newer than the installed one; otherwise says so | **yes** |

- `limen_update` takes no argument, so it can only go to the latest release: whoever leads the model to call it can't pick an
  older version. It never downgrades.
- `limen_update` runs the release's own `install.sh`, which downloads the binary, checks it against the release's `SHA256SUMS`
  and runs the idempotent `limen install`: the binary is replaced atomically, and the hub's key and `limen.toml`
  stay as they are. Each request starts the binary anew, so nothing needs restarting.
- `limen_packs` knows a pack's version from its folder's name, `<pack>@X.Y.Z`, as
  [docs/scripts.md](../../docs/scripts.md#released-packs) unpacks them. It only says that a newer release exists:
  bringing it is the machine's own setup.
- The hub is not updated from here: a hub in a container changes with its image.
- `limen_update` writes the installer's output to `/var/log/limen-update.log`, and shows its end only if it failed.

Needs `curl`, `awk` and, for `limen_update`, what `install.sh` needs (`sha256sum`).
