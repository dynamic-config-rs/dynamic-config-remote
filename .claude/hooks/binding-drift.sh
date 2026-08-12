#!/usr/bin/env bash
# Names the files a change has to travel to, the moment it is made.
#
# Two surfaces in this repository drift silently. The Rust core's public
# API is mirrored by a Python facade and a stub, and neither the compiler
# nor the test suite notices when one moves without the others — the stub
# only fails under `mypy --strict`, and the API reference fails nowhere at
# all. This prints the checklist while the change is still in hand.
#
# Advisory by design: it exits 0 and never blocks a tool call.
set -euo pipefail

input=$(cat)
path=$(printf '%s' "$input" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("tool_input", {}).get("file_path", ""))' 2>/dev/null || true)

[ -z "$path" ] && exit 0

case "$path" in
  */dynamic-config-python/src/config.rs)
    cat <<'NOTE'
The compiled surface moved. A method added or changed there has to reach:
  · python/dynamic_config/__init__.py   the facade wrapper, with a docstring
  · python/dynamic_config/_core.pyi     or mypy --strict stops seeing through
  · book/src/python/reference.md        async twins share a row
  · tests/                              the behaviour, not the call
  · dynamic-config-python/CHANGELOG.md  under Unreleased
NOTE
    ;;
  */dynamic-config-python/python/dynamic_config/__init__.py)
    cat <<'NOTE'
The facade moved. Check that _core.pyi and book/src/python/reference.md
followed, and that every public definition still carries a docstring —
this package is fully documented and `help()` is its manual.
NOTE
    ;;
  */dynamic-config/src/lib.rs)
    cat <<'NOTE'
The core's front door moved. If a public item changed, the Python binding
may need to follow: dynamic-config-python/src/ wraps this crate, and
nothing in the Rust build tells you when a wrapper goes stale.
NOTE
    ;;
  */dynamic-config-python/Cargo.toml)
    cat <<'NOTE'
This crate versions independently of the workspace and is excluded from
cargo release. If you touched `version` or `[package.metadata.release]`,
check RELEASING.md still describes what the file does.
NOTE
    ;;
esac

exit 0
