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

## [0.1.0] — 2026-08-10

### Breaking

- A watch callback that panics ends the watch with an error instead of
  unwinding through the caller's task.

### Fixed

- An expired auth token *during* the watch stream re-logs-in and
  re-establishes the stream (bounded), instead of failing terminally —
  previously only the initial connection recovered.

## [0.0.1] — 2026-08-10

Initial release.

### Added

- `Etcd` as an `AsyncRemoteSource`: one key holds a whole configuration
  document; the format comes from the key's extension or `with_format`.
- A real push watch over etcd's gRPC stream — no polling.
- Credentials and TLS through `etcd-client`'s own `ConnectOptions`
  (re-exported), `from_client` for a client the program already has, and
  `tls` / `tls-roots` features.

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
