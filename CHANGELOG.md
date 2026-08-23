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

### Added

- **Vault reads dynamic secrets.** `Vault::with_dynamic()` reads the path
  itself rather than KV v2's `data/` nesting, takes the values from `data`,
  and keeps the `lease_id`, `lease_duration` and `renewable` the engine
  answered with — so `database/creds`, `pki/issue` and `aws/creds` are
  readable as configuration. Explicit rather than detected from the response:
  a mistyped mount should be an error, not a quiet fall into the other mode,
  and each refusal names the builder to add or drop.
- **A decryption stage between the store and the resolver.** A configuration
  document in git whose passwords are ciphertext is reviewable in a pull
  request, diffable, and useless to whoever clones the repository — the
  shape SOPS made popular. `store-core`'s `Transformed` is a source that
  holds a source and rewrites the document on its way past;
  `dynamic-config-vault`'s `Transit` is the transform that decrypts
  `vault:v1:…` values through a transit mount.

  A wrapper rather than a stage inside the engine, because a wrapper
  composes with every store already: git, S3, Consul and the config server
  are the same line of code, and an application that encrypts nothing gains
  no API and pays nothing.

  The revision and the lease pass through untouched, so the recency fence
  and the renewal timer are unaffected. `Transit` decrypts **values**
  rather than whole documents — a file that is one opaque blob cannot be
  reviewed — in one batch request whatever the count, and in no request at
  all for a document with nothing sealed in it. It borrows the `Vault`
  beside it rather than authenticating a second time.
- **A `pki/issue` certificate's own expiry binds its lease.** A PKI role can
  hand back a lease longer than the certificate it issued — the lease is
  Vault's accounting record, `notAfter` is what a TLS peer enforces — so the
  `Lease` now carries whichever runs out first. Vault reports `notAfter` as
  `data.expiration`, so this needs no X.509 parser. An `expiration` already
  in the past is treated as clock skew rather than believed: the two clocks
  belong to different machines, Vault does not issue expired certificates,
  and the arithmetic would otherwise re-issue in a tight loop.
- **`RenewableSource` for Vault**: `renew` through `sys/leases/renew` and
  `revoke` through `sys/leases/revoke`. The answer to a renewal is
  authoritative — a role's `max_ttl` is a ceiling the caller cannot see — so
  the next renewal is scheduled from what Vault granted, never from what was
  asked for. Dynamic mode reads **one** path: every read mints a credential
  with its own lease, and merging several would leave every lease but one
  unrenewed and unrevoked.
- **Every store that has a revision now reports one.** Vault's KV version and
  Consul's index as `Counter`; etcd's `mod_revision` and NATS' KV revision
  likewise; git's commit, S3's ETag and Firestore's `updateTime` as `Opaque`;
  the config server's envelope generation as `Counter`. That is what makes
  the engine's new recency fence real: a slow read of an old version can no
  longer land on top of a fast read of a new one. Redis reports none, because
  keyspace notifications carry none.

- **`TlsConfig` gains a server name and a way to go without verification.**
  `with_server_name` says which name the certificate must carry when the
  address is one it does not name — a cluster IP, a load balancer — and
  keeps the server authenticated: the certificate still chains and still has
  to carry that name. `with_skip_verification` turns authentication off
  entirely, which is not a weaker TLS but TLS without the part that means
  anything.

  Neither is uniform, and the two supported sets are almost disjoint:
  `ureq` (Vault, Consul, Firestore) can skip verification and cannot
  override a name; `tonic` (etcd) can override a name and cannot skip. The
  config server does both, git skips, and Redis, NATS and S3 do neither. A
  store that cannot express one **refuses the whole configuration by name**,
  on the same rule the other three settings already followed.

  Skipping exists because deployments exist that cannot be fixed today and
  still have to run, and because the alternative to a named switch was a
  caller reaching into `with_agent` or `from_client` where nothing warns.
  `with_ca_certificate_file` remains the answer to both cases people usually
  reach for it in.
- **A ceiling on requests in flight per source.** A watch, a resync, an auth
  refresh and a lease renewal can all be out at once, and five thousand pods
  doing that to one store is the number this exists for. Ten by default — the
  same number and the same argument as the reference implementation's
  `max_connections_per_host` — and a request past the ceiling is **refused
  rather than queued**, so it backs off through the pace it already has. A
  queue would hold a delivery behind a resync behind an auth refresh, none of
  which is improved by being later.

### Changed

- **A document that is not there is `Absent`, not `Remote`.** "No key held a
  value" was categorised as a store being unreachable — the category a watch
  loop waits out — so a deleted Vault secret produced no delivery, no log
  line and no metric movement, and the stale document went on being served
  with every health check reporting fine. A 404 and a KV v2 soft delete are
  both `Absent` now, and the two conditions are told apart because they
  deserve different answers: waiting cures an outage and does not bring back
  a key.
- **The polling watch loops back off.** Vault, Consul and S3 each override
  `watch` and so never reached `Pace`: they slept a flat interval, which
  meant a mistyped path polled at full rate forever and a fleet that lost its
  store together came back to it together. All three are paced now, and reset
  on success.
- **S3 has a default timeout** where this crate built the client. The AWS SDK
  ships with none, which made an unanswered `GET` indistinguishable from a
  slow one and could park a watch loop indefinitely. Ten seconds per attempt,
  matching every other store here; `from_client` is untouched, because a
  caller who configured their own client configured their own timeouts.
- The engine floor is **0.10**, and `Keys` is `#[non_exhaustive]`.

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
