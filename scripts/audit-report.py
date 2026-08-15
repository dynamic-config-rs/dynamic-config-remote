"""What an OSV scan found, and which half of it is this repository's to fix.

Two kinds of finding come back, and they are not the same event:

- **A fix exists.** Somebody published a version without the problem, and
  the answer is a dependency bump. That fails the job.
- **There is no fix.** An unmaintained crate, or an advisory published
  before a patch. Failing on those makes the repository red for a fact
  about somebody else's release schedule, and a gate that goes red for
  something nobody here can do turns into a gate people learn to ignore.
  Those are a warning and a line in the job summary.

Exemptions are `osv-scanner.toml`'s, applied by the scanner before this
sees anything: an advisory answered there never reaches this script.

    osv-scanner ... --format json --output osv.json
    python scripts/audit-report.py osv.json
"""

import json
import sys
from pathlib import Path


def fixes_for(vulnerability: dict, package: str) -> list[str]:
    """Every version this advisory names as fixed, for this package."""
    fixed = []

    for affected in vulnerability.get("affected", []):
        if affected.get("package", {}).get("name") != package:
            continue

        for span in affected.get("ranges", []):
            fixed.extend(
                event["fixed"] for event in span.get("events", []) if "fixed" in event
            )

    return fixed


def main() -> int:
    report = json.loads(Path(sys.argv[1]).read_text())
    fixable: list[str] = []
    unfixable: list[str] = []

    for result in report.get("results", []):
        source = result.get("source", {}).get("path", "?")

        for entry in result.get("packages", []):
            package = entry["package"]
            name, version = package["name"], package.get("version", "?")
            ecosystem = package.get("ecosystem", "?")

            for vulnerability in entry.get("vulnerabilities", []):
                identifier = vulnerability["id"]
                summary = vulnerability.get("summary", "").splitlines()
                headline = summary[0] if summary else "no summary"
                fixed = fixes_for(vulnerability, name)
                where = f"{ecosystem} {name} {version} ({Path(source).name})"

                if fixed:
                    fixable.append(f"{where}: {identifier} — fixed in {', '.join(fixed)} — {headline}")
                else:
                    unfixable.append(f"{where}: {identifier} — no fix published — {headline}")

    summary_file = Path(sys.argv[2]) if len(sys.argv) > 2 else None
    lines = ["## Third-party advisories", ""]

    if not fixable and not unfixable:
        lines.append("Nothing, in any ecosystem.")
    else:
        for title, found in (("Fixable — a bump answers these", fixable),
                             ("No fix published — watched, not blocking", unfixable)):
            if found:
                lines += [f"### {title}", ""] + [f"- {line}" for line in found] + [""]

    text = "\n".join(lines)
    print(text)

    if summary_file is not None:
        with summary_file.open("a", encoding="utf-8") as handle:
            handle.write(text + "\n")

    for line in unfixable:
        print(f"::warning::{line}")

    for line in fixable:
        print(f"::error::{line}")

    if fixable:
        print(
            f"\n{len(fixable)} advisor{'y' if len(fixable) == 1 else 'ies'} with a "
            "fix available. Bump the dependency, or — if the fix is out of reach "
            "because something upstream pins it — add an entry to "
            "`osv-scanner.toml` saying so and what would expire it."
        )

        return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
