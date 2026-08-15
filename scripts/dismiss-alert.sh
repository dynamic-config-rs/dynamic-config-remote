#!/usr/bin/env bash
# Dismiss a Dependabot alert, with the reason recorded on the alert itself.
#
#   scripts/dismiss-alert.sh <number> <reason> "<comment>"
#
# GitHub's UI is the other way to do this, and it does not leave the
# reasoning anywhere a reviewer will find later. The comment this sends is
# the record: why the advisory does not reach this crate.
#
#   scripts/dismiss-alert.sh 1 no_bandwidth "lru is a transitive dev-only …"
#   scripts/dismiss-alert.sh --list
#
# Reasons GitHub accepts:
#
#   fix_started       a fix is in progress
#   inaccurate        the advisory does not apply as described
#   no_bandwidth      not now, deliberately
#   not_used          the vulnerable path is never reached
#   tolerable_risk    understood and accepted
#
# `not_used` is the honest one for an advisory in a dependency this crate
# pulls in but does not call into; reach for it before `tolerable_risk`.
set -euo pipefail
cd "$(dirname "$0")/.."

repository=$(gh repo view --json nameWithOwner --jq .nameWithOwner)

if [[ ${1:-} == "--list" || $# -eq 0 ]]; then
    echo "Open Dependabot alerts on ${repository}:"
    echo

    gh api "repos/${repository}/dependabot/alerts?state=open&per_page=100" \
        --jq '.[] | "  #\(.number)  \(.security_advisory.severity | ascii_upcase)  \(.dependency.package.name)  \(.security_advisory.summary)"' \
        || echo "  (none, or the token cannot read security alerts)"

    echo
    echo "Dismiss one with: $0 <number> <reason> \"<comment>\""
    exit 0
fi

if [[ $# -lt 3 ]]; then
    echo "usage: $0 <number> <reason> \"<comment>\"" >&2
    echo "       $0 --list" >&2
    exit 2
fi

number=$1
reason=$2
comment=$3

case "${reason}" in
    fix_started | inaccurate | no_bandwidth | not_used | tolerable_risk) ;;
    *)
        echo "unknown reason: ${reason}" >&2
        echo "expected one of: fix_started inaccurate no_bandwidth not_used tolerable_risk" >&2
        exit 2
        ;;
esac

if [[ -z ${comment//[[:space:]]/} ]]; then
    echo "a dismissal without a reason written down is the thing this avoids" >&2
    exit 2
fi

echo "Alert #${number} on ${repository}:"
gh api "repos/${repository}/dependabot/alerts/${number}" \
    --jq '"  \(.dependency.package.name) — \(.security_advisory.summary)\n  severity: \(.security_advisory.severity)\n  state: \(.state)"'

echo
read -r -p "Dismiss as '${reason}'? [y/N] " answer

if [[ ${answer,,} != y ]]; then
    echo "left open."
    exit 0
fi

gh api -X PATCH "repos/${repository}/dependabot/alerts/${number}" \
    -f state=dismissed \
    -f dismissed_reason="${reason}" \
    -f dismissed_comment="${comment}" \
    --jq '"dismissed: #\(.number) \(.dependency.package.name) — \(.dismissed_reason)"'

echo
echo "The alert reopens by itself if the dependency graph changes, which is"
echo "the point: this is a decision about today's tree, not a permanent mute."
