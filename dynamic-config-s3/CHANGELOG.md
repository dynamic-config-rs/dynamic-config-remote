# Changelog

All notable changes to `dynamic-config-s3` are documented here. The format follows
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

- **`S3::reporting_to(sink)`: a failing poll says so.** A watch loop is the
  half of a store `dynamic-config` cannot see. A delivery keeps the
  `RemoteStatus` current because `RemoteSink::apply` records one, so
  `dynamic_config_remote_up` reported the last *delivery* rather than the last
  *attempt* — and a bucket that stopped answering an hour ago looked healthy
  until something called `refresh_remote_async()`. Give the watch source the
  same sink the loop already pushes documents through and the failures
  **inside** the loop are reported as they happen: a `HEAD` that did not
  answer, and a `GET` that did not answer after the ETag moved.

  Surviving a failure is what makes this necessary. A poll loop that retries
  forever is a loop that reports nothing forever, so an expired credential, a
  bucket policy that changed under the process or a gateway that went away is
  indistinguishable from a configuration nobody has changed.

  A reported failure moves the failure streak and nothing else, so
  `dynamic_config_remote_last_fetch_seconds` keeps ageing while
  `dynamic_config_remote_up` goes to zero — the pair an alert wants: down, and
  stale for how long. Only the failure's kind and key path are recorded, so
  the bucket, the key and the endpoint stay out of it, and reporting is
  infallible: a loop must never have to handle a failure to report a failure.

  **Refusals at the door are deliberately not reported** — no format, or a
  source naming several keys. `watch()` returns those to the caller standing
  there, before there is a loop to be silent in, and they are deployment
  mistakes rather than a store that stopped answering.

  Nothing else changes: `watch()` returns what it always returned, a source
  built without `reporting_to` reports nowhere, and `fetch()` already records
  itself through `refresh_remote_async()`.

- **`S3::with_tls(&config, bucket, key, tls)`: a private certificate
  authority, as data.** The shared vocabulary from
  `dynamic-config-store-core`, for the servers where a private authority
  actually turns up: MinIO, Ceph and a company's own gateway all present
  certificates AWS' public chain has never heard of.

  **S3 cannot express a client certificate.** The SDK reaches TLS through
  `aws-smithy-http-client`, whose `TlsContext` is a trust store and nothing
  else — there is no slot to fill. mTLS is **refused**, naming the call and
  pointing at `from_client`; it is not ignored, because a caller who asked to
  present a certificate and did not would discover it as an authentication
  failure a long way from the cause.

  The CA is parsed here purely in order to refuse: the SDK's rustls
  connector calls `.expect("cert parsable")` on the material, so a
  certificate it cannot read would otherwise be a **panic** at the first
  connection rather than an error at construction.

  Two new manifest entries, `aws-smithy-http-client` and
  `rustls-pki-types`, and **no new crate in the build**: both were already
  resolved through `default-https-client`, and the feature named
  (`rustls-aws-lc`) is the one that was already on. `cargo tree` is
  unchanged.

  The escape hatch is untouched and still the answer for anything this has
  no spelling for. Where both doors reach the same slot the interaction is
  defined rather than guessed. **There is deliberately no way to turn
  verification off** — the reasoning is in `TlsConfig`'s own documentation
  and in the book.

- **`Keys`, and reading several objects as one document.** `Keys::several([..])`
  merges named keys in call order — later wins, the rule `.file(..)` already
  teaches — and `Keys::prefix("prod/")` merges the sections under a prefix,
  where an overlap between two of them is an error naming both keys and the
  paths they collided on. Every constructor takes `impl Into<Keys>` and a bare
  `&str` is still one key, so nothing that compiled before stops compiling.
  A prefix is one `ListObjectsV2` and then one `GetObject` per key; a named
  list is one `GetObject` per key. Neither is atomic and S3 offers nothing that
  would make one so — AWS made listings strongly consistent in December 2020,
  but the gap between the listing and the reads remains, and another
  implementation of this API is free not to be consistent either.
  **The 512-key bound is applied to the listing**: each page asks for one key
  more than the budget allows, so a prefix pointed at a whole bucket is refused
  after one request rather than after a million bodies. A key outside the
  prefix is refused, a key ending in `/` is the console's folder object and is
  skipped, and a listing whose continuation token never clears is given up on
  after eight pages. One unreadable key fails the whole fetch, naming it;
  provenance becomes store-grained; and a multi-key source refuses to be
  watched, because an ETag belongs to an object and a set of them has none.
- **A key list whose extensions name two formats is refused by name**, rather
  than parsed as whichever came first. `with_format` settles it.
- **`S3::with_timeout`** — the deadline for a single fetch **attempt**,
  excluding retries the SDK performs. It maps onto the SDK's
  `operation_attempt_timeout`, so with the default three attempts a five
  second timeout is a fifteen second call. That multiplier is documented in
  the README and asserted by a test rather than tuned away: a retry policy is
  a deployment's decision. The SDK sets no timeout of its own by default, so
  this is additive.

### Changed

- **The watch callback's panic net moved to `dynamic-config-store-core`**,
  an internal crate the store crates share. It was seven byte-identical
  copies; it is now one, with the same behaviour and its own test.
- **A credential the store will not accept now reports `ErrorKind::Auth`**
  rather than `ErrorKind::Remote`: `AccessDenied`, `InvalidAccessKeyId`,
  `SignatureDoesNotMatch`, `ExpiredToken`, `InvalidToken` and
  `TokenRefreshRequired`. Matched on the error code rather than the `403` that
  carries them, because `RequestTimeTooSkewed` shares that status and does
  come right once the clock does.

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

### Changed

- The tokio requirement is documented loudly (the AWS SDK is `rt-tokio`).
- Sub-250ms watch intervals sleep what was asked instead of rounding up.
- AWS floors declared honestly: `aws-config 1.6`, `aws-sdk-s3 1.79` (where
  `default-https-client` first exists).

## [0.0.1] — 2026-08-10

Initial release.

### Added

- `S3` as an `AsyncRemoteSource`: one object holds a whole configuration
  document. Works against anything speaking the API — MinIO, Ceph, R2, B2 —
  via `with_config` and path-style addressing.
- Watching by polling the ETag: each tick is a `HEAD`, only a new tag costs
  a `GET`. A key naming no format refuses at watch start.
- Credentials from the AWS chain; `from_client` for a client the program
  already has; the endpoint named in every error.

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/ctolon/dynamic-config/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ctolon/dynamic-config/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/ctolon/dynamic-config/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/ctolon/dynamic-config/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ctolon/dynamic-config/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
