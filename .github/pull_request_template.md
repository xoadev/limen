## What changes

<!-- What and why. Link the issue if there is one. -->

## Checklist

- [ ] The title follows Conventional Commits (`fix(gate): …`): it becomes the commit and the release note
- [ ] `docs/spec.md` says what the code now does
- [ ] The README, the packs' READMEs and `docs/` say it too, and nothing they say is now wrong
- [ ] Tests for it; a scene in `tools/e2e.sh` if it reaches a real machine (gate, install, join, SSH, sudo)
- [ ] The limits stay on the node: no shell, paths through the policy, arguments through their schema

## Verification

```
<!-- the end of `make check`, and of `make e2e` when it applies -->
```
