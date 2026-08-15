#!/usr/bin/env bash
# Names the files a change has to travel to, the moment it is made.
#
# Advisory by design: it exits 0 and never blocks a tool call.
set -euo pipefail

input=$(cat)
path=$(printf '%s' "$input" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("tool_input", {}).get("file_path", ""))' 2>/dev/null || true)

[ -z "$path" ] && exit 0

case "$path" in
  */dynamic-config-*/src/watch.rs|*/dynamic-config-*/src/lib.rs)
    store=$(printf '%s' "$path" | sed -E 's|.*/dynamic-config-([a-z0-9-]+)/src/.*|\1|')
    cat <<NOTE
A store crate moved. What has to follow, and what a review has caught here
before:
  · book/src/remote-stores/${store}.md   the chapter, and its failure table
  · dynamic-config-${store}/CHANGELOG.md under Unreleased
  · the watch loop's reporting rules, if a failure branch changed
NOTE
    ;;
  */dynamic-config-server/src/*)
    cat <<'NOTE'
The server moved. Check book/src/config-server.md and its threat model,
and the crate's CHANGELOG under Unreleased.
NOTE
    ;;
esac

exit 0
