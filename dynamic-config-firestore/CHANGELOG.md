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

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
