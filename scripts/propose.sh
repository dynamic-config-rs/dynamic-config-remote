#!/usr/bin/env bash
# The first half of promote.sh, deliberately without the merge:
#
#   push dev → ensure the pull request exists → stop
#
# For when something should read the pull request before the gates decide —
# an `@claude review` comment, a colleague, your own second look. Nothing is
# armed: the PR sits open until you either merge it yourself or run
# ./scripts/promote.sh, which picks up from exactly here (both scripts are
# no-ops for what is already done).
set -euo pipefail
cd "$(dirname "$0")/.."

current=$(git rev-parse --abbrev-ref HEAD)
if [ "$current" != "dev" ]; then
  echo "propose runs from 'dev' (you are on '$current')"
  exit 1
fi

if [ -n "$(git status --porcelain)" ]; then
  echo "the working tree is not clean — commit or stash before proposing:"
  git status --short
  exit 1
fi

echo "── pushing dev"
git push -u origin dev

echo "── ensuring the pull request exists"
# The title carries the version when this push is a release: "release 0.3.0"
# scans better in the PR list than a row of "promote dev to main", and the
# squash-merge reuses it as main's commit subject. One rule, one copy:
. "$(dirname "$0")/promotion-title.sh"
promotion_title

pr=$(gh pr list --base main --head dev --state open --json number -q '.[0].number')
if [ -z "$pr" ]; then
  gh pr create --base main --head dev \
    --title "$title" \
    --body "Promotes \`dev\` to \`main\`. Gates decide; this description does not."
  pr=$(gh pr list --base main --head dev --state open --json number -q '.[0].number')
else
  # A bump can land after the PR opened; keep the title honest either way.
  # REST, not `gh pr edit`: the edit command's GraphQL query still asks for
  # the deprecated projectCards field and dies on the deprecation notice.
  gh api -X PATCH "repos/{owner}/{repo}/pulls/$pr" -f title="$title" >/dev/null
fi

echo "── pull request #$pr is open, nothing armed"
gh pr view "$pr" --json url -q .url
echo
echo "ask for a review on it:   gh pr comment $pr --body '@claude review this'"
echo "merge when satisfied:     ./scripts/promote.sh"
