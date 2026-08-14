# Changelog

All notable changes to `dynamic-config-etcd` are documented here. The format follows
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

## [0.6.1] — 2026-08-14

### Changed

- **A watch refused before its first round trip no longer reports the cluster
  as unreachable.** No format, or a source naming a list of keys: those are
  refusals this crate makes about the *source*, before a request has left the
  process, and `RemoteStatus::reachable()` is *whether the store answered the
  last time it was asked*. `Some(false)` there was a status saying something
  untrue about a cluster that may be perfectly healthy — and a status carries
  a kind and a path and no message, so nothing downstream could correct it.
  The error still says exactly what is wrong, to the caller holding it.

  This was 0.6.1's audit of all seven watch loops settling a split: this crate
  and `dynamic-config-nats` reported such a refusal, `dynamic-config-redis`
  and `dynamic-config-s3` did not, and each had a test asserting its own half.
  Every failure a watch meets *after* the first round trip reports exactly as
  it did.

### Added

- **Every failure branch of the watch loop is in a table** in this crate's
  documentation, marked *reports* or *silent* with the reason — including the
  two silences that are decisions rather than gaps: a token that expired and
  was refreshed successfully, and a key that was deleted.
- **A chaos test** (`tests/chaos.rs`, `just chaos`): a stream cut mid-watch by
  a toxiproxy in front of a cluster that never restarts. It asserts the pair
  an alert reads — `remote_up` goes to zero *while the staleness clock keeps
  running* — and that the document that was serving before the cut is still
  serving after it.

## [0.6.0] — 2026-08-13

### Added

- **`Etcd::reporting_to(sink)`: a failing watch now says so.** A watch is the
  half of a store `dynamic-config` cannot see — `RemoteSink::apply` records a
  delivery, so a *working* watch keeps `RemoteStatus` current, while a loop
  whose stream broke delivers nothing and used to report nothing.
  `dynamic_config_remote_up` therefore described the last *delivery* rather
  than the last *attempt*, and a store that stopped answering an hour ago
  looked healthy until something called `refresh_remote_async()`.

  One builder option rather than a second `watch` method: a sink is `Copy` and
  captured at wiring time anyway, and it fences a stale loop's reports away
  from a replacement source exactly as the delivering half is fenced. The
  default is that nobody is listening, so a caller who does not ask pays for
  nothing.

  Every end of the watch reports — the watch that could not be established,
  the stream erroring, etcd cancelling the watch, the range read at an event's
  revision failing (the key budget and a refused overlap with it), a value
  that is not UTF-8, and the connection closing — and reporting cannot fail,
  because a loop must never have to handle a failure to report a failure. Only
  the streak and the last failure move, so the staleness clock keeps ageing
  while `remote_up` goes to zero. `fetch` is untouched: a fetch already records
  itself through `refresh_remote_async()`.

  **A replaced auth token is deliberately not a failure.** etcd's simple
  tokens expire, so a long-lived watch turns one over routinely; the loop logs
  in again and resumes from the last delivered revision, and the store
  answered. Reporting that would drive `remote_up` to zero every five minutes
  on a healthy cluster — and, since only a delivery or a fetch clears the
  streak, hold it there until the next configuration change. A
  re-authentication that *fails*, a stream that will not re-establish, and the
  recovery cap running out all report. `on_change`'s own refusal does not: the
  store answered, `apply` counted the delivery, and whether the document then
  installs is `ConfigStatus`'s half of the picture.

- **`Etcd::with_tls(endpoints, keys, options, tls)`: a private certificate
  authority and a client certificate, as data.** The shared vocabulary from
  `dynamic-config-store-core`, so reaching TLS no longer means naming a
  `tonic` type — while `ConnectOptions` keeps carrying everything that is
  not TLS. mTLS matters more here than elsewhere: an etcd started with
  `--client-cert-auth` is the ordinary hardened deployment. All of it is
  expressible.

  Behind the existing `tls` feature, which is what buys the stack; the
  `TlsConfig` type itself is re-exported unconditionally so it can be named
  either way. No new dependency.

  **The `tls` argument owns the TLS slot.** `etcd-client` exposes no way to
  ask whether `ConnectOptions` already carries a `TlsOptions`, so this one
  interaction is documented rather than refused: use one door or the other.

  The escape hatch is untouched and still the answer for anything this has
  no spelling for. Where both doors reach the same slot the interaction is
  defined rather than guessed. **There is deliberately no way to turn
  verification off** — the reasoning is in `TlsConfig`'s own documentation
  and in the book.

  A new example, `etcd_client_certificate`, presents a client certificate to
  an etcd started with `--client-cert-auth`.

