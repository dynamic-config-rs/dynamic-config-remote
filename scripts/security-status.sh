#!/usr/bin/env bash
# The whole security surface, in one read-only report:
#
#   Dependabot alerts (GHSA feed)  → open ones, with who pulls the package
#   Code scanning (Scorecard SARIF) → open rules, first line of each finding
#   cargo-deny advisories (RustSec) → the local gate's view
#
# Three feeds with different coverage — passing any one of them does not
# close the others, which is why this prints all three. The standing rule
# they answer to is in SECURITY.md: every open alert is triaged before a
# release ships — the lockfile moves, or the alert is dismissed with a
# written reason.
#
# Read-only: nothing here dismisses, updates or pushes. Exit code is the
# number of open alerts across both tabs (0 = the tab is clean), so it can
# gate a script without deciding anything by itself.
set -euo pipefail
cd "$(dirname "$0")/.."

repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)
open=0

bold() { printf '\n\033[1m── %s\033[0m\n' "$*"; }

# Both tabs are fetched up front, loudly: a `while` loop fed from a process
# substitution discards the API call's exit status, and an expired token or
# an outage would then read as "no alerts" — a false green in the one
# script whose job is to gate on this. A fetch that fails ends the report.
dependabot=$(gh api "repos/$repo/dependabot/alerts?per_page=100" --paginate \
  -q '.[] | select(.state != "fixed") | [.number, .state, .security_advisory.severity, .dependency.package.name, .security_vulnerability.vulnerable_version_range, .security_vulnerability.first_patched_version.identifier // ""] | @tsv') || {
  echo "could not read the Dependabot alerts — refusing to report a false green" >&2
  exit 97
}
scanning=$(gh api "repos/$repo/code-scanning/alerts?state=open&per_page=100" --paginate \
  -q '.[] | [.number, .rule.security_severity_level // "-", .rule.id, (.most_recent_instance.message.text | split("\n")[0])] | @tsv') || {
  echo "could not read the code-scanning alerts — refusing to report a false green" >&2
  exit 97
}

bold "Dependabot alerts ($repo)"
while IFS=$'\t' read -r number state severity package range fixed; do
  [ -z "${number:-}" ] && continue
  if [ "$state" = "open" ]; then
    open=$((open + 1))
    marker="OPEN"
  else
    marker="$state"
  fi

  current=$(grep -A1 "^name = \"$package\"" Cargo.lock | sed -n 's/^version = "\(.*\)"/\1/p' | head -1 || true)
  printf '  #%-3s %-9s %-8s %-14s lock=%-10s vulnerable %-18s fixed in %s\n' \
    "$number" "$marker" "$severity" "$package" "${current:-?}" "$range" "${fixed:-n/a}"

  # Who pulls it decides how it can move: a dev-dependency never reaches a
  # user, and a transitive pin can be blocked by its parent's requirement.
  if [ "$state" = "open" ]; then
    cargo tree -i "$package" 2>/dev/null | sed -n '2,3p' | sed 's/^/       /' || true
  fi
done <<< "$dependabot"

bold "Code scanning ($repo)"
while IFS=$'\t' read -r number severity rule message; do
  [ -z "${number:-}" ] && continue
  open=$((open + 1))
  printf '  #%-3s OPEN      %-8s %-20s %s\n' "$number" "${severity:--}" "$rule" "$message"
done <<< "$scanning"

bold "cargo-deny advisories (RustSec, local)"
# `unsound` advisories warn rather than fail by default, which is exactly
# how a finding can sit in the other two feeds while this one stays green —
# the accepted ones live in deny.toml's ignore list, each with its reason.
# A failing deny joins the count instead of aborting the report under
# `set -e`: the run where this gate is red is the run the summary is for.
if deny_output=$(cargo deny check advisories 2>&1); then
  printf '%s\n' "$deny_output" | tail -3 | sed 's/^/  /'
else
  printf '%s\n' "$deny_output" | tail -8 | sed 's/^/  /'
  open=$((open + 1))
fi

bold "$open open alert(s) across both tabs"
exit "$open"
