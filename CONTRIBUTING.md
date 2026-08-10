# Contributing

New here? [docs/CONTRIBUTOR-ONBOARDING.md](docs/CONTRIBUTOR-ONBOARDING.md) is a
tour of every crate and module — what each does, why it is shaped that way, and
where you would touch it. This file is the short version.

## Branches

Pull requests target **`dev`** (the default branch). `main` is production:
nothing lands there except `dev` promotions that passed every gate, and
releases are tags on it.

## Before code

For anything larger than a fix, open an issue first. Not for permission — to
find out whether the thing has already been decided against, and why.
[Not planned](README.md#not-planned) records what was refused and why, and
[ROADMAP.md](ROADMAP.md) what might still be built. Both are shorter than a
list of what exists, and more useful.

## Running everything

```sh
just check              # what CI runs, in the order it fails fastest
./scripts/ci-local.sh   # the same, plus containers and MSRV — the whole gate
```

`scripts/` holds the flows around the checks — watching CI, promoting `dev`
to `main`, watching a release. Each says what it does; see
[scripts/README.md](scripts/README.md).

**Tests run on stable.** The MSRV toolchains (1.71 and friends) are
*check-only*: dev-dependencies track stable, and `cargo test` on an MSRV
toolchain fails inside the dev-dependency tree — that is expected, not a
regression. `just msrv` does exactly what CI does: `cargo check` per floor.

Or by hand:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features
just test           # the workspace suite, minus the container-backed crates
cargo test -p dynamic-config --no-default-features --features json   # feature-off diagnostics
RUSTDOCFLAGS="--cfg docsrs" cargo +nightly doc --workspace --all-features --no-deps
```

The `no_std` crate has its own features and its own recipe:

```sh
just embedded       # host tests + a thumbv7em build with no std at all
```

The seven store crates' tests drive real servers in containers and need a
working Docker daemon. They fail rather than skipping when there is none: a
test that quietly stops running is one nobody notices has stopped.

```sh
just containers     # all seven; or one at a time:
cargo test -p dynamic-config-vault    # and -consul, -etcd, -nats, -redis, -s3, -firestore
```

Their scripted-server tests need no Docker and run in seconds:

```sh
just mocks
```

## What a change should carry

**A test that would fail without it.** Not a test that exercises the new code —
one that catches the bug coming back.

**The reasoning, where it is not obvious.** Comments here explain *why*, not
what: the code says what. If you chose between two reasonable designs, the
rejected one belongs in a comment or in the roadmap.

**Documentation, if a user would notice.** A new macro argument goes in the
README's attribute table with a section of its own; a new feature goes in the
feature table and, if it moves the floor, the MSRV table.

**A changelog entry**, under `Unreleased`.

## Things that are load-bearing

Changing any of these is fine — arguing for it is the price:

- **Reading is lock-free.** `current()` is an atomic load and nothing more.
- **`figment` is the loader and does not appear in a signature.** A figment
  major bump should not be a breaking change here.
- **Secrets are paths, never values.** Every diagnostic reports which key moved,
  not what it moved to.
- **The core crate's MSRV is 1.71**, and every feature that raises it says so in
  the README table. Features that raise it are verified against real toolchains
  in CI, not trusted from a manifest — `age` declares 1.74 and needs 1.85.
- **No mandatory dependency** beyond `figment`, `serde` and `arc-swap`.

## Style

`rustfmt` decides layout; `clippy` with `-D warnings` decides the rest. Beyond
that: name things after what they mean to a caller, and let comments carry the
decisions rather than the mechanics.
