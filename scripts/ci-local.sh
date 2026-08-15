#!/usr/bin/env bash
# The whole CI gate, locally, in the order that fails fastest.
#
#   ./scripts/ci-local.sh           everything, containers and MSRV included
#   ./scripts/ci-local.sh --quick   the fast subset — what to run before a push
#
# Mirrors ci.yml deliberately: a gate that passes here and fails there is a
# bug in one of the two, and worth a look either way.
set -euo pipefail
cd "$(dirname "$0")/.."

quick=false
[ "${1:-}" = "--quick" ] && quick=true

step() { printf '\n\033[1m── %s\033[0m\n' "$*"; }

step "fmt"
cargo fmt --all -- --check

step "clippy, both feature extremes"
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy --workspace --all-targets --no-default-features -- -D warnings

step "workflows lint clean"
if command -v actionlint >/dev/null; then
  actionlint .github/workflows/*.yml
else
  echo "actionlint not installed — CI still runs it (cargo install actionlint, or your package manager)"
fi

step "documentation links and anchors resolve"
# CI runs lychee, which also checks the web. This is the half that breaks
# most often and needs nothing installed: relative links between our own
# files, and the heading each `#fragment` claims to point at. A renamed
# heading is the failure this catches — twice now, both times after the
# push rather than before it.
python3 - <<'LINKS'
import re
import sys
from pathlib import Path


def slug(title):
    """mdbook's rule: lowercase, drop punctuation, spaces to dashes."""
    text = re.sub(r"[`*\[\]()<>.,:;!?\"'/\\]", "", title.lower())

    return re.sub(r"[^a-z0-9_-]", "", re.sub(r"\s+", "-", text.strip()))


pages = [
    path
    for path in Path(".").rglob("*.md")
    if "book/book" not in str(path) and "target" not in str(path)
]
anchors = {
    path.resolve(): {
        slug(match.group(1))
        for match in re.finditer(r"^#{1,6}\s+(.*?)\s*$", path.read_text(errors="replace"), re.M)
    }
    for path in pages
}
broken = []

for page in pages:
    for match in re.finditer(r"\[[^\]]*\]\(([^)\s]+)\)", page.read_text(errors="replace")):
        link = match.group(1)

        if link.startswith(("http", "mailto:")):
            continue

        path, _, fragment = link.partition("#")
        target = (page.parent / path).resolve() if path else page.resolve()

        # A crate README is a symlink to the root one, so its relative
        # links resolve from there rather than from the crate directory.
        if not target.exists():
            if "README.md" not in str(page):
                broken.append(f"{page}: {link}  (no such file)")

            continue

        if fragment and target in anchors and fragment not in anchors[target]:
            broken.append(f"{page}: {link}  (no such heading)")

if broken:
    print("\n".join(broken))
    sys.exit(1)

print(f"{len(pages)} pages, every relative link and anchor resolves")
LINKS

step "the workspace suite (container crates excluded)"
just test

step "the loom models, every interleaving"
just loom

step "the shuttle models, the residue loom cannot reach"
just shuttle

step "the scripted-server mocks"
just mocks

step "embedded: host tests + a target with no std"
just embedded

step "every example builds"
just examples

step "advisories, licences, sources, bans"
cargo deny check

if $quick; then
  step "quick gate done — containers, MSRV, serial rerun and docs were skipped"
  exit 0
fi

step "docs, the way docs.rs builds them"
just docs

step "the suite again, serialised (shared-state races pass alone)"
cargo test --workspace --features full \
  --exclude dynamic-config-etcd --exclude dynamic-config-consul \
  --exclude dynamic-config-nats --exclude dynamic-config-vault \
  --exclude dynamic-config-redis --exclude dynamic-config-s3 \
  --exclude dynamic-config-firestore --exclude dynamic-config-embedded \
  -- --test-threads=1

step "every MSRV floor, against real toolchains"
just msrv

if docker info >/dev/null 2>&1; then
  step "the seven stores, against real servers"
  just containers
else
  step "SKIPPED: containers — no Docker daemon. CI will still run them."
fi

step "the whole gate is green"
