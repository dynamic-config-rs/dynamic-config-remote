# AGENTS.md

Instructions for coding agents working in this repository. Humans want
[CONTRIBUTING.md](CONTRIBUTING.md); this file is the same ground rules with the
things an agent gets wrong made explicit.

## Orientation

Ten crates in one workspace, one version, published together:

```text
dynamic-config-macros      the proc macro; no stable API of its own
dynamic-config             everything with behaviour — loading, layers, storage, watching
dynamic-config-etcd        \
dynamic-config-consul       |
dynamic-config-nats         |  one remote store each, behind a `RemoteSource`
dynamic-config-redis        |  or `AsyncRemoteSource` implementation
dynamic-config-vault        |
dynamic-config-s3           |
dynamic-config-firestore   /
dynamic-config-embedded    a separate `no_std` crate, sharing no code
```

Read [README.md](README.md) before changing anything: it is the specification,
not a summary. [Not planned](README.md#not-planned) lists what is deliberately
*not* here and why; [ROADMAP.md](ROADMAP.md) lists what might still be. Check
both before building something that was already decided.

## Commands

```sh
just check        # fmt, clippy at both extremes, tests, docs, the no_std build
just containers   # the seven remote stores, against real servers; needs Docker
just embedded     # the no_std crate, on a host and for thumbv7em-none-eabihf
just msrv         # every MSRV floor, against real toolchains
just mocks        # the store crates' scripted-server tests; no Docker, seconds
just hack         # every pairwise feature combination compiles
just bless        # regenerate compile-fail expectations after an intended change
```

There are skills in `.claude/skills/` for the four tasks that recur:
[adding a remote store](.claude/skills/add-remote-store/SKILL.md),
[adding a macro argument](.claude/skills/add-macro-argument/SKILL.md),
[adding a Cargo feature](.claude/skills/add-cargo-feature/SKILL.md), and
[reviewing before a release](.claude/skills/review-for-release/SKILL.md). Read
the relevant one before starting — each records decisions that are settled, so
you do not spend the turn re-deriving them.

Never claim a change works without running `just check`. If Docker is
unavailable, say so rather than skipping `just containers` silently.

## Rules that are not negotiable

**Reading configuration is lock-free.** `current()` is an atomic load. Anything
that puts a mutex, an allocation or a parse on that path is wrong regardless of
how convenient it is.

**Secrets are paths and types, never values.** Diffs, `check()` reports,
unknown-key suggestions and *error messages* all report which key moved and what
type was expected — never what was there. `dynamic-config/tests/security.rs`
enforces this. A change that puts a value into a diagnostic is a security
regression even if every test still passes.

**figment does not appear in a public signature** unless the `figment` feature
is on. That feature exists precisely so the coupling is opt-in; do not widen it.

**`dynamic-config-embedded` shares no code with the rest**, and that is
deliberate: figment is `std`, so there is nothing to share. Do not try to unify
them. It keeps the *shape* — a snapshot in a `static`, a bad document leaving
the previous one serving, `changes()` — and nothing else.

**No mandatory dependency** beyond `figment`, `serde` and `arc-swap`. Everything
else is a feature or a companion crate.

**`#![forbid(unsafe_code)]`** in every crate, checked by CI.

**MSRV is measured, not declared.** The core floor is 1.71. A feature that
raises it says so in the README table *and* gets a row in the CI matrix — `age`
declares 1.74 and actually needs 1.85, which is the kind of thing only a real
toolchain finds.

## Mistakes this repository has actually seen

These are not hypothetical. Each one shipped, got caught, and cost a debugging
session:

**Tests that share state.** The macro takes *literal* paths, and layers,
aliases and bindings live in `static`s. Two tests using the same config type,
the same fixture path or the same environment variable will race — and pass
alone, which is worse. **One type, one fixture, one variable per test.** Use a
`macro_rules!` to declare them if that gets repetitive.

**Silent string replacement.** When editing files programmatically, assert the
anchor exists. A `replace` that matches nothing looks exactly like a successful
edit until something further downstream fails for an unrelated-looking reason.

**Believing a manifest.** `age` says 1.74 and needs 1.85. etcd's client claims
to connect and connects lazily. Measure, then write the number down.

**Cleanup that destroys the thing being protected.** `save_new` deleted the file
it had just refused to overwrite. Before removing anything on an error path,
ask whether this call is what created it.

**Assuming an executor's ordering.** Two tasks spawned together are polled in
whatever order the executor likes. Yield explicitly instead.

**Turning default features off without reading what they were.** An SDK's
defaults often include its HTTP client; removing them produces "no HTTP client
was available" at runtime rather than a compile error.

**Trusting a container registry.** A Docker Hub 429 looks exactly like a broken
test. Pre-pull in CI, and prefer a registry without anonymous limits.

**Deriving `Debug` over anything that can hold a credential or a fetched
document.** A derive prints every field; three store crates shipped 0.0.1
printing Vault/Consul/GCP tokens on `{:?}`. Hand-write `Debug` for any type
whose fields can carry a secret (redact the secret, keep the fields a
debugger needs), and add a planted-token test asserting `{:?}` excludes it.

**Stacking `#[cfg]` attributes.** Two `#[cfg]`s on one item AND together:
`#[cfg(unix)] #[cfg(not(unix))]` is unsatisfiable and compiles to *nothing*,
silently. Three tests in `write.rs` never ran for months because of exactly
that pair. One `cfg` per item; combine conditions with `all()`/`any()`.

**Emitting `#[cfg(feature = ...)]` from the proc macro.** A `cfg` in generated
code is evaluated against the *user's* crate features, where the feature does
not exist — the gated method silently vanishes for every user. Route it
through a `#[macro_export] #[doc(hidden)]` redirect macro defined in the
facade crate, where the `cfg` means what it says (see
`__save_encrypted_method!` in `dynamic-config/src/redirects.rs`, and the
add-cargo-feature skill).

## What a change must carry

- **A test that would fail without it** — not one that merely exercises the code.
- **The reasoning, where it is not obvious.** Comments here explain *why*; the
  code says what. If you chose between two reasonable designs, the rejected one
  belongs in a comment or in the roadmap.
- **Documentation** if a user would notice: a new macro argument goes in the
  book's attribute reference (`book/src/attribute-reference.md`) with a section
  of its own; a new feature goes in the feature tables (lib.rs front page and
  the book), and in the MSRV table if it moves the floor. A new generated
  method that skips the book fails `tests/doc_surface.rs`.
- **A `CHANGELOG.md` entry** under `Unreleased` — the workspace one, and the
  companion crate's own if that is what changed.

## Where things live

| Looking for | Go to |
|---|---|
| what the crate does, and why each decision was made | `book/src/` — the book is the specification; `README.md` is the storefront |
| what is deliberately absent, and what would reopen it | `book/src/limitations.md` |
| what might still be built | `ROADMAP.md` |
| how a contributor gets started, and what every module does | `docs/CONTRIBUTOR-ONBOARDING.md` |
| the properties that must hold, and what enforces them | `SECURITY.md` |
| loading, merging, precedence | `dynamic-config/src/loader/` |
| what the attribute expands to | `dynamic-config-macros/src/expand/` |
| storage and reload hooks | `dynamic-config/src/cell.rs` |

## Style

`rustfmt` decides layout and `clippy -D warnings` decides the rest, at both
feature extremes. Beyond that: name things after what they mean to a caller.
Comments carry decisions, not mechanics — `// increments the counter` above
`counter += 1` is noise; `// bumped before the wake, so a waiter that polls
immediately sees the new generation` is not.

Prose in documentation is for a reader who is deciding whether to trust the
crate. State what it does *and* what it deliberately does not.

## Releasing

Do not publish. `cargo release` prepares and CI publishes on the tag; see
[RELEASING.md](RELEASING.md). Never run `cargo publish` directly.
