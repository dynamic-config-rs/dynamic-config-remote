# Changelog

All notable changes to `dynamic-config-redis` are documented here. The format follows
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

- **`Redis::reporting_to(sink)`: a failing watch says so.** A watch loop is
  the half of a store `dynamic-config` cannot see. A delivery keeps the
  `RemoteStatus` current because `RemoteSink::apply` records one, so
  `dynamic_config_remote_up` reported the last *delivery* rather than the last
  *attempt* — and a Redis that stopped answering an hour ago looked healthy
  until something called `refresh_remote()`. Give the watch source the same
  sink the loop already pushes documents through and the failures **inside**
  the loop are reported as they happen.

  A reported failure moves the failure streak and nothing else, so
  `dynamic_config_remote_last_fetch_seconds` keeps ageing while
  `dynamic_config_remote_up` goes to zero — the pair an alert wants: down, and
  stale for how long. Only the failure's kind and key path are recorded, so
  the URL that carries the password stays out of it, and reporting is
  infallible: a loop must never have to handle a failure to report a failure.

  **Both of Redis' watch failures report, and the streak is what tells them
  apart.** A re-read that came back with nothing — one `MGET`, for a named
  list — is transient: the next write notifies again and one delivery clears
  the streak, so a blip looks like a blip while a credential the server has
  started refusing climbs. A dead subscription ends the watch, and is the
  failure nobody notices: the loop runs on a thread whose result is usually
  dropped, so configuration silently stops updating.

  **Refusals at the door are deliberately not reported** — a prefix, no
  format, no keys, notifications off, a subscription the server will not
  accept. `watch()` returns those to the caller standing there, before there
  is a loop to be silent in, and half of them are deployment mistakes rather
  than a store that stopped answering.

  Nothing else changes: `watch()` returns what it always returned, a source
  built without `reporting_to` reports nowhere, and `fetch()` already records
  itself through `refresh_remote()`.

- **`Redis::with_tls(url, keys, tls)`: a private certificate authority and a
  client certificate, as data.** The shared vocabulary from
  `dynamic-config-store-core`, so reaching TLS no longer means building a
  `redis::Client` by hand. Credentials still travel in the URL. All of it is
  expressible.

  Behind the existing `tls` feature, which is what `rediss://` needs anyway;
  the `TlsConfig` type itself is re-exported unconditionally. No new
  dependency.

  **TLS material on a `redis://` URL is refused**, naming the scheme, rather
  than by the client three layers down where the message carries no address:
  it is a deployment that believes it is encrypted and is not.

  The escape hatch is untouched and still the answer for anything this has
  no spelling for. Where both doors reach the same slot the interaction is
  defined rather than guessed. **There is deliberately no way to turn
  verification off** — the reasoning is in `TlsConfig`'s own documentation
  and in the book.

- **`Keys`, and reading several keys as one document.** `Keys::several([..])`
  merges named keys in call order — later wins, the rule `.file(..)` already
  teaches — and `Keys::prefix("myapp/")` merges the sections under a prefix,
  where an overlap between two of them is an error naming both keys and the
  paths they collided on. Every constructor takes `impl Into<Keys>` and a bare
  `&str` is still one key, so nothing that compiled before stops compiling.
  A named list is one `MGET`, which Redis runs as one operation. A prefix is a
  `SCAN` and then an `MGET` — **never `KEYS`**, which walks the whole key space
  in one blocking operation and is the classic way to stall a production
  server; the price is that the scan is not atomic. The prefix is matched as a
  literal: `*`, `?`, `[`, `]` and `\` in it are escaped before the `MATCH` goes
  out and every key that comes back is checked against the literal prefix, so a
  tenant id with a bracket in it selects itself and nothing else. Capped at 512
  keys, and at a thousand scan rounds so a server that never advances its cursor
  ends the fetch rather than the process. One unreadable key fails the whole
  fetch, naming it. Provenance becomes store-grained.
- **A key list whose extensions name two formats is refused by name**, rather
  than parsed as whichever came first. `with_format` settles it.

- **A named list can be watched.** `watch` now subscribes to
  `__keyspace@{db}__:{key}` for every key of the set, and the re-read that
  follows a notification is the one `MGET` the fetch already was. That is what
  makes it honest: `MGET` is a single command and Redis runs one command as one
  operation, so the document delivered is a state the server really held —
  never one key's new value beside another's old one. The read still *follows*
  the event rather than being simultaneous with it, so a delivery may carry a
  newer state than the write that woke it, and a set written with one `MSET`
  publishes once per key and is delivered once. Spurious, never torn.

  **`Keys::prefix` still refuses at `watch()`**, and now says why rather than
  lumping every multi-key shape together: re-finding the keys means a `SCAN`,
  and a cursor is many commands with writes free to land between them, so the
  set could be collected half from before a write and half from after. The
  refusal names `Keys::several` as the shape that works.

- **`Redis::with_timeout`** — the deadline for a single fetch attempt,
  excluding retries the underlying client performs. Ten seconds by default,
  where before there was no deadline at all. Redis has three separate knobs
  and this sets all of them from the one value: connect, write and read. A
  deadline covering only the connect sails past a server that accepted the
  socket and then stopped answering, which is what a wedged Redis looks like.

### Changed

- **The watch callback's panic net and the URL redaction moved to
  `dynamic-config-store-core`**, an internal crate the store crates share.
  No behaviour change: `redis://user:p@ss@w@rd@host` still redacts to
  `redis://user:***@host`, and `redis://app@host` still keeps the user name
  — which is where this crate differs from the NATS one, and that
  difference is now an argument rather than a second copy of the function.
- **A password Redis will not accept now reports `ErrorKind::Auth`** rather
  than `ErrorKind::Remote` — `NOAUTH`, `WRONGPASS`, `NOPERM`, and the client's
  own `AuthenticationFailed`. Reconnecting with the same password is not a
  recovery, and a caller can now see that without reading the message.

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

- A broken subscription ends the watch with an error — previously a dead
  socket spun the loop at full CPU while the handle looked alive.
- A watch callback that panics ends the watch with an error.

### Fixed

- URL redaction uses the *last* `@`, so a password containing `@` cannot
  leak a fragment into an error message.

## [0.0.1] — 2026-08-10

Initial release.

### Added

- `Redis` as a blocking `RemoteSource`: one key holds a whole configuration
  document; the connection is opened on first read and reused.
- Change-driven watching via keyspace notifications, refusing loudly at
  start when the server has them off or the database index cannot be
  determined — never a watch that silently cannot fire.
- Credentials in the URL, redacted before any error message; a `tls` feature
  for `rediss://`; `from_client` for a client the program already has.

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/ctolon/dynamic-config/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ctolon/dynamic-config/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/ctolon/dynamic-config/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/ctolon/dynamic-config/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ctolon/dynamic-config/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
