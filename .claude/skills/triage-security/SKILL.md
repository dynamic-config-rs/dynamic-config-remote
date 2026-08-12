---
name: triage-security
description: Use when GitHub's security surface needs triaging — Dependabot alerts, the code-scanning tab's Scorecard findings, or a red cargo-deny — covers the fix ladder for a vulnerable dependency, the MSRV-fallback trap, when and how to dismiss with a written reason, and what each feed does and does not see.
---

# Triaging the security tab

The standing rule lives in `SECURITY.md`: **every open alert is triaged
before a release ships.** Triaged means one of two visible outcomes — the
lockfile moves to a patched version, or the alert is dismissed with a
written reason saying why the vulnerable path is not reachable and what
would reopen the question. An alert that is neither is a release blocker.

Start with the whole picture:

```sh
./scripts/security-status.sh    # read-only; exits with the open-alert count
```

## Three feeds, three blind spots

| Feed | Source | Does not see |
|---|---|---|
| Dependabot alerts | GHSA database, against `Cargo.lock` | advisories RustSec has and GHSA lacks |
| Code scanning | OSSF Scorecard SARIF (OSV database) | anything — but most of its rules are *posture*, not vulnerabilities |
| `cargo deny check advisories` | RustSec | GHSA-only advisories; and `unsound` advisories only **warn** by default, so deny stays green while the other feeds flag them |

Passing one closes nothing on the others. This is why the lru advisories
sat in two tabs while `cargo deny` said `advisories ok`.

## Dependabot alerts: the fix ladder

For each open alert, establish three facts before touching anything:
the current locked version (`grep -A1 'name = "pkg"' Cargo.lock`), who
pulls it (`cargo tree -i pkg` — a dev-dependency never reaches a user),
and the first patched version (in the alert).

1. **`cargo update -p pkg`.** Expect `Locking 0 packages to latest Rust
   1.71 compatible versions` — that is not "already fixed", it is
   `.cargo/config.toml`'s `incompatible-rust-versions = "fallback"`
   refusing a jump whose `rust-version` exceeds the workspace floor.
2. **`cargo update -p pkg --precise <fixed>`.** The documented mechanism
   (the `time` note in `deny.toml`). A patched version needing Rust 1.88
   is fine when the package is a dev-dependency or reached only through a
   companion crate: the MSRV CI rows run `cargo check` on library targets,
   which never builds dev-dependencies, and the companion floors are 1.85
   and 1.88. The core's 1.71 rows are the only ones to think about.
3. **Blocked by a parent's requirement** (`failed to select a version for
   the requirement`, the lru-under-aws-sdk case): the fix cannot land
   until upstream moves. Then — and only then — dismiss, and write the
   acceptance down twice: a `deny.toml` ignore entry with the reason and
   the revisit condition, and the alert dismissal itself.

After the push, GitHub re-checks the lockfile and closes the fixed alerts
by itself; do not close them by hand.

**Verify a runtime bump like a change, because it is one.** A big SDK jump
(aws-sdk-s3 1.91 → 1.112) gets `cargo check --workspace --all-features`,
`cargo deny check`, and the affected store's container suite run for real.
A dev-only bump gets the workspace check.

**Changelog:** lockfile pins go under the root `CHANGELOG.md`'s
`### Security` with the honest scope — library consumers resolve their own
trees and were never pinned by these crates; the lockfile governs this
repository's CI and any `--locked` install.

## Dismissing, precisely

The two tabs use different APIs and different reason enums, and the
comment field is capped at **280 characters** — a longer one is a 422.

```sh
# Dependabot: reasons are fix_started | inaccurate | no_bandwidth | not_used | tolerable_risk
gh api -X PATCH repos/{owner}/{repo}/dependabot/alerts/N \
  -f state=dismissed -f dismissed_reason=tolerable_risk -f dismissed_comment="…"

# Code scanning: reasons are "false positive" | "won't fix" | "used in tests"
gh api -X PATCH repos/{owner}/{repo}/code-scanning/alerts/N \
  -f state=dismissed -f dismissed_reason="won't fix" -f dismissed_comment="…"
```

A dismissal comment answers two questions or it is not a triage: why the
vulnerable path is not reachable, and what would reopen the question
("revisit when the SDK moves its lru floor"). A dismissal is sticky —
re-detection does not reopen it — which is exactly why the reason has to
carry the revisit condition.

## Code scanning: Scorecard is posture, mostly

The rules on that tab are OSSF Scorecard checks, uploaded as SARIF by
`scorecard.yml`. Only one of them is about vulnerabilities:

- **`VulnerabilitiesID`** lists OSV findings against the lockfile. Check
  *when the scan ran* before reacting — `.most_recent_instance.ref` and
  the run list — because a scan from before a pin push lists things that
  are already fixed. Scorecard runs on pushes to the default branch and on
  its Monday cron, so a stale finding refreshes on the next one. Identify
  any advisory you do not recognise against OSV directly:
  `curl -s https://api.osv.dev/v1/vulns/RUSTSEC-XXXX-XXXX`. RUSTSEC and
  GHSA ids alias each other; one package can carry several advisories, so
  do not assume a familiar name means a familiar finding.
- **The rest** (`MaintainedID`, `CodeReviewID`, `FuzzingID`, `SASTID`,
  `CIIBestPracticesID`) are project-shape metrics. Fix the ones worth
  fixing (a real gap gets a ROADMAP entry — fuzzing did), dismiss the
  structural ones with the reason ("single-maintainer: review is the gate
  set plus the Claude workflow; self-approval is not possible"), and let
  the self-resolving ones resolve (repository age).

## Things that have bitten

- `cargo update` silently doing nothing *is* the MSRV fallback working;
  read the "Rust N.NN compatible" line before concluding anything.
- `cargo deny`'s green is not the tab's green: `unsound` advisories warn,
  and GHSA-only advisories never reach RustSec.
- The 280-character dismissal cap, discovered as a 422 mid-triage.
- A scorecard scan predating the fix push reads like nothing was fixed.
  Check the scanned ref first; re-triaging fixed findings wastes the pass.
- Dependabot's and code-scanning's `dismissed_reason` enums are disjoint;
  the wrong vocabulary in the right field is also a 422.
- `gh api -X PATCH` on alerts may be blocked for an agent by the
  permission layer: prepare the exact command and hand it over instead of
  working around the refusal.
- **`cargo generate-lockfile` silently reverts every `--precise` security
  pin** — the MSRV-fallback resolver re-picks the floor-compatible (and
  vulnerable) versions. Four patched pins regressed at once this way. The
  `just msrv` and `just minimal-versions` recipes now restore the
  committed lock afterwards; after any *manual* regeneration, run
  `./scripts/security-status.sh` and re-pin before committing the lock.
