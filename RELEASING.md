# Releasing

Ten crates. Ten publish to crates.io in two waves, versioned together.

The waves are dependency order: `dynamic-config-store-core`, which every
store names exactly, and then the eight stores and the server. A crate
cannot be `publish = false` and be depended on by a published one: cargo
resolves a path dependency of a published crate from the registry, so
packaging fails before the release does.

**The engine is not released from here.** It is named with a caret
(`dynamic-config = "0.6"`), so its patch releases arrive without a release
here at all. What does oblige one is a *breaking* engine release: `0.7`
means bumping the requirement, compiling against it, and saying so under
Changed.

```text
dynamic-config-store-core      first, always
  ├── dynamic-config-etcd
  ├── dynamic-config-consul
  ├── dynamic-config-nats        second, in any order
  ├── dynamic-config-redis
  ├── dynamic-config-vault
  ├── dynamic-config-s3
  ├── dynamic-config-firestore
  ├── dynamic-config-git
  └── dynamic-config-server
```

Every crate here names `dynamic-config-store-core` with an exact
requirement (`=x.y.z`): it is machinery these crates share rather than a
door anybody else opens, and the ten move in one release — so it goes
first, and nothing can resolve against a version that is not there yet.

**The engine is a caret** (`dynamic-config = "0.6"`), because a user
chooses it alongside these crates. An exact pin there would mean an engine
upgrade waits for a release here, which is the coupling this repository
exists to be free of.

## The branch model

