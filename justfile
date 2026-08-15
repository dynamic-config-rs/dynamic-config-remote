# Everything CI runs, in the order that fails fastest.
#
# The container-backed suites are separate: they need a Docker daemon, and
# a contributor without one should still be able to run everything else.

default: check

# fmt, clippy, tests, docs — the whole gate, locally. No Docker needed:
# the tests that drive real servers live in `containers`, and their
# non-Docker mock tests still run here.
check: fmt lint test docs

# Formatting, as CI checks it.
fmt:
    cargo fmt --all -- --check

# Clippy with warnings denied, at both ends of the feature range.
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo clippy --workspace --all-targets --no-default-features -- -D warnings

# Everything that does not need a daemon: the seven container-backed
# crates are excluded and run by `containers`. What is left is
# `store-core`, git — its fixture is a repository in a temporary
# directory — and the server.
test:
    # Default features: the server's TLS suite asserts a refusal that is
    # `#[cfg(unix)]`, and it gets its own line below.
    cargo test --workspace \
        --exclude dynamic-config-etcd --exclude dynamic-config-consul \
        --exclude dynamic-config-nats --exclude dynamic-config-vault \
        --exclude dynamic-config-redis --exclude dynamic-config-s3 \
        --exclude dynamic-config-firestore
    # The server's TLS suite needs its own line: it has no `full` feature,
    # so a default build never compiles the handshake, the key-permission
    # refusal or the mTLS tests.
    cargo test -p dynamic-config-server --all-features

# The seven networked stores against real servers. Needs a Docker daemon.
# git is not here: its fixture is a repository it builds itself.
containers:
    cargo test -p dynamic-config-etcd -p dynamic-config-consul \
               -p dynamic-config-nats -p dynamic-config-vault \
               -p dynamic-config-redis -p dynamic-config-s3 \
               -p dynamic-config-firestore -- --test-threads=2

# The watch loops, with the store unplugged underneath them: toxiproxy in
# front of a container that never restarts, so the port stays put while
# the connection does not. Needs a Docker daemon.
chaos:
    cargo test -p dynamic-config-redis --test chaos -- --ignored --nocapture
    cargo test -p dynamic-config-consul --test chaos -- --ignored --nocapture
    cargo test -p dynamic-config-etcd --test chaos -- --ignored --nocapture

# Docs as docs.rs builds them: every feature, and a warning is an error.
docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps

# Every crate's floor still compiles. Needs the toolchains
# (`rustup toolchain install 1.85 1.88`).
msrv:
    #!/usr/bin/env bash
    set -euo pipefail
    cp Cargo.lock Cargo.lock.pinned
    trap 'mv Cargo.lock.pinned Cargo.lock' EXIT
    cargo +stable generate-lockfile
    cargo +1.71 check -p dynamic-config-store-core --locked
    cargo +1.85 check -p dynamic-config-etcd --locked
    cargo +1.85 check -p dynamic-config-consul --locked
    cargo +1.88 check -p dynamic-config-nats --locked
    cargo +1.85 check -p dynamic-config-vault --locked
    cargo +1.88 check -p dynamic-config-redis --locked
    cargo +1.88 check -p dynamic-config-s3 --locked
    cargo +1.85 check -p dynamic-config-firestore --locked
    cargo +1.85 check -p dynamic-config-git --locked
    cargo +1.80 check -p dynamic-config-server --locked
    cargo +1.80 check -p dynamic-config-server --locked --all-features

# Every example builds, including the one behind `required-features`.
examples:
    cargo build --examples -p dynamic-config-etcd -p dynamic-config-consul \
                -p dynamic-config-nats -p dynamic-config-vault \
                -p dynamic-config-redis -p dynamic-config-s3 \
                -p dynamic-config-firestore -p dynamic-config-git
    cargo build -p dynamic-config-server --all-features --examples

# This repository's book. The docs site builds it alongside the other
# three and publishes all four together; this is the same build, alone.
# Needs mdbook (`cargo install mdbook`).
book:
    mdbook build book
    test -f book/book/index.html

# Advisories, licences and registries.
audit:
    cargo deny check
