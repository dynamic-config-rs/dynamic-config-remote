#!/usr/bin/env bash
# Watches the newest Release run on main and reports the outcome.
#
# There is nothing to *start* by hand any more: merging a version-bump PR
# into main is the release, and this only watches what that merge set off.
set -euo pipefail
cd "$(dirname "$0")/.."

run_id=$(gh run list --workflow Release --branch main --limit 1 --json databaseId -q '.[0].databaseId')

if [ -z "$run_id" ]; then
  echo "no Release run found — has anything been merged to main?"
  exit 1
fi

echo "watching Release run $run_id…"

if gh run watch "$run_id" --exit-status; then
  gh run view "$run_id" --json jobs \
    -q '.jobs[] | .name + ": " + (.conclusion // .status)'
else
  echo
  echo "── failing steps' logs ──"
  gh run view "$run_id" --log-failed
  echo
  echo "A crates.io *rate limit* just needs patience: wait out the window it"
  echo "names, then:  gh run rerun $run_id --failed   (publishing is idempotent)"
  exit 1
fi