Work lands on `dev`. `main` is production: it accepts no direct pushes — not
even from admins — only pull requests whose gates ("CI is green", "Security
is green") have passed, merged with a linear history (squash or rebase).

**Merging a version bump into `main` is the release.** There is no tag to
push by hand: `release.yml` runs on every push to `main`, checks whether the
workspace version is new, and — only then — verifies (including
`cargo semver-checks` against the published baseline), publishes in waves,
and mints the tag and the GitHub release itself, at the merge commit.

```text
feature work ──▶ dev ──(PR, gates green)──▶ main
                                             │
                             version unchanged: green no-op
                             version new: verify ─▶ publish ─▶ tag + release
```

## The lifecycle, step by step

Everything above, as the sequence an operator actually types. Each step is
safe to redo; nothing before step 5 leaves the laptop.

1. **Land the work on `dev`** through pull requests, entries accumulating
   under each touched crate's `## [Unreleased]` heading as they go.
2. **Pre-flight.** `just check` on `dev`, plus `just containers` and `just
   chaos` when a store or a watch loop moved, and `just msrv` when a floor
   did.
   Confirm the changelog entries sit under `## [Unreleased]` and **not**
   under a version heading you wrote yourself — `cargo release` inserts the
   dated heading, and a pre-written one becomes a duplicate it cannot see.
   Confirm the README's install snippet names the version about to exist.
3. **`cargo release minor --execute`** (or `patch`; pre-1.0 a breaking
   change is `minor`). Runs `just check` first, bumps every crate,
   rotates the changelogs, and makes one local commit — no push, no tag,
   no publish.
4. **Read the release commit.** `git show --stat HEAD`, then skim a
   changelog or two: exactly one heading per version, entries under the new
   one, `Unreleased` empty again.
5. **Optional review before the gates decide.** `./scripts/propose.sh` —
   pushes `dev` and opens the pull request without arming anything. Then
   comment `@claude review this release` on it: the Claude workflow reads
   the diff and answers on the PR. Its verdict is advisory, not a required
   check, so read it *before* the next step. The same review without the
   round trip: `./scripts/claude-review-pr.sh` runs Claude locally over the
   PR (add `--post` to leave the review as a comment). From the Claude Code
   CLI, `/code-review ultra <PR#>` is the heavier multi-agent version of
   the same idea.
6. **`./scripts/promote.sh`.** Pushes `dev`, ensures the PR exists, arms
   auto-merge and waits; when "CI is green" and "Security is green" pass,
   the squash-merge lands — **that merge is the release** — and `dev` is
   re-synced onto `main`.
7. **`./scripts/watch-release.sh`.** Follows the run the merge set off:
   verification (including `cargo semver-checks` against the published
   baseline — see [A crate's first release](#a-crates-first-release)),
   publishing in the three waves, then the tag and the GitHub
   release, minted by CI at the merge commit. The same push to `main`
   deploys the book to Pages.
8. **Afterwards.** Check docs.rs built each crate (its *own* README,
   feature badges present), and open issues for whatever the release
   deferred.

## A crate's first release

A crate that has never been on crates.io has no baseline to compare
against, and `cargo semver-checks` calls that an error rather than
nothing to do — so one unpublished member fails the whole workspace run.
0.6 introduced three (`dynamic-config-store-core`, `-git`, `-server`) and
that is exactly what happened on its first release attempt.

The verify step derives the exclusions instead of carrying a list: it
asks crates.io whether each publishable member exists, skips the ones it
has never heard of, and fails on any answer that is neither 200 nor 404 —
a rate limit must not read as "unpublished". A crate is therefore skipped
on the release that introduces it and checked on every release after,
with nobody having to remember to remove anything.

Nothing else about a first release is special: `cargo publish` claims the
name, and the publish waves already list the new crates in dependency
order.

## Releasing

`cargo release` prepares; the merge publishes. The split is deliberate: a
laptop cannot reach crates.io at all, and nothing reaches it without the
gates in front of it.

```sh
# python3 is also needed: the pre-release hook's changelog rotation uses it.
cargo install cargo-release just

# On dev (or a branch that lands there):
cargo release patch --execute     # 0.0.1 -> 0.0.2: bump + changelogs + commit
cargo release minor --execute     # 0.0.1 -> 0.1.0, which pre-1.0 is a break

./scripts/promote.sh              # PR, gates, merge — the merge releases
./scripts/watch-release.sh        # watch the run the merge set off
```

`cargo release` runs `just check`, bumps every crate, moves each
`## [Unreleased]` section under a dated version heading — the workspace
`CHANGELOG.md` included, which `scripts/rotate-root-changelog.sh` handles
from the pre-release hook because the per-package replacements never touch
it — and commits. It does **not** push or tag; that is CI's job, after
publishing succeeded. A
crates.io *rate limit* mid-publish just needs the window waited out and the
job re-run — publishing is idempotent, already-uploaded crates are skipped.

### Before you run it

1. `main` is green, including the container job and every MSRV row.
2. `CHANGELOG.md` — and each companion's — has entries under `Unreleased`,
   and no hand-written heading for the version being cut: the heading is
   `cargo release`'s to write. A release with an empty section is a release
   nobody can read.
3. The README install snippets take care of themselves: the pre-release
   hook runs `scripts/sync-readme-versions.sh`, which rewrites every
   snippet — the root's and the nine companions' — to the version being
   cut. It touches only the assignment shapes the `doc_surface` gate
   parses, never prose (the old objection to automating this was a regex
   loose enough to catch prose; the answer was to not catch prose). The
   gate still fails when the snippets disagree, as the backstop.

### If it has to be done by hand

The waves exist because each crate pins the one below it exactly, so a wave
cannot resolve until the previous one is on the registry:

```sh
cargo publish -p dynamic-config-store-core
# wait for the index, usually under a minute
cargo publish -p dynamic-config-etcd
cargo publish -p dynamic-config-consul
cargo publish -p dynamic-config-nats
cargo publish -p dynamic-config-redis
cargo publish -p dynamic-config-vault
cargo publish -p dynamic-config-s3
cargo publish -p dynamic-config-firestore
cargo publish -p dynamic-config-git
cargo publish -p dynamic-config-server
```

`--no-verify` is deliberately not used. The verification build is the last
chance to catch a package that resolves locally through a path dependency and
nowhere else.

### Afterwards

Check docs.rs built each crate with `all-features = true`, so feature-gated
items carry their badges — and that each companion rendered *its own* README
rather than the workspace one.

## Version policy

- **Pre-1.0, a breaking change bumps the minor version** and everything else the
  patch. `0.0.x` is the pre-announcement series: the API is expected to move.
- MSRV changes are breaking. Every figure in the README's MSRV table is part
  of the public contract. The store crates, the server and the bindings
  declare their own floors in their own repositories — a companion pays for
  what it pulls in.
- The engine resolves, and its behaviour is this crate's behaviour. A change
  to how values are merged or how environment strings are read is a breaking
  change here even when no signature moves — `tests/loader.rs` exists to make
  that visible rather than surprising. Which *backend* folds the layers is
  not that kind of change: every engine is held to the same merge rule, leaf
  by leaf, in the engine's own agreement tests.
- The traces a value carries are contract too: `Origin::Remote` names whatever
  the source's own `describe()` returns, so changing that string in a companion
  crate changes what users see in an error.
- `Error`, `ErrorKind` and `Origin` are `#[non_exhaustive]`, so new variants are
  additive.
