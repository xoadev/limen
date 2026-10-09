#!/usr/bin/env bash
# Runs a draft or release workflow on main with `gh` and follows it to the end: what Actions → Run workflow does,
# from here.
#
#   tools/workflow.sh drafts                  packs-draft.yml: every pack's draft rewritten
#   tools/workflow.sh pack <pack> <publish>   packs.yml for one pack: built, and published if <publish> is 1
#   tools/workflow.sh release <publish>       release.yml: limen built, and published if <publish> is 1
#
# Unpublished is the trial: the whole workflow on the draft's commit, the files left as the run's artifacts, nothing
# public. Releases are immutable, so publishing is asked for by name (PUBLISH=1), never the default.
set -euo pipefail
cd "$(dirname "$0")/.."

command -v gh > /dev/null || { echo "workflow: needs the GitHub CLI, gh (https://cli.github.com), logged in" >&2; exit 1; }

flag() {
  case "$1" in
    1) echo true ;;
    0 | '') echo false ;;
    *) echo "workflow: PUBLISH is 1 or 0, not '$1'" >&2; exit 1 ;;
  esac
}

what=${1:-}
case "$what" in
  drafts)
    workflow=packs-draft.yml
    fields=()
    ;;
  pack)
    pack=${2:-}
    [[ "$pack" =~ ^[a-z0-9][a-z0-9_-]*$ && -d "packs/$pack" ]] || { echo "workflow: no pack '$pack' under packs/ (PACK=<pack>)" >&2; exit 1; }
    workflow=packs.yml
    fields=(-f "pack=$pack" -f "publish=$(flag "${3:-}")")
    ;;
  release)
    workflow=release.yml
    fields=(-f "publish=$(flag "${2:-}")")
    ;;
  *)
    echo "workflow: drafts, pack <pack> <publish> or release <publish>, not '$what'" >&2
    exit 1
    ;;
esac

# The run this dispatch starts is the newest one by this event created after it: gh doesn't say which it started.
since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
gh workflow run "$workflow" --ref main "${fields[@]}"
echo "workflow: $workflow started on main ${fields[*]}"
run=
for _ in $(seq 30); do
  run=$(gh run list --workflow "$workflow" --event workflow_dispatch --limit 5 --json databaseId,createdAt \
    --jq "[.[] | select(.createdAt >= \"$since\")] | last | .databaseId // empty")
  [[ -n "$run" ]] && break
  sleep 2
done
[[ -n "$run" ]] || { echo "workflow: the run of $workflow did not show up; look in Actions" >&2; exit 1; }
gh run watch "$run" --exit-status
