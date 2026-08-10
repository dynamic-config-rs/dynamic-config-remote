# Changelog

All notable changes to `dynamic-config-consul` are documented here. The format follows
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

## [0.0.1] — 2026-08-10

Initial release.

### Added

- `Consul` as a blocking `RemoteSource`: one KV key holds a whole document,
  base64-decoded from Consul's answer.
- Change-driven watching via blocking queries, with index-reset handling,
  a wait clamped to Consul's ten-minute ceiling, and a `Watching` stop token
  honoured mid-pause.
- ACL auth: a supplied token, or a login (Kubernetes, JWT) with the session
  cached, renewed, and retried exactly once on a *typed* 403 — never for a
  supplied token, which cannot change.
- `with_datacenter`, `with_timeout`, `with_wait`, `with_agent` for an HTTP
  client the program already has.

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
