# Changelog

All notable changes to `dynamic-config-vault` are documented here. The format follows
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

## [0.4.0] — 2026-08-12

## [0.3.0] — 2026-08-11

## [0.2.0] — 2026-08-11

### Changed

- Released in lockstep with `dynamic-config` 0.2.0, where the attribute
  declares and the builder configures. This crate's own surface is
  unchanged; its examples and docs now configure through the builder.

## [0.1.0] — 2026-08-10

### Breaking

- `Debug` no longer prints credentials (tokens, AppRole secret ids,
  passwords, JWTs redacted; mounts/roles/usernames still shown).
- A watch callback that panics ends the watch with an error.

### Changed

- Token refresh margin unified at 60s (`REFRESH_WITHIN`).

## [0.0.1] — 2026-08-10

Initial release.

### Added

- `Vault` as a blocking `RemoteSource` over KV v2: the secret's fields become
  the configuration section, wrapped under the type's key.
- Watching by polling the *metadata* version counter — the secret itself is
  read, decrypted and audit-logged only when the version moves. A v1 mount
  ends the watch with an error instead of never firing.
- Auth: token, AppRole, Kubernetes, JWT/OIDC, userpass, LDAP, TLS
  certificate; tokens cached, renewed before expiry, and retried exactly
  once on a *typed* 403 — never for a supplied token.
- `with_namespace`, `with_timeout`, `with_agent`; one HTTP client per
  source, not per request.

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/ctolon/dynamic-config/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/ctolon/dynamic-config/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ctolon/dynamic-config/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
