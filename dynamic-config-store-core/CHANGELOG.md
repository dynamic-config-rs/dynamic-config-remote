# Changelog

All notable changes to `dynamic-config-store-core` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Before 1.0, a breaking change bumps the **minor** version and anything else
bumps the patch. A change to the minimum supported Rust version is breaking.

This crate is an implementation detail of the store crates and carries no
stable API: anything in it may change in a patch release. It is published
only because cargo will not let a published crate depend on an unpublished
one.

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

- **`tls::TlsConfig`, one TLS vocabulary for the seven store crates.** A
  custom certificate authority and a client certificate (mTLS), each as a
  file path or as PEM bytes, and no client type anywhere in a signature —
  which is what lets each store translate it into `tonic`, `ureq`, `redis`,
  `async-nats` or the AWS SDK, and what makes it expressible from a language
  that has never heard of any of them. `Pem`, `ClientCertificate` and
  `unsupported` come with it; the last is the one wording for a setting a
  store cannot express, because a silently ignored `ca_certificate` is a
  program that believes it is pinned and is not.

  `Debug` is hand-written throughout and prints shape only — a path where
  there is one, `<redacted>` where the key is bytes. A planted-key test
  covers it, and another covers the file-read error path.

  Nothing new in the dependency graph, and the 1.71 floor is unchanged: the
  module is `std` and `dynamic_config::Error`.

- **`documents`** — folding several keys into the one document `fetch`
  returns. `merged` applies one of two rules (`Overlap::LaterWins` for a
  list the caller ordered, `Overlap::Refused` for a prefix nobody ordered,
  whose refusal names both keys and the colliding paths and never a value),
  `agreed_format` catches a key list whose extensions name two formats,
  `within_key_budget` caps a prefix read at `MOST_KEYS` (512), and
  `under_prefix` refuses a key the server answered with that is not under
  the prefix asked for. etcd, Consul and Redis would each have written the
  same thing.

- **First release.** The machinery the store crates had more than one copy
  of, and nothing else.
- **`credential::Cached<T>` and `credential::Issued<T>`** — when to obtain,
  reuse and refresh a credential that expires, with `REFRESH_WITHIN` and
  `SERVICE_ACCOUNT_TOKEN` beside them. Consul, Vault and Firestore each kept
  a copy of this; the module's documentation carries the table of what the
  three agree on and what stayed in each store, which is the reconciliation
  the extraction waited on.
- **`guarded`** — running a watch callback with a panic net, previously
  seven byte-identical copies.
- **`redacted`, `redacted_list` and `LoneAuthority`** — removing a credential
  from a store URL before it reaches an error message. NATS and Redis
  differed in one documented way (what `scheme://something@host` means), and
  that is now the `LoneAuthority` argument rather than a second copy of the
  algorithm.

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/ctolon/dynamic-config/compare/v0.5.0...v0.6.0
