#!/usr/bin/env bash
# Review a pull request with Claude, locally.
#
# The @claude workflow does this on GitHub; this is the same idea without
# the round trip — for when the action is down, the answer is wanted now,
# or the review should not leave a trace on the PR. Claude gets the PR's
# title, body and diff, plus read-only access to the checkout so it can
# look at the code *around* a hunk instead of judging the hunk alone.
# Read-only is the boundary: no Edit, no Write, no arbitrary Bash — a
# review that wants to change something should say so, not do so.
#
#   ./scripts/claude-review-pr.sh            # the open dev → main PR
#   ./scripts/claude-review-pr.sh 7          # a specific PR
#   ./scripts/claude-review-pr.sh 7 --post   # ...and comment the review on it
#
# Needs: gh (authenticated), claude (the Claude Code CLI, logged in).
# The model is whatever the local CLI is configured for; override with
# CLAUDE_MODEL=claude-opus-5 ./scripts/claude-review-pr.sh
set -euo pipefail
cd "$(dirname "$0")/.."

pr="${1:-}"
post="${2:-}"

if [ -z "$pr" ]; then
  pr=$(gh pr list --base main --head dev --state open --json number -q '.[0].number')
  if [ -z "$pr" ]; then
    echo "no open dev → main pull request — name one: $0 <number> [--post]"
    exit 1
  fi
fi

echo "── gathering pull request #$pr" >&2
title=$(gh pr view "$pr" --json title -q .title)
body=$(gh pr view "$pr" --json body -q .body)
diff=$(gh pr diff "$pr")

model_args=()
if [ -n "${CLAUDE_MODEL:-}" ]; then
  model_args=(--model "$CLAUDE_MODEL")
fi

echo "── reviewing (this takes a few minutes)" >&2
# `+"..."` rather than a bare expansion: an empty array under `set -u` is an
# "unbound variable" on the bash 3.2 that macOS still ships.
review=$(claude -p \
  ${model_args[@]+"${model_args[@]}"} \
  --allowed-tools "Read,Grep,Glob,Bash(git log:*),Bash(git show:*)" \
  <<EOF
Review this pull request as a maintainer of the repository you are in.
AGENTS.md states the repository's rules; hold the diff to them.

Focus, in this order: correctness bugs the tests would not catch; secrets
or values leaking into Debug output, errors or diagnostics; breaking
changes absent from the changelogs; documentation the diff makes stale.
You have read-only access to the checkout — read the surrounding code
before judging a hunk. Do not suggest running anything; state findings.

Structure the answer as: a one-paragraph verdict, then findings ordered by
severity, each with file:line and a concrete fix. If something is fine,
do not pad it into a finding — a short review of a good diff is correct.

## ${title}

${body}

## Diff

\`\`\`diff
${diff}
\`\`\`
EOF
)

echo
echo "$review"

if [ "$post" = "--post" ]; then
  echo
  echo "── posting the review as a comment on #$pr" >&2
  gh pr comment "$pr" --body "$review"
fi
