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

## [0.6.1] — 2026-08-14

### Added

- **Every failure branch of the watch loop is in a table** in this crate's
  documentation, marked *reports* or *silent* with the reason. Nothing about
  the loop changed; what changed is that the question can now be answered by
  reading.

## [0.6.0] — 2026-08-13

### Added

- **`reporting_to(RemoteSink)`: a failing watch is no longer invisible.** The
  loop now records every tick that came back with nothing on the sink its
  callback already applies documents through — a metadata check that failed, a
  version that moved beside a secret that will not be read, and the v1-mount
  refusal on its way out. Without it `dynamic_config_remote_up` reported the
  last *delivery* rather than the last *attempt*, which is worst here of
  anywhere: this watch polls a version counter, so a secret nobody rewrote and
  a Vault that sealed itself yesterday deliver exactly the same nothing.

  A failed attempt moves the failure streak and nothing else: the fetch clock
  keeps ageing, so `dynamic_config_remote_last_fetch_seconds` still says how
  stale the served secret is while `remote_up` goes to zero. Reporting is
  infallible and silent — a loop is never handed a failure to report a failure
  — and a source built without it records nowhere, as before. Only an
  `ErrorKind` and a key path are recorded; the Vault's address never enters a
  status.

- **`with_tls(TlsConfig)`: a private certificate authority and a client
  certificate, as data.** The shared vocabulary from
  `dynamic-config-store-core`, so reaching TLS no longer means building a
  `ureq::Agent` — which is what a Vault behind an internal CA needs and what
  `VAULT_CACERT` already names. Vault expresses all of it: a CA from a file
  or from bytes, mTLS from either. No new feature and no new dependency;
  `ureq` already carries rustls.

  `with_agent` and `with_tls` together are **refused** at the first request,
  naming both calls: an agent is already a complete TLS configuration, so
  applying a second one could only mean discarding one of them.

  The escape hatch is untouched and still the answer for anything this has
  no spelling for. Where both doors reach the same slot the interaction is
  defined rather than guessed. **There is deliberately no way to turn
  verification off** — the reasoning is in `TlsConfig`'s own documentation
  and in the book.

  A new example, `vault_private_ca`, reads a secret from a Vault behind an
  authority the machine has never heard of.

- **`Keys`, and reading several paths as one section.** `Keys::several([..])`
  reads each path and merges them under the one section key in call order —
  later wins, the rule `.file(..)` already teaches. `Vault::new` takes
  `impl Into<Keys>` and a bare `&str` is still one path, so nothing that
  compiled before stops compiling. KV v2 has no batch read of a caller-chosen
  set, so a list is one request per path and is **not** read atomically — and
  each of those requests is a line in the audit log on every fetch, which is
  stated rather than left to be discovered. One unreadable path fails the whole
  fetch, naming it; provenance becomes store-grained; and a multi-path source
  refuses to be watched, because the version counter that watch polls belongs
  to one secret and a set of them has none.
- **There is deliberately no prefix form**, and `LIST` is not what is missing.
  A secret is a section's *contents*, so folding a subtree into one section
  would make `myapp/db` and `myapp/server` collide on `host` — the ordinary
  layout, refused — and naming a sub-section after each secret's path would
  invent a convention no other store in this family has, and would make a list
  of one path mean something different from one path. The crate documentation
  and the book carry the reasoning.

### Changed

- **The token cache moved to `dynamic-config-store-core`**, an internal
  crate the store crates share. No behaviour change: the same margin, the
  same proactive refresh, the same one-shot retry after a `403`, and the
  same tests. What stayed here is what Vault does and the other two token
  stores cannot — renewing a lease rather than logging in again.
- **A refused token now reports `ErrorKind::Auth`** rather than
  `ErrorKind::Remote` — the `403` this crate already detected, a `400` or
  `403` from the login endpoint, and a source constructed with no credentials
  at all. A caller can now tell "the policy is wrong" from "the Vault is
  sealed" without reading the message. A sealed Vault stays `Remote`, because
  it un-seals.
- **`with_timeout` documents the semantics the whole store family shares**:
  the deadline for a single fetch attempt, excluding retries the underlying
  client performs. No behaviour change; the README has a Timeouts section.

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

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/ctolon/dynamic-config/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/ctolon/dynamic-config/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ctolon/dynamic-config/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/ctolon/dynamic-config/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/ctolon/dynamic-config/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ctolon/dynamic-config/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
