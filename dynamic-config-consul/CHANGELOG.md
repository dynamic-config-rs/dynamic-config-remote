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

## [0.6.1] — 2026-08-14

### Added

- **Every failure branch of the watch loop is in a table** in this crate's
  documentation, marked *reports* or *silent* with the reason — including the
  empty-key branch, which this crate records and two others deliberately do
  not.
- **A chaos test** (`tests/chaos.rs`, `just chaos`): a blocking query cut
  mid-watch by a toxiproxy in front of an agent that never restarts. This is
  the loop that *survives* a failure, so it is the one where the cable going
  back in has an ending worth asserting — the streak clears, the next document
  is delivered, and nobody had to call anything.

## [0.6.0] — 2026-08-13

### Added

- **`reporting_to(RemoteSink)`: a failing watch is no longer invisible.** The
  loop now records every attempt that came back with nothing on the sink its
  callback already applies documents through — a blocking query that errored,
  a watched key that holds no value, a subtree that cannot be folded into a
  document. Without it `dynamic_config_remote_up` reported the last *delivery*
  rather than the last *attempt*, so an agent that stopped answering an hour
  ago went on looking healthy until something called `refresh_remote`.

  A failed attempt moves the failure streak and nothing else: the fetch clock
  keeps ageing, so `dynamic_config_remote_last_fetch_seconds` still says how
  stale the served document is while `remote_up` goes to zero. Reporting is
  infallible and silent — a loop is never handed a failure to report a failure
  — and a source built without it records nowhere, as before. Only an
  `ErrorKind` and a key path are recorded; the agent's address never enters a
  status.

- **`with_tls(TlsConfig)`: a private certificate authority and a client
  certificate, as data.** The shared vocabulary from
  `dynamic-config-store-core`, so reaching TLS no longer means building a
  `ureq::Agent`. Consul's own agent CA — `consul tls ca create` — is exactly
  this case. All of it is expressible: a CA from a file or from bytes, mTLS
  from either. No new feature and no new dependency.

  It reaches the blocking query too, which builds its own agent with a
  longer timeout: a watch that ignored the certificate authority would be a
  watch that never connects, discovered hours after the fetch that worked.

  `with_agent` and `with_tls` together are **refused** at the first request,
  naming both calls.

  The escape hatch is untouched and still the answer for anything this has
  no spelling for. Where both doors reach the same slot the interaction is
  defined rather than guessed. **There is deliberately no way to turn
  verification off** — the reasoning is in `TlsConfig`'s own documentation
  and in the book.

- **`Keys`, and reading several keys as one document.** `Keys::several([..])`
  merges named keys in call order — later wins, the rule `.file(..)` already
  teaches — and `Keys::prefix("myapp/")` merges the sections under a prefix,
  where an overlap between two of them is an error naming both keys and the
  paths they collided on. Every constructor takes `impl Into<Keys>` and a bare
  `&str` is still one key, so nothing that compiled before stops compiling.
  A prefix is one `?recurse` request answered at one index; a named list is one
  request *per key*, because Consul's KV API has no batch read of a
  caller-chosen set, and is therefore not read atomically — the transaction
  endpoint could, at the price of a write-shaped request and a sixty-four
  operation ceiling, and that trade is recorded rather than taken. A prefix is
  capped at 512 keys, a key outside the prefix is refused, and a key ending in
  `/` with no value is a Consul folder and is skipped rather than reported as a
  missing document. One unreadable key fails the whole fetch, naming it.
  Provenance becomes store-grained.
- **A key list whose extensions name two formats is refused by name**, rather
  than parsed as whichever came first. `with_format` settles it.

- **A prefix can be watched**, and it is the cheapest correct watch on a set
  anywhere in this family because it re-reads nothing at all: a recursive
  blocking query's *answer is the subtree at that index*, so the document is
  folded from the very bytes the agent blocked to send and there is no window
  between "the set changed" and "read the set" for a second write to land in.
  A subtree that cannot be folded — two keys supplying one path, a key outside
  the prefix, more keys than the budget — ends the watch rather than being
  retried forever in silence, because none of those is a blip a retry cures. A
  **named list** still refuses at `watch()`, and now says why: Consul has no
  batch read, so the set would be blocked on one key and then read key by key.

### Changed

- **The token cache moved to `dynamic-config-store-core`**, an internal
  crate the store crates share. No behaviour change: the same margin, the
  same replacement on expiry, the same one-shot retry after a `403`, and
  the same tests. Consul has no renewal to keep, so nothing of its token
  handling is left here beyond the login itself.
- **A refused ACL token now reports `ErrorKind::Auth`** rather than
  `ErrorKind::Remote` — the `403` this crate already detected, plus a `403`
  from the `/v1/acl/login` endpoint. A caller can now tell "the policy is
  wrong" from "the agent is down" without reading the message, which is the
  difference between a watch loop that stops and one that backs off. Consul
  uses `403` and nothing else for an ACL refusal, so a `401` — which can only
  be a proxy in front of it — stays `Remote`.
- **`with_timeout` documents the semantics the whole store family shares**:
  the deadline for a single fetch attempt, excluding retries the underlying
  client performs. No behaviour change; the sentence is now the same one every
  companion crate answers to, and the README has a Timeouts section.

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
- `Debug` no longer prints credentials (`Auth`, tokens, sessions redacted).

### Changed

- Token refresh margin unified at 60s (`REFRESH_WITHIN`) across the
  token-caching store crates, with the clock-skew rationale documented.
- `base64` 0.23.

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

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/ctolon/dynamic-config/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/ctolon/dynamic-config/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ctolon/dynamic-config/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/ctolon/dynamic-config/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/ctolon/dynamic-config/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ctolon/dynamic-config/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ctolon/dynamic-config/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/ctolon/dynamic-config/releases/tag/v0.0.1
