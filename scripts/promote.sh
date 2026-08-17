#!/usr/bin/env bash
# dev → main, the whole choreography:
#
#   push dev → ensure the pull request exists → wait for the gates →
#   merge (squash — one commit per promotion on main) → re-sync dev onto main
#
# Safe to re-run at any point; each step is a no-op when already done.
# `main` takes no direct pushes — this is the only road, on purpose.
set -euo pipefail
cd "$(dirname "$0")/.."

current=$(git rev-parse --abbrev-ref HEAD)
if [ "$current" != "dev" ]; then
  echo "promote runs from 'dev' (you are on '$current')"
  exit 1
fi

if [ -n "$(git status --porcelain)" ]; then
  echo "the working tree is not clean — commit or stash before promoting:"
  git status --short
  exit 1
fi

echo "── pushing dev"
git push -u origin dev

echo "── ensuring the pull request exists"
# Everything below reads `main` as origin has it. `promotion-title.sh`
# fetches it too, but unguarded — on a repository whose `main` was never
# pushed, that fetch fails and `set -e` ends the run right here having
# printed nothing at all.
if ! git fetch -q origin main; then
  # git has printed why. The common one on a new repository is that `main`
  # was never pushed at all, and there is a one-liner for that:
  echo "could not read 'main' from origin. If it does not exist yet:"
  echo "  git push origin dev:main"
  exit 1
fi

# Nothing to promote is not an error, but `gh pr create` treats it as one
# — "No commits between main and dev" — and `set -e` would, again, end the
# run with no explanation. This is the state a repository is in right after
# `dev` is branched from `main` and pushed.
ahead=$(git rev-list --count origin/main..HEAD)

if [ "$ahead" -eq 0 ]; then
  echo "dev is level with main — nothing to promote."
  exit 0
fi
echo "dev is $ahead commit(s) ahead of main"

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
echo "pull request #$pr"

echo "── arming auto-merge and waiting"
# Auto-merge instead of watching checks ourselves: the same commit can carry
# check runs from a cancelled twin (the push-run the PR-run deduplicated),
# and only GitHub's own merge logic knows which one counts. Auto-merge fires
# exactly when branch protection is satisfied — required gates green,
# conversations resolved.
gh pr merge "$pr" --squash --auto

deadline=$((SECONDS + 3600))
while [ "$(gh pr view "$pr" --json state -q .state)" = "OPEN" ]; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "not merged within an hour — see what is holding it: gh pr view $pr"
    echo "(a red gate, or an unresolved conversation; auto-merge stays armed)"
    exit 1
  fi
  sleep 30
done

if [ "$(gh pr view "$pr" --json state -q .state)" != "MERGED" ]; then
  echo "the pull request closed without merging — investigate: gh pr view $pr"
  exit 1
fi

echo "── re-syncing dev onto the new main"
# A squash-merge lands one new commit on main, so dev is re-pointed at main
# rather than diverging forever; the granular commits' story is the
# changelog's to tell, which is the point of squashing. --force-with-lease
# so a push that arrived on dev meanwhile is a stop, not a casualty.
git fetch origin
git reset --hard origin/main
git push --force-with-lease origin dev

echo "── promoted. main is at $(git rev-parse --short origin/main)."
echo "if this bumped the workspace version, the merge just started a release:"
echo "  ./scripts/watch-release.sh"
