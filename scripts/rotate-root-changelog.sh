#!/usr/bin/env bash
# Rotates the workspace CHANGELOG.md the way cargo-release rotates each
# package's own: the `Unreleased` section gains the new version's dated
# heading, the `[Unreleased]` compare link moves forward, and the new
# version gets its own reference link.
#
# cargo-release's `pre-release-replacements` are applied per *package*, so
# the workspace file at the root is nobody's — it was hand-rotated, and
# forgotten, twice. This script is called from the pre-release hook, which
# runs once per package; the version check makes every run after the first
# a no-op, so the repetition is harmless.
set -euo pipefail
cd "$(dirname "$0")/.."

# Set by cargo-release for its hooks; running the script outside a release
# should fail loudly, not rotate to nothing.
version="${NEW_VERSION:?NEW_VERSION is set by the cargo-release hook environment}"

# `cargo release <level>` without `--execute` is a dry run, and it still
# runs the hooks — with DRY_RUN=true. A look-before-you-leap run must not
# leave the tree dirty.
if [ "${DRY_RUN:-false}" = "true" ]; then
  echo "dry run: would rotate CHANGELOG.md for $version"
  exit 0
fi

if grep -q "^## \[$version\]" CHANGELOG.md; then
  exit 0
fi

python3 - "$version" <<'PY'
import datetime
import pathlib
import re
import sys

version = sys.argv[1]
path = pathlib.Path("CHANGELOG.md")
text = path.read_text()

# The template comment spells its heading `[_Unreleased_]` precisely so
# that searches like this one match only the real heading.
heading = "## [Unreleased]"
assert text.count(heading) == 1, "expected exactly one real Unreleased heading"
today = datetime.date.today().isoformat()
text = text.replace(heading, f"{heading}\n\n## [{version}] — {today}", 1)

link = re.compile(r"\[Unreleased\]: (.*)/compare/v([0-9.]+)\.\.\.HEAD")
assert len(link.findall(text)) == 1, "expected exactly one Unreleased compare link"
text = link.sub(
    lambda m: (
        f"[Unreleased]: {m.group(1)}/compare/v{version}...HEAD\n"
        f"[{version}]: {m.group(1)}/compare/v{m.group(2)}...v{version}"
    ),
    text,
)

path.write_text(text)
PY

echo "rotated CHANGELOG.md for $version"
