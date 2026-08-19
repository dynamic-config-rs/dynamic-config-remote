<div align="center">

# dynamic-config-remote

**The eight remote stores for [dynamic-config](https://github.com/dynamic-config-rs/dynamic-config), and the server that serves what they fetch.**

[![CI](https://github.com/dynamic-config-rs/dynamic-config-remote/actions/workflows/ci.yml/badge.svg?event=pull_request)](https://github.com/dynamic-config-rs/dynamic-config-remote/actions/workflows/ci.yml)
[![Security](https://github.com/dynamic-config-rs/dynamic-config-remote/actions/workflows/security.yml/badge.svg?event=pull_request)](https://github.com/dynamic-config-rs/dynamic-config-remote/actions/workflows/security.yml)
[![crates.io](https://img.shields.io/crates/v/dynamic-config-etcd.svg?label=dynamic-config-etcd)](https://crates.io/crates/dynamic-config-etcd)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

[**The Book**](https://dynamic-config-rs.github.io/remote/) · [The engine](https://github.com/dynamic-config-rs/dynamic-config) · [Changelog](CHANGELOG.md)

</div>

---

A configuration does not always live in a file next to the binary. It
lives in etcd because a cluster puts it there, in Vault because it is a
secret, in S3 because a deployment writes it once and a hundred pods read
it. These crates read those.

```toml
[dependencies]
dynamic-config = { version = "0.8.0", features = ["toml"] }
dynamic-config-etcd = "0.8.0"
```

```rust,ignore
let store = Etcd::new(["http://etcd:2379"]).key("myapp/db.json");

DatabaseConfig::builder("db")
    .file("config.toml")   // the base, from disk
    .remote(store)         // and what the cluster says on top of it
    .init()?;
```

**One trait, and the engine knows nothing else.** A store answers with a
document — text and a format — and everything after that is the engine's:
the same layering, the same validation, the same last-known-good cache,
the same `explain`. A store this project has never heard of works exactly
the same way.

## The crates

| Crate | Store | How it notices a change | Stability |
|---|---|---|---|
| [`dynamic-config-etcd`](dynamic-config-etcd) | etcd | push watch over gRPC | **Beta** |
| [`dynamic-config-consul`](dynamic-config-consul) | Consul KV | blocking queries | **Beta** |
| [`dynamic-config-nats`](dynamic-config-nats) | NATS JetStream KV | push watch | **Beta** |
| [`dynamic-config-redis`](dynamic-config-redis) | Redis | keyspace notifications | **Beta** |
| [`dynamic-config-vault`](dynamic-config-vault) | Vault KV v2 | version polling | **Beta** |
| [`dynamic-config-s3`](dynamic-config-s3) | S3 and compatibles | ETag polling — needs tokio | **Beta** |
| [`dynamic-config-firestore`](dynamic-config-firestore) | Firestore REST | `updateTime` polling | **Beta** |
| [`dynamic-config-git`](dynamic-config-git) | a git repository | shallow single-ref fetch — GitHub, GitLab, Azure DevOps | **Beta** |
| [`dynamic-config-server`](dynamic-config-server) | — | serves configuration over HTTP, per-caller authorisation | **Beta** |

`dynamic-config-store-core` is also published and is not in the table: it
is machinery these crates share rather than something to depend on.

Every store follows the same contract — the current value is not announced
at startup, a deleted key is not a change, transport failures retry, a
panicking callback ends the watch with an error — and each documents its
stop latency and change-detection rule side by side in
[Store Crates at a Glance](https://dynamic-config-rs.github.io/remote/remote-stores/store-crates.html).

**What promoted these from Experimental is evidence**: each is tested
against a real server in a container, each watch loop's failure branches
are enumerated in its own documentation, and three of them are unplugged
mid-watch by `just chaos` — toxiproxy in front of a container that never
restarts, so the port stays put while the connection does not.

## The engine is a dependency, not a sibling

These crates name it with a caret (`dynamic-config = "0.6"`), so an engine
patch release reaches them with no release here, and a breaking one is
picked up deliberately. The engine, the macro and the loader are
[their own repository](https://github.com/dynamic-config-rs/dynamic-config).

## MSRV

**Rust 1.88 — one floor, every crate**, since the org-wide raise. Older
toolchains resolve the last pre-raise releases through cargo's
MSRV-aware fallback and are end-of-life; the
[Compatibility Contract](https://dynamic-config-rs.github.io/compatibility.html)
carries the policy.

MSRV changes are breaking, and every floor has a CI row against a real
toolchain rather than a number somebody remembered.

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md). `just check` is the gate without
Docker; `just containers` and `just chaos` are the ones with it.

What you may build on and find unchanged tomorrow is written down: the [Compatibility Contract](https://dynamic-config-rs.github.io/compatibility.html).

## License

[MIT](LICENSE).
