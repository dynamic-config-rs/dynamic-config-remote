# Contributing

New here? [docs/CONTRIBUTOR-ONBOARDING.md](https://github.com/dynamic-config-rs/dynamic-config/blob/main/docs/CONTRIBUTOR-ONBOARDING.md) is a
tour of every crate and module — what each does, why it is shaped that way, and
where you would touch it. This file is the short version.

## Branches

Pull requests target **`dev`**. `main` is the default branch — the one
visitors land on — and it is production: nothing lands there except `dev`
promotions that passed every gate (squash-merged, one commit per
promotion), and releases are tags on it.

## Before code

For anything larger than a fix, open an issue first. Not for permission — to
find out whether the thing has already been decided against, and why.
[Not planned](https://dynamic-config-rs.github.io/limitations.html#not-planned) records what was refused and why, and
[ROADMAP.md](https://github.com/dynamic-config-rs/dynamic-config/blob/main/ROADMAP.md) what might still be built. Both are shorter than a
list of what exists, and more useful.

## Running everything

```sh
just check              # fmt, clippy, tests, docs — no Docker needed
just containers         # every store against a real server
just chaos              # the watch loops with the store unplugged
```

`scripts/` holds the flows around the checks; see
[scripts/README.md](scripts/README.md).

**The seven container-backed crates are excluded from `just check`** and
run by `just containers`, which needs a Docker daemon. They fail rather
than skipping when there is none: a test that quietly stops running is one
nobody notices has stopped. `just chaos` is `#[ignore]`d on top of that —
it is the only suite you have to ask for by name.

`just chaos` is the one that catches what a healthy container cannot:
toxiproxy in front of a store that never restarts, so the port stays put
while the connection does not.

**Tests run on stable.** The MSRV toolchains are *check-only*:
dev-dependencies track stable, and `just msrv` does exactly what CI does —
`cargo check` per floor.

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
- **No mandatory dependency** beyond `serde`, `arc-swap` and the engine's
  default resolution backend.

## Style

`rustfmt` decides layout; `clippy` with `-D warnings` decides the rest. Beyond
that: name things after what they mean to a caller, and let comments carry the
decisions rather than the mechanics.
