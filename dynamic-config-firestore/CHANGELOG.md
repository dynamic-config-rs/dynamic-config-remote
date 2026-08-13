# Changelog

All notable changes to `dynamic-config-firestore` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Before 1.0, a breaking change bumps the **minor** version and anything else
bumps the patch. A change to the minimum supported Rust version is breaking.

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

## [0.6.0] — 2026-08-13

### Added

- **`reporting_to(RemoteSink)`: a failing watch is no longer invisible.** The
  loop now records every poll that came back with nothing on the sink its
  callback already applies documents through — a read that failed, and the
  missing-`updateTime` refusal on its way out. Without it
  `dynamic_config_remote_up` reported the last *delivery* rather than the last
  *attempt*, and a poll is exactly where the two cases are indistinguishable
  from outside: a document nobody edited and a project that stopped answering
  deliver the same nothing.

  A failed attempt moves the failure streak and nothing else: the fetch clock
  keeps ageing, so `dynamic_config_remote_last_fetch_seconds` still says how
  stale the served document is while `remote_up` goes to zero. Reporting is
  infallible and silent — a loop is never handed a failure to report a failure
  — and a source built without it records nowhere, as before. Only an
  `ErrorKind` and a key path are recorded; the endpoint never enters a status.

- **`with_tls(TlsConfig)`: a private certificate authority and a client
  certificate, as data.** The shared vocabulary from
  `dynamic-config-store-core`, so reaching TLS no longer means building a
  `ureq::Agent`. Rarely wanted against Google's own endpoint, whose
  certificates chain to an authority the platform already trusts — it is for
  the deployments that do not go there directly: an enterprise
  TLS-inspecting proxy, or an emulator behind `with_endpoint`. All of it is
  expressible. No new feature and no new dependency.

  `with_agent` and `with_tls` together are **refused** at the first request,
  naming both calls.

  The escape hatch is untouched and still the answer for anything this has
  no spelling for. Where both doors reach the same slot the interaction is
  defined rather than guessed. **There is deliberately no way to turn
  verification off** — the reasoning is in `TlsConfig`'s own documentation
  and in the book.

- **`Keys`, and reading several documents as one section.**
  `Keys::several([..])` reads the named documents and merges them under the one
  section key in call order — later wins, the rule `.file(..)` already teaches.
  `Firestore::new` takes `impl Into<Keys>` and a bare `&str` is still one
  document, so nothing that compiled before stops compiling. It is **one
  `:batchGet` request**, which is Firestore's own answer to a set rather than a
  loop wearing a batch's name — with two things the API's own documentation
  makes explicit and this crate therefore does not hide: the service returns
  the documents in whatever order it likes, so they are put back into call
  order here, and one request is not one snapshot, because without a
  transaction each document is read at its own time. None is opened: an open
  read-only transaction is state on the service a configuration read would have
  to remember to release. One missing document fails the whole fetch, naming
  it; a document nobody asked for, one answered twice, and one the store says
  nothing at all about are each refused; provenance becomes store-grained; and
  a multi-document source refuses to be watched, because the `updateTime` that
  watch compares belongs to one document.
- **There is deliberately no collection form**, and `documents.list` is not
  what is missing. A document is a section's *contents*, so folding a
  collection into one section would make `config/db` and `config/server`
  collide on `host` — the ordinary layout, refused — and naming a sub-section
  after each document's id would invent a convention no other store in this
  family has. The crate documentation and the book carry the reasoning.

### Changed

- **The token cache moved to `dynamic-config-store-core`**, an internal
  crate the store crates share. No behaviour change: the same margin, the
  same proactive refresh, the same one-shot retry after a `401`, and the
  same tests. The metadata-server call itself stays here.
- **A rejected credential now reports `ErrorKind::Auth`** rather than
  `ErrorKind::Remote`: the `401` this crate already detected, a `403`
  (`PERMISSION_DENIED` — the identity is not allowed to read the document),
  and a metadata server that refuses to mint a token. Exhausted quota is a
  `429` and stays `Remote`, because that one comes right on its own.
- **`with_timeout` documents the semantics the whole store family shares**:
  the deadline for a single fetch attempt, excluding retries the underlying
  client performs. No behaviour change; the README has a Timeouts section.

## [0.5.0] — 2026-08-12

## [0.4.0] — 2026-08-12

## [0.3.0] — 2026-08-11

## [0.2.0] — 2026-08-11

### Changed

- Released in lockstep with `dynamic-config` 0.2.0, where the attribute
  declares and the builder configures. This crate's own surface is
  unchanged; its examples and docs now configure through the builder.

## [0.1.0] — 2026-08-10

### Breaking

- `Debug` no longer prints credentials (access tokens redacted).
- A watch callback that panics ends the watch with an error.

### Changed

- Token refresh margin documented (60s, `REFRESH_WITHIN`).

## [0.0.1] — 2026-08-10

Initial release.

### Added

- `Firestore` as a blocking `RemoteSource` over the REST API: a document's
  fields become the configuration section, with Firestore's value types
  mapped the obvious way.
- Watching by polling `updateTime`; a document without one ends the watch
  with an error instead of never firing.
- Auth: workload identity via the metadata server (tokens cached, refreshed,
  retried exactly once on a *typed* 401), a supplied access token, or the
  emulator's nothing-at-all. A service-account JSON key is deliberately not
  supported.

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/ctolon/dynamic-config/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ctolon/dynamic-config/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/ctolon/dynamic-config/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/ctolon/dynamic-config/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ctolon/dynamic-config/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
