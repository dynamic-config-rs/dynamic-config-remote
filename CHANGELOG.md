# Changelog

All notable changes to the remote stores and the config server are
documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Before 1.0, a breaking change bumps the **minor** version and anything else
bumps the patch. A change to the minimum supported Rust version is
breaking.

Ten crates on one version: each store's own changelog carries what changed
in it, and this file carries what changed across them.

**The engine is not in this repository.** It is named with a caret
(`dynamic-config = "0.6"`), so a patch release of the engine reaches these
crates without a release here — and a breaking one is picked up
deliberately, with an entry under Changed saying so.

<!-- Keep this template. Add entries under `Unreleased` as you go, and move
     the whole block under a new version heading at release time.
     (Spelled `_Unreleased_` here so cargo-release's `exactly = 1` search
     for the real heading matches only the real heading.)

## [_Unreleased_]

### Added
### Changed
### Deprecated
### Removed
### Fixed
### Security

-->

## [Unreleased]

## [0.9.0] — 2026-08-21

### Changed

- **The engine floor is 0.9**, which is what makes this wave 0.9.0 too:
  the watch contract lives in the engine, so a store implementing it
  cannot build against an older one. Two transitive floors move with
  it — `serde` to `1.0.228` and `serde_json` to `1.0.149`, both
  required by the crate the engine folds with. Neither moves the MSRV.

### Added

- **Every store says how it learns that its document changed, and
  watches through the trait.** The engine's 0.9 carries the contract —
  `WatchCapability`, `RemoteSource::watch`, `Remote::watch` — and each
  store now answers it with the mechanism it already had:

  | store | capability | mechanism |
  |---|---|---|
  | consul | Native | a blocking query on `X-Consul-Index` |
  | etcd | Native | the gRPC watch stream |
  | nats | Native | a JetStream KV watch |
  | redis | Native | keyspace notifications |
  | config-server | Native | **new** — the change stream, below |
  | vault | Conditional | the KV metadata version counter |
  | s3 | Conditional | `HEAD` and an ETag |
  | git | Conditional | the ref advertisement |
  | firestore | Conditional | `updateTime` |

  What changes for a caller: a watch is reachable through the *installed
  source* rather than through the concrete type. A program that swapped
  Vault for etcd used to keep polling, because the type its watch was
  written against had been erased; `Remote::watch` now drives whichever
  is installed and gets a push where there is one to get.

  Nothing in a store's own surface changed. Each keeps its inherent
  `watch`, and the trait method forwards to it.

- **`dynamic-config-server`: the client subscribes.**
  `ConfigServer::watch` follows `GET /{application}/{profile}/stream`:
  connect, read events, re-fetch when the generation moves, reconnect
  from the `Last-Event-ID` the server left off at. The server has served
  that stream since 0.7 and no client followed it, which made the
  endpoint something you had to write a loop for.

  Resumption is a comparison rather than a replay — a generation
  subsumes every one before it — so a reconnect cannot land past a
  change and miss it. A document is delivered only when it differs from
  the last one, an event that runs past 64 KiB is refused, and a stream
  that goes silent for fifty seconds is treated as dead: the server
  sends a keep-alive comment every fifteen precisely so that silence
  means something. Tested against the real router, as the rest of the
  client half is.

  `examples/server_watching.rs` runs it against the compose pair, beside
  the `served` example's poll — the two side by side are the comparison
  worth having.

## [0.8.0] — 2026-08-19

### Changed

- **The engine floor is 0.8** — and that bump is why THIS wave is
  0.8.0 too: every store implements the engine's `RemoteSource`, so
  the engine's breaking release (a `LoadSpec` field, the MSRV) is
  breaking here by composition. Nothing in these crates' own surface
  changed shape.

### Added

- **`dynamic-config-server`: Kubernetes authentication** (the
  `kubernetes-auth` feature, default-on for the binary). A caller
  presents its projected service-account token; the server asks the
  API server whose it is (TokenReview) and `[[kubernetes.grants]]`
  maps `namespace:serviceaccount` to applications — no client tokens
  minted, distributed or rotated at all. Verdicts cached 60s by keyed
  hash (the token is never stored); static `[[clients]]` checked
  first; refuses to start outside a cluster, naming what is missing.
- **`ConfigServer::with_token_file`** — the bearer read from a file,
  re-read at every fetch, for credentials something else rotates
  underneath the client: first among them the projected token above.
  Wins over `with_token` when both are set.

## [0.7.0] — 2026-08-18

### Added

- **The config server, served and consumed.** `examples/compose/` runs
  the server in a container from a real `[server]` configuration, and
  `dynamic-config-server`'s `served` example (behind the `client`
  feature) is the first thing to drive the crate's own `ConfigServer`
  client end to end — the pairing the book's chapter now opens with.

### Changed

- **The config-server chapter opens with running it.** The threat model
  is still the design's source, and still required reading before
  production — it is no longer the doorway to trying the server out.
- **The book opens with a Quick Start**: one store, a file base, and a
  change in the cluster reaching a running process.

### Changed

- **The book has parts** — *Guide*, *The Stores*, *The Config Server* and
  *Advanced* — where the eight store chapters had been nested under *Remote
  Stores* and *Writing a Store* sat among them. No page moved file.

## [0.6.2] — 2026-08-16

### Changed

- **These crates left the engine's repository.** Eight stores, what they
  share and the config server are now
  [dynamic-config-remote](https://github.com/dynamic-config-rs/dynamic-config-remote),
  released on their own schedule. Nothing about them changed: same crate
  names, same versions, same API. What changed is the dependency on the
  engine — `=0.6.1` became `"0.6"`, so upgrading the engine no longer
  requires a release here. **That takes effect with this release**: the
  0.6.1 crates on the registry still carry the exact pin.

- **Every crate's `repository` and `homepage` point at the new home**, and
  each README's badges and book links with them. The chapters for these
  crates are now their own book, at
  [dynamic-config-rs.github.io/remote/](https://dynamic-config-rs.github.io/remote/).

[Unreleased]: https://github.com/dynamic-config-rs/dynamic-config-remote/compare/v0.9.0...HEAD
[0.9.0]: https://github.com/dynamic-config-rs/dynamic-config-remote/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/dynamic-config-rs/dynamic-config-remote/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/dynamic-config-rs/dynamic-config-remote/compare/v0.6.2...v0.7.0
[0.6.2]: https://github.com/dynamic-config-rs/dynamic-config-remote/compare/v0.6.1...v0.6.2
