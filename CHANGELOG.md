# Changelog

All notable changes to the remote stores and the config server are
documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Before 1.0, a breaking change bumps the **minor** version and anything else
bumps the patch. A change to the minimum supported Rust version is
breaking.

Ten crates on one version: each store's own changelog carries what changed
in it, and this file carries what changed across them.

**The engine is not in this repository.** It is named with a caret
(`dynamic-config = "0.6"`), so a patch release of the engine reaches these
crates without a release here — and a breaking one is picked up
deliberately, with an entry under Changed saying so.

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

- **The book has parts** — *Guide*, *The Stores*, *The Config Server* and
  *Advanced* — where the eight store chapters had been nested under *Remote
  Stores* and *Writing a Store* sat among them. No page moved file.

## [0.6.2] — 2026-08-16

### Changed

- **These crates left the engine's repository.** Eight stores, what they
  share and the config server are now
  [dynamic-config-remote](https://github.com/dynamic-config-rs/dynamic-config-remote),
  released on their own schedule. Nothing about them changed: same crate
  names, same versions, same API. What changed is the dependency on the
  engine — `=0.6.1` became `"0.6"`, so upgrading the engine no longer
  requires a release here. **That takes effect with this release**: the
  0.6.1 crates on the registry still carry the exact pin.

- **Every crate's `repository` and `homepage` point at the new home**, and
  each README's badges and book links with them. The chapters for these
  crates are now their own book, at
  [dynamic-config-rs.github.io/remote/](https://dynamic-config-rs.github.io/remote/).

[Unreleased]: https://github.com/dynamic-config-rs/dynamic-config-remote/compare/v0.6.2...HEAD
[0.6.2]: https://github.com/dynamic-config-rs/dynamic-config-remote/compare/v0.6.1...v0.6.2
