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

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
