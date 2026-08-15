# Scripts

The `justfile` runs *checks*; these run *flows* — the git/gh choreography
around the checks. Each one is safe to re-run and says what it did.

| Script | What it does |
|---|---|
| `audit-report.py` | An OSV scan's findings, split into *a fix exists* (fails the job) and *no fix published* (a warning). Exemptions are `osv-scanner.toml`'s and never reach this. |
| `ci-local.sh` | The whole CI gate locally, in the order that fails fastest. |
| `claude-review-pr.sh` | Reviews a pull request with Claude locally — title, body, diff, and read-only access to the checkout. `--post` comments the review on the PR; without it, nothing leaves the terminal. |
| `dismiss-alert.sh` | Dismisses a Dependabot alert with the reason recorded *on the alert* — GitHub's UI leaves that nowhere a reviewer finds later. `--list` shows what is open. |
| `promote.sh` | `dev` → `main`: pushes, opens the pull request if it is not already open (titled "release X.Y.Z" when the push carries a version bump), arms auto-merge, waits for the gates, merges (squash), and re-syncs `dev` onto the new `main`. |
| `promotion-title.sh` | Sourced by the two scripts above — the one copy of the rule that titles a promotion. Not run by hand. |
| `propose.sh` | The first half of `promote.sh`: pushes `dev` and opens the pull request, then stops — for when something should read the PR before anything merges. |
| `rotate-root-changelog.sh` | Rotates the root `CHANGELOG.md` for a release — dated heading, compare link, the version's own reference link. Called by cargo-release's pre-release hook; idempotent. Not run by hand. |
| `security-status.sh` | The whole security surface, read-only: open Dependabot alerts, open code-scanning findings, and cargo-deny's local view. Exits with the open-alert count. |
| `sync-readme-versions.sh` | Rewrites every README's install snippet to the version being released — assignment shapes only, never prose. Called by the pre-release hook; idempotent. Not run by hand. |
| `watch-ci.sh` | Watches the newest CI run for the current branch; on failure, prints the failed jobs' logs. |
| `watch-release.sh` | Watches the Release run the latest merge to `main` set off, and says how to recover from a rate limit. |

The release itself is a pull request: `cargo release patch --execute` bumps
the ten crates, rewrites their changelogs and commits — nothing else. Land
it on `dev`, then `./scripts/promote.sh`. The merge to `main` is what
publishes, in two waves; CI mints the tag afterwards.
