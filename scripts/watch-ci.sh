#!/usr/bin/env bash
# Watches the newest CI run for the current branch and reports per job.
# On failure, prints the failed jobs' logs — the part you would have clicked
# through four pages to find.
#
#   ./scripts/watch-ci.sh             the current branch
#   ./scripts/watch-ci.sh main        a named branch
set -euo pipefail
cd "$(dirname "$0")/.."

branch="${1:-$(git rev-parse --abbrev-ref HEAD)}"

run_id=$(gh run list --branch "$branch" --workflow CI --limit 1 --json databaseId -q '.[0].databaseId')

if [ -z "$run_id" ]; then
  echo "no CI run found for branch '$branch' — did the push land?"
  exit 1
fi

echo "watching CI run $run_id on '$branch'…"

# `gh run watch` exits non-zero when the run fails; keep going so the logs
# print before this script gives its own verdict.
if gh run watch "$run_id" --exit-status; then
  echo "CI is green on '$branch'."
else
  echo
  echo "── failed jobs ──"
  gh run view "$run_id" --json jobs \
    -q '.jobs[] | select(.conclusion != "success" and .conclusion != "skipped") | .name + ": " + .conclusion'
  echo
  echo "── failing steps' logs ──"
  gh run view "$run_id" --log-failed
  exit 1
fi
