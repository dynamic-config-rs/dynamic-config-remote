# Contributing

New here? [docs/CONTRIBUTOR-ONBOARDING.md](docs/CONTRIBUTOR-ONBOARDING.md) is a
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
[Not planned](book/src/limitations.md#not-planned) records what was refused and why, and
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

Seven of the eight store crates drive real servers in containers and need a
working Docker daemon — git is the exception, and its tests build a repository
in a temporary directory like any other fixture. They fail rather than skipping when there is none: a
test that quietly stops running is one nobody notices has stopped.

```sh
just containers     # all seven; or one at a time:
cargo test -p dynamic-config-vault    # and -consul, -etcd, -nats, -redis, -s3, -firestore
```

Their scripted-server tests need no Docker and run in seconds:

```sh
just mocks
```

And the chaos suites take a store away *mid-watch* — a toxiproxy in front of
a server that never restarts, so both the cut and the recovery are
assertable. They are `#[ignore]`d, so nothing above runs them:

```sh
just chaos          # Redis, Consul, etcd — one per loop shape
```

Two containers per test and tens of seconds each, which is why they are a
nightly and release gate rather than a per-commit one. What they pin is the
pair an alert reads: `remote_up` goes to zero *while the staleness clock
keeps running*, and the last good document is still being served.

## The Node.js bindings

```sh
just node           # build the addon, run the suite, run every example
```

Node 18 or newer, and nothing else: the suite is `node --test` and the
facade is JavaScript with a hand-written `.d.ts`, so there is no build
step and no `npm install` in the loop. Three examples want a framework —
they say so and exit cleanly without one.

The type check is the exception, and it is skipped with a word rather than
failing when TypeScript is absent:

```sh
cd dynamic-config-node && npm install -D typescript && npm run typecheck
```

It is not optional in CI. A regression in the `.d.ts` is invisible to a
test suite — `config.current().host` runs perfectly well while the checker
calls it `unknown` — which is why `tests/typing/usage.ts` exists and is
compiled under `strict`, `exactOptionalPropertyTypes` and
`noUncheckedIndexedAccess`.

**What to know before changing the compiled half**: validation runs inside
the load, on a worker thread, and reaches the event loop through a
`ThreadsafeFunction`. That ordering is what makes a rejected document
change nothing, and it is why nothing here is synchronous.
`book-node/src/internals.md` is the whole argument.

## The Python bindings, without a GIL

`just python` runs the suite on whatever interpreter the venv holds. The
free-threaded build is a second one, and it needs its own venv because the
wheel is not abi3:

```sh
uv python install 3.14t
uv venv --python 3.14t /tmp/ft && VIRTUAL_ENV=/tmp/ft uv pip install maturin pytest pytest-asyncio pydantic
cd dynamic-config-python && VIRTUAL_ENV=/tmp/ft maturin develop --no-default-features
VIRTUAL_ENV=/tmp/ft /tmp/ft/bin/python -m pytest tests -q
```

`maturin develop` uses the active venv, so it picks the free-threaded
interpreter and emits a `cp314t` build. `maturin **build**` does not — without
`-i` it builds an abi3 wheel against no interpreter in particular and ignores
the venv entirely, so CI passes `-i python3.14t` and that flag is the
load-bearing one. `--no-default-features` switches off the `abi3` Cargo
feature as well; it does not change the tag, but it means pyo3 is never asked
for abi3 and the build does not lean on pyo3's backward-compatibility fallback.
Cargo features are additive, so abi3 has to be a default that is dropped rather
than an opt-in. 3.14t and not 3.13t: PyO3 0.29 dropped 3.13t, following
CPython.

Two things are worth knowing before changing anything there. PyO3 has declared
modules GIL-free *by default* since 0.28, so a module that says nothing is
already making the claim — `src/lib.rs` writes `gil_used = false` out anyway,
so the claim lives where it is made. And
`tests/test_free_threaded.py::test_the_module_declares_itself_gil_free` asserts
`sys._is_gil_enabled()` rather than watching for the interpreter's warning: the
warning fires once per process at the first import, so a test that reloads the
module and catches warnings passes either way.
[Free-Threaded CPython](book-python/src/free-threading.md) is the audit.

## Instruction counts

`benches/read_path.rs` and `benches/engine.rs` measure wall clock and are not
gates — a shared runner's variance is larger than the changes worth catching.
`benches/instructions.rs` counts instructions under callgrind, which is stable
enough to gate on. Running it needs two things the ordinary gate does not:

```sh
sudo apt-get install valgrind
cargo install iai-callgrind-runner@0.16.1   # EXACTLY the dev-dependency's version
cargo bench -p dynamic-config --features json,toml --bench instructions
```

**The runner's version must match `iai-callgrind` in `dynamic-config/Cargo.toml`
exactly.** The runner refuses a mismatch rather than reporting wrong numbers,
and forgetting it is the most common way a first setup fails. The
`instructions.yml` workflow reads the version out of the manifest for the same
reason.

Without root, valgrind builds into a prefix in about ten minutes and needs no
package manager — which is worth knowing, because "I cannot install valgrind"
is what kept this bench unrun for its first release:

```sh
curl -LO https://sourceware.org/pub/valgrind/valgrind-3.24.0.tar.bz2
tar xf valgrind-3.24.0.tar.bz2 && cd valgrind-3.24.0
./configure --prefix="$HOME/.local" && make -j"$(nproc)" && make install
```

Expect four of the five benchmarks to reproduce to the instruction on repeated
runs; only `reload_twenty_keys` drifts, by under 0.1 %. A local count that
moves by percent between identical runs means something is wrong with the
setup, not with the code.

The gate compares against a baseline committed under
`dynamic-config/benches/baselines/`, and the baseline has to be produced on the
CI image — instruction counts differ by libc and codegen, so one made on a
laptop would fail every run for reasons nobody changed. If that directory is
empty, `instructions.yml` says so with a warning and uploads an `iai-baseline`
artefact instead of comparing: download it, commit it, and the gate is armed
from the next run.

**Updating a baseline is legitimate exactly when the change that moved it is in
the same commit**, with a changelog entry explaining the move. A baseline
bumped separately is how a gate quietly stops meaning anything. The limits
themselves — 2 % for the read path, 10 % for reload, 25 % for `explain` — live
in `benches/instructions.rs` rather than in the workflow, because they are a
claim about the code.

## The concurrency claims: loom and shuttle

Two model checkers, because neither reaches what the other does. Both drive
the *real* code — `dynamic-config/src/sync.rs` hands the library `std`'s
primitives, loom's or shuttle's depending on the `cfg` — so neither suite is
a copy that can drift.

```sh
just loom          # 3 models, exhaustive, seconds
just shuttle       # 4 models, 50,000 schedules each, ~2s
just shuttle-soak  # the same models, 2,000,000 schedules each, ~65s
```

**loom** (`dynamic-config/tests/loom.rs`) explores *every* interleaving of a
small model, and it models atomic orderings faithfully — a `Relaxed` that
should have been `Acquire` fails there and nowhere else. That exhaustiveness
is why its models must stay small, and it is why loom cannot run two things
this crate does: `arc-swap`, which it cannot instrument at all, and
process-wide `static`s, which its iteration model does not tolerate.

**shuttle** (`dynamic-config/tests/shuttle.rs`) is the opposite trade: a
randomised scheduler over real code, unsound but scalable. It runs
`ConfigCell` (arc-swap and all), the reload-hook list, `ReloadGroup` under
concurrent reloaders, and a `static` cell awaited through `changes()` — none
of which has a loom model. What it does *not* do is see inside arc-swap
either: arc-swap's atomics are `std`'s, so shuttle places no yieldpoint
within a `load` or a `swap`, and those run atomically under it. The shuttle
models therefore claim things about generations, hook lists and wake-ups.
They do not claim "no torn read", and should not be read as claiming it.
Shuttle also treats every atomic as `SeqCst`, which is the other half of why
loom stays.

`just shuttle` runs from a **fixed seed**, so it explores the same schedules
every time and CI can gate on it without being flaky. Searching for
something new is `just shuttle-soak`, by hand. Either way the harness prints
the seed it used, and both knobs are environment variables:

```sh
SHUTTLE_SEED=1786567793006179371 just shuttle   # replay a reported seed
SHUTTLE_ITERATIONS=500000 just shuttle          # deeper, same seed
SHUTTLE_SEED=random just shuttle                # draw one, and print it
```

On a failure shuttle prints the exact schedule string as well, which
`shuttle::replay(body, "…")` re-runs step for step — that, rather than "run
it a lot", is the reason shuttle is here. Put the seed in the issue.

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
