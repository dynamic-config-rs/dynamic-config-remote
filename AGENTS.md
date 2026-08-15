# AGENTS.md

Ten crates: eight remote stores, the internals they share, and the server
that hands a section to a program which can reach none of them. The engine
they all name is [a separate
repository](https://github.com/dynamic-config-rs/dynamic-config).

## Orientation

```text
dynamic-config-store-core  credentials, TLS, retry, URL redaction, the watch
                           panic net. No stable API — machinery, not a door
dynamic-config-etcd        \
dynamic-config-consul       |
dynamic-config-nats         |  one store each, behind a `RemoteSource` or
dynamic-config-redis        |  `AsyncRemoteSource` implementation
dynamic-config-vault        |
dynamic-config-s3           |
dynamic-config-firestore    |
dynamic-config-git         /   (git: shallow single-ref fetch, any host)
dynamic-config-server      serves configuration over HTTP; a security boundary,
                           so it starts from a threat model rather than a router
```

One version across all ten, moved by `cargo release`. The engine is named
with a **caret** (`dynamic-config = "0.6"`): an engine patch release
reaches these crates with no release here, and a breaking one is picked up
deliberately, with a changelog entry saying so.

## Commands

```sh
just check        # fmt, clippy, docs, and every test that needs no daemon
just containers   # every store against a real server; needs Docker
just chaos        # the watch loops with the store unplugged; needs Docker
just msrv         # every floor, against real toolchains
just examples     # every example builds, including the server's TLS one
just book         # this repository's book
```

Skills in `.claude/skills/`: [adding a
store](.claude/skills/add-remote-store/SKILL.md), [adding a Cargo
feature](.claude/skills/add-cargo-feature/SKILL.md), [triaging the security
tab](.claude/skills/triage-security/SKILL.md), [reviewing before a
release](.claude/skills/review-for-release/SKILL.md).

Never claim a change works without running `just check`. If Docker is
unavailable, say so rather than skipping `just containers` silently.

## Rules that are not negotiable

**Secrets are paths and types, never values.** A store's `describe()`, its
error messages and every diagnostic report *which* key and *what* was
expected — never what was there. A URL with a password in it is redacted by
`store-core`, once, so the rule cannot be re-implemented differently in
eight places.

**A watch reports what it knows, not what it assumes.**
`RemoteStatus::reachable()` means *whether the store answered the last time
it was asked*. A refusal that never reached the store — an unwatchable key
shape, a missing format — reports nowhere: `Some(false)` there is a status
saying something untrue about a store that may be perfectly healthy. Every
failure branch of every watch loop is a row in that crate's own chapter.

**The contract is the same across all eight**, and each departure would be
a surprise: the current value is not delivered at startup, a deleted key is
not a change, transport failures retry, a panicking callback ends the watch
with an error rather than taking the thread down.

**Nothing here parses configuration.** A store answers with text and a
format. Layering, validation, the cache and `explain` are the engine's, and
a store that starts interpreting documents is a store that will disagree
with the other seven.

## Mistakes this repository has actually seen

**Believing a manifest.** `age` says 1.74 and needs 1.85; etcd's client
claims to connect and connects lazily. Measure, then write the number down.

**A watch table that drifted from its loop.** Two chapters said seven store
crates when git made it eight, and a failure branch was documented in prose
that the code had stopped taking. The tables in `book/src/remote-stores/`
are part of the change, not a follow-up.

**Tests that share a container.** The chaos suites put toxiproxy in front
of a store that never restarts — a restarted container gets a new host
port, and the test then proves nothing about the loop it was written for.

## What a change must carry

A store change reaches: the crate's `CHANGELOG.md` under `Unreleased`, its
chapter in `book/src/remote-stores/`, its failure table if a branch moved,
and a test — a mock one at least, a container one when the behaviour only
exists against a real server.

## Releasing

Do not publish. `cargo release` prepares; merging the bump into `main` is
what publishes. See [RELEASING.md](RELEASING.md), and never run
`cargo publish` directly.
