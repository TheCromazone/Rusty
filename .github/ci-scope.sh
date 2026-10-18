#!/usr/bin/env bash
# Unknown files and failed diffs require the full gate. Never skip a required job.
set -euo pipefail
base=${1:?base revision required}
head=${2:-HEAD}
if ! paths=$(git diff --name-only "$base" "$head"); then
  echo runtime=true
  exit 0
fi
while IFS= read -r path; do
  case "$path" in
    ''|README.md|CONTRIBUTING.md|AGENTS.md|docs/*.md|docs/demo/*.gif|LICENSE|THIRD_PARTY_NOTICES.txt|.gitignore) ;;
    *) echo runtime=true; exit 0 ;;
  esac
done <<< "$paths"
echo runtime=false
