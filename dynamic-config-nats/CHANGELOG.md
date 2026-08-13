# Changelog

All notable changes to `dynamic-config-nats` are documented here. The format follows
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

- **`Nats::reporting_to(sink)`: a failing watch now says so.** A watch is the
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

  Every end of the watch reports — the source that refuses to be watched, the
  watch that could not be established, the stream erroring, a value that is
  not UTF-8, and the stream closing — and reporting cannot fail, because a
  loop must never have to handle a failure to report a failure. Only the
  streak and the last failure move, so the staleness clock keeps ageing while
  `remote_up` goes to zero. `fetch` is untouched: a fetch already records
  itself through `refresh_remote_async()`. `on_change`'s own refusal is not
  reported either — the store answered, `apply` counted the delivery, and
  whether the document then installs is `ConfigStatus`'s half of the picture.

  What this covers follows from the crate's existing promise that reconnecting
  is the client's job: **a server that goes away is not a failed watch**,
  because `async-nats` keeps recreating the subscription for as long as it
  takes and the loop waits through it. What reaches this crate is a stream
  that stopped — a deleted bucket, a consumer that is gone — and that is what
  is reported.

- **`Nats::with_tls(server, bucket, key, options, tls)`: a private
  certificate authority and a client certificate, as data.** The shared
  vocabulary from `dynamic-config-store-core`, so reaching TLS no longer
  means naming an `async-nats` type — while `ConnectOptions` keeps carrying
  everything that is not TLS. No new feature and no new dependency.

  **NATS cannot express the PEM-bytes spellings.** `async-nats` opens the
  files itself, and the only byte-taking door is a hand-built
  `rustls::ClientConfig` — a direct `rustls` dependency and a
  crypto-provider decision, for one spelling. So the byte forms are
  **refused**, naming the call and pointing at the file forms; they are not
  ignored. Writing the material to a temporary file is deliberately not done:
  it would put a private key on a disk that never asked for one.

  Naming a certificate authority also sets `require_tls`, so a `nats://` URL
  fails rather than quietly negotiating plaintext against an authority the
  caller just named.

  The escape hatch is untouched and still the answer for anything this has
  no spelling for. Where both doors reach the same slot the interaction is
  defined rather than guessed. **There is deliberately no way to turn
  verification off** — the reasoning is in `TlsConfig`'s own documentation
  and in the book.

- **`Keys`, and reading several keys as one document.** `Keys::several([..])`
  merges named keys in call order — later wins, the rule `.file(..)` already
  teaches. Every constructor takes `impl Into<Keys>` and a bare `&str` is still
  one key, so nothing that compiled before stops compiling. The KV API has no
  batch read, so a list is one get per key and is **not** read atomically. One
  unreadable key fails the whole fetch, naming it; provenance becomes
  store-grained; and a multi-key source refuses to be watched — `watch_many`
  could say the set moved, but nothing here could then re-read the set as of
  one instant.
- **There is deliberately no prefix form**, and it is the client that decides
  that: `Store::keys()` is the only listing `async-nats` exposes and it walks
  the whole bucket, the filtered constructor being private. A prefix would be a
  full-bucket scan wearing a prefix's name — the 512-key bound would be a bound
  on the bucket, and a bucket of a hundred thousand keys would stream a hundred
  thousand headers to find three. Name the keys, or give the set its own
  bucket, which is the partition NATS actually offers.
- **A key list whose extensions name two formats is refused by name**, rather
  than parsed as whichever came first. `with_format` settles it.
- **`Nats::with_timeout`** — the deadline for a single fetch attempt,
  excluding retries the underlying client performs. Ten seconds by default.
  It bounds **each** get, so a source reading several keys reads each of them
  under this deadline rather than sharing one between them.
  `ConnectOptions::request_timeout` is its twin on the connection side; this
  one wraps the KV read, so a server that accepted the request and then went
  quiet ends the fetch rather than parking it. It does not apply to `watch`.

### Security

- **A credential in the server URL no longer reaches an error message.**
  `nats://token@host:4222` and `nats://user:password@host:4222` are shapes
  NATS accepts, and the address is quoted by `describe()` into every error and
  into `Debug`. It is redacted before it is stored now, the way the Redis
  companion already redacted its own URL. A comma-separated server list is
  redacted server by server.

### Changed

- **The watch callback's panic net and the URL redaction moved to
  `dynamic-config-store-core`**, an internal crate the store crates share.
  No behaviour change: `nats://token@host` still redacts to
  `nats://***@host` and a comma-separated list is still redacted server by
  server, because what an authority with no colon in it *means* is now an
  argument rather than a second copy of the function. The Redis crate reads
  that same shape as a user name, and now says so.
- **A credential the server refuses now reports `ErrorKind::Auth`** rather
  than `ErrorKind::Remote`, for the two failures `async-nats` names as such:
  a nonce it would not sign for, and an outright authorization violation.
  Both happen at construction. A later read refused for want of permission
  arrives as an undifferentiated KV error and stays `Remote` — guessing there
  would stop a watch loop that a reconnect would have fixed.

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

- A watch callback that panics ends the watch with an error.

## [0.0.1] — 2026-08-10

Initial release.

### Added

- `Nats` as an `AsyncRemoteSource` over a JetStream KV bucket: one key holds
  a whole configuration document.
- A real push watch over the bucket's watch stream — no polling; a deleted
  key is not delivered as a change.
- Credentials through `async-nats`'s own `ConnectOptions` (re-exported), and
  `from_client` for a connection the program already has.

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/ctolon/dynamic-config/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ctolon/dynamic-config/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/ctolon/dynamic-config/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/ctolon/dynamic-config/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ctolon/dynamic-config/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
