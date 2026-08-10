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

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