- **`Keys`, and reading several keys as one document.** `Keys::several([..])`
  merges named keys in call order — later wins, the rule `.file(..)` already
  teaches — and `Keys::prefix("myapp/")` merges the sections under a prefix,
  where an overlap between two of them is an error naming both keys and the
  paths they collided on. Every constructor takes `impl Into<Keys>` and a bare
  `&str` is still one key, so nothing that compiled before stops compiling.
  Both shapes are one round trip at one etcd revision: a list goes as a
  transaction of range reads, a prefix as one range read, so a write landing
  mid-read cannot tear the document. A list is capped at etcd's own
  `--max-txn-ops` (128) and refuses rather than splitting into several
  revisions; a prefix is capped at 512 keys. One unreadable key fails the whole
  fetch, naming it. Provenance becomes store-grained — the merged document is
  one layer, so `source_of` names the store and the set.
- **A key list whose extensions name two formats is refused by name**, rather
  than parsed as whichever came first — which produced a syntax error about a
  document that had no syntax error in it. `with_format` settles it.

- **A prefix can be watched.** One stream over one range says *the range
  moved* and carries the revision it moved at, and one range read at that
  revision is the whole set as of one instant — so the document delivered is a
  state the cluster really was in, never one key's new value beside another's
  old one. The read happens once per batch rather than once per event, because
  a batch is one revision; a token that expired under a live stream is
  refreshed and the read retried once; a prefix left with nothing under it is
  the no-configuration case and leaves the running snapshot alone; and a
  subtree that cannot be folded — two keys supplying one path — ends the watch
  rather than being retried forever in silence. A **named list** still refuses
  at `watch()`, and now says why: etcd establishes a watch on a key or a range,
  so a list would be one stream per key and none of them would say the set
  moved together.

- **`Etcd::with_timeout`** — the deadline for a single fetch attempt,
  excluding retries the underlying client performs. Ten seconds by default.
  etcd's own `ConnectOptions::with_timeout` bounds *connecting*, which does
  nothing for a member that accepts a request on an established connection and
  then goes quiet; this wraps the request itself. It does not apply to `watch`.

### Changed

- **The watch callback's panic net moved to `dynamic-config-store-core`**,
  an internal crate the store crates share. It was seven byte-identical
  copies; it is now one, with the same behaviour and its own test.
- **A refusal etcd words as its own — `invalid auth token`, `authentication
  failed`, `permission denied` — now reports `ErrorKind::Auth`** rather than
  `ErrorKind::Remote`, on a read, a watch, a token refresh and a connect. Only
  a refusal that arrived as a gRPC status counts: an OS-level "permission
  denied" reading a certificate file is still an I/O problem, not a credential
  one.

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

- A watch callback that panics ends the watch with an error instead of
  unwinding through the caller's task.

### Fixed

- An expired auth token *during* the watch stream re-logs-in and
  re-establishes the stream, instead of failing terminally — previously only
  the initial connection recovered. The new stream resumes just past the
  last delivered revision, so a write landing while the stream is down is
  replayed rather than lost, and consecutive recoveries are capped so a
  server that accepts logins while failing the stream cannot be hammered.

## [0.0.1] — 2026-08-10

Initial release.

### Added

- `Etcd` as an `AsyncRemoteSource`: one key holds a whole configuration
  document; the format comes from the key's extension or `with_format`.
- A real push watch over etcd's gRPC stream — no polling.
- Credentials and TLS through `etcd-client`'s own `ConnectOptions`
  (re-exported), `from_client` for a client the program already has, and
  `tls` / `tls-roots` features.

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/ctolon/dynamic-config/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/ctolon/dynamic-config/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ctolon/dynamic-config/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/ctolon/dynamic-config/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/ctolon/dynamic-config/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ctolon/dynamic-config/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
