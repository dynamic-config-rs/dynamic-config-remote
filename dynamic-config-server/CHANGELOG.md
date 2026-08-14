# Changelog

All notable changes to `dynamic-config-server` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Before 1.0, a breaking change bumps the **minor** version and anything else
bumps the patch. A change to the minimum supported Rust version is breaking.

This crate's MSRV is **1.80**, above the workspace floor of 1.71: axum 0.8
declares 1.80, and there is no version of a server that carries no web
framework. Nothing depends on this crate, so the library's floor is
untouched.

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

## [0.6.0] — 2026-08-13

### Added

- **The client half, behind a `client` feature.** `client::ConfigServer` is a
  `RemoteSource` reading `GET /{application}/{profile}` from a config server,
  with a bearer token and — through the same
  `dynamic_config_store_core::tls::TlsConfig` every store crate takes — a
  private authority and a client certificate. Both halves live in one crate so
  that they are tested against each other: every test in `tests/client.rs`
  drives this against the real router on a real socket, rather than against a
  fixture of what the router is believed to return.

  It unwraps the server's envelope, so the engine sees the document rather
  than `{application, profile, generation, config}`, and it reads the body
  through a one-megabyte limit — a client that trusts a server to send
  something finite can be made to allocate until it dies, and the server this
  talks to is exactly what an attacker who got that far would be
  impersonating.

  A `404` is classified as `ErrorKind::Auth`, which reads oddly until you
  remember what the server means by it: the same answer for *not yours* and
  *not there*, deliberately, so that a caller cannot enumerate what it may not
  read. Neither reading is fixed by waiting, and *stop retrying* is the only
  thing the reload logic does with this.

  **It does not subscribe.** `/stream` carries a generation, and a client that
  follows it calls `refresh_remote()` when the number moves — a dozen lines
  belonging to whoever owns the reload cadence, rather than a task, a backoff
  and a reconnect policy owned by this crate.

- **TLS termination and mutual TLS, behind a `tls` feature.** A
  `[server.tls]` section names a certificate, a private key and — the one
  that matters for a config server — an optional `client_ca`. With it,
  **every caller must present a certificate that chains to that authority**
  or the handshake does not complete: a second, independent factor beside
  the bearer token, checked by rustls before a byte of HTTP exists.

  **Opt-in twice, and that is the point.** The reason this crate shipped
  without TLS was that a second TLS stack is CVE surface in the one program
  that holds every service's secrets. That cost is real, so the feature is
  off by default and a build without it contains no TLS code at all; a
  `[server.tls]` section in such a build is a refusal naming the feature
  rather than a key that is quietly ignored. What was wrong in the old
  reasoning was the other half — that every deployment already has a
  terminator. A host with no ingress does not, and a deployment that wants
  *this* socket to demand a client certificate cannot delegate it at all:
  a terminator in front can verify a certificate, but what arrives here is
  then a header, and a header is a claim.

  **A certificate is a gate, not an identity.** It does not replace the
  bearer token and does not name a caller: a valid certificate with no
  token is a 401, and a valid certificate with a token scoped to `billing`
  gets the same 404 for `payroll` as anybody else. Mapping a certificate
  subject to a principal was rejected — it hands authorisation to whoever
  holds the CA key, and creates a second roster that can silently disagree
  with the first.

  `rustls` with the `ring` provider, `tokio-rustls` for the handshake and
  hyper's own HTTP/1 connection for what follows — no OpenSSL, and no
  crate the workspace's lockfile did not already contain. The protocol
  versions and cipher suites are rustls's defaults, chosen by neither this
  crate nor its operator; ALPN is `http/1.1` and only that, because this
  build speaks nothing else. `cargo +1.80 check --all-features` passes:
  the floor did not move.

  New API: `TlsConfig`, `Tls`, `TlsError`, `serve_tls`,
  `HANDSHAKE_TIMEOUT`, `Server::tls`, `Server::posture`. The router, the
  sections and the authorisation are unchanged — only the serving line
  differs.

- **`examples/tls_mutual.rs`.** Generates a CA, a server certificate and a
  client certificate, writes the `server.toml` that names them, runs the
  server over TLS and makes the three requests that are the feature:
  certificate and token (served), certificate and no token (401), no
  certificate (refused at the handshake). No `openssl` binary, and nothing
  checked in — a repository with a private key in it has a private key in
  it.

- **`GET /{application}/{profile}/stream` — a change stream, and an event
  that is a number.** `text/event-stream`, one event per install, carrying
  the generation that landed and the two names from the URL. **Not the
  document, and not the changed paths**: the document endpoint is the one
  endpoint that serves values, and a stream carrying them would be a
  second one with a body that outlives the request which authorised it.
  The client re-fetches the endpoint it was already using.

  That decision is what makes the rest small. **Resumption is a
  comparison** — a generation is monotonic, so the current one subsumes
  every one before it, and `Last-Event-ID: 6` against a section at 9 is a
  single event carrying 9; there is no ring of recent events, so no bound
  to choose and no past-the-end case to answer. **Memory is flat** — per
  connection, one `Changes` handle, one waker and two short strings,
  nothing proportional to the document and nothing per event. **Nothing
  queues**, so backpressure needs no policy: a client that stops reading
  is not polled, and when it is polled it gets the latest generation.

  It is authenticated and authorised on the same path as everything else,
  so a subscription to a section the caller may not read is the same 404
  having done the same work, and the audit log records the subscription
  once rather than every event. `max_stream_connections` (default 4096)
  bounds how many are open at once, with a 503 and a `Retry-After` past
  it; **zero turns the endpoint off**, and it then answers like a path
  this server does not have. One new dependency, `futures-core`, for the
  `Stream` trait `Sse` needs — already in the graph under axum, and it
  does not move the 1.80 floor.
- **No OpenTelemetry SDK, written down rather than left open.** An OTLP
  exporter would mean four dependency trees and a background exporter task
  in the one program here that holds every service's secrets and whose
  posture is a small CVE surface. That is the trade TLS termination
  already lost, and to the same answer: the sidecar or ingress in front is
  where telemetry leaves the deployment. `router()` is the API, so a
  service that wants request spans, `traceparent` propagation and an
  exporter mounts it inside its own axum application — and the library's
  spans already reach OTLP through `tracing-opentelemetry` in that
  application's graph.

- **First release.** An HTTP configuration server in the spirit of Spring
  Cloud Config Server: one resolved document per application and profile,
  served under per-caller authorisation. `Server::start` loads the sections
  and `router` is the axum `Router` over them, so a service that already runs
  axum can mount the whole thing instead of running the binary.
- **Eight endpoints.** `GET /{application}/{profile}` is the document —
  values, secrets included, the handover a config server exists for. Beside
  it, `/paths` (which keys exist, no values), `/explain/{path}` (every
  layer's answer, every value `***`), `/check` (would the next load succeed,
  and where each key comes from), `/status` (generation, health, staleness),
  `/stream` (one event per install; see above), and unauthenticated
  `/healthz` and `/readyz`. Every route is a `GET`.
- **Per-application authorisation.** A bearer token is a `Principal` with an
  exact list of applications — no wildcards — so a leaked pod token reads
  that pod's section and nothing else. Tokens are compared without stopping
  at the first differing byte, and every configured token is compared even
  after one has matched.
- **Refusals, not warnings, at startup.** No clients, no sections, a token
  under 32 characters, a client with no token where `allow_anonymous` was not
  set, two anonymous clients, two clients sharing a token or a name, a grant
  naming an application nothing serves, a non-loopback `bind` without
  `insecure`, or a `bind` that is not `address:port`. Each names the key that
  would fix it and none of them prints a token.
- **It refuses to be an oracle.** A section the caller may not read and one
  nothing serves produce the same 404, with the same body, having done the
  same work — authorisation is decided from the caller's grants alone and the
  section map is never consulted for an application the caller was not
  granted.
- **An audit log that cannot carry a value.** One line per request — caller,
  application, profile, endpoint, outcome, generation — through an
  `AuditSink` a deployment can replace. The application and profile are
  recorded only after the request's shape has been checked, so a path segment
  cannot forge a line.
- **It is a user of the library, not a reimplementation.** Each served
  section is a `Dynamic<Document>`: the same loader, the same file watcher
  (nothing polls), the same last-known-good behaviour — a bad edit upstream
  leaves the previous document serving and moves `consecutive_failures` on
  `/status` and `/readyz` — and `ConfigStatus` is what `/status` renders.
- **`GET /metrics`.** The library's `telemetry::Exposition` over the same
  `ConfigStatus` that `/status` returns — one set of numbers, two shapes —
  labelled `application` and `profile`, for the sections the calling
  principal may read and no others. **Authenticated**, unlike `/healthz`
  and `/readyz`: those answer a boolean and disclose nothing, which is what
  lets them be open, while a metrics endpoint that named no section could
  not be alerted on and one that names sections enumerates every
  application the fleet configures. A scraper is a client like any other —
  a token and its own grants — and Prometheus has read `bearer_token_file`
  from a scrape configuration for years. A principal granted nothing gets a
  well-formed empty scrape rather than a refusal. `6 × sections` series per
  scrape, and no label can carry a key path, a file name or a value.

- **`whole_document` on a section**, for files that carry no header. The
  section key inside a served file is the application name, which is one
  fact rather than two — but a config server is routinely pointed at a file
  another tool writes, and such a file has no reason to carry a header this
  server invented. `whole_document = true` says the file *is* the section.
  Per section, because two sections may read files of different shapes;
  everything downstream — the environment prefix, the watcher, `/paths`,
  `/explain`, the audit log — is unchanged.
- **`Section::installed()`**, the document and a generation that is never
  ahead of it, and what `/{application}/{profile}` and `/paths` now answer
  from.
- **`DRAIN_TIMEOUT`**, the deadline on graceful shutdown, honoured by both
  serving paths; and **`HEADER_TIMEOUT`**, the deadline on request headers
  after a TLS handshake.

### Fixed

- **A response could carry the previous document under the new
  generation.** The document and the generation were two loads in the order
  that makes the pair lead rather than lag, so a reload landing between them
  labelled the old document with the new number — and a client that recorded
  it, or resumed its change stream with it, believed it had consumed an
  update whose contents it never received. The generation is read first now,
  so the worst case is a response labelled one install behind its own
  contents, which costs a client one extra fetch and loses nothing.
- **A stream resumed from a previous process could stay silent.** A
  generation counts installs since the process started, so a restart puts
  every section back at 1 while a reconnecting `EventSource` still sends the
  `Last-Event-ID` the old process gave it. The opening event was sent only
  for a *greater* generation, so a client resuming from 50 was told nothing
  until the new process had reloaded fifty times. A resumed generation the
  section is not at is news, whichever side of it it falls on.
- **SIGTERM is handled.** The shutdown future waited on Ctrl-C alone, so the
  signal a rollout actually sends fell through to the default disposition
  and killed the process outright — the graceful path never ran in the one
  situation its documentation was written for.
- **Shutdown ends.** `/{application}/{profile}/stream` is a response body
  that never finishes, so a drain that waits for every body waited for every
  subscriber to disconnect: a rollout hung on exactly the clients that were
  paying attention. Both serving paths now bound the drain by
  `DRAIN_TIMEOUT`.
- **A TLS connection has a deadline after the handshake.** The handshake
  timeout stopped at the handshake, so a client that completed one and then
  sent no request bytes — or dripped an incomplete header — held a socket
  and a task indefinitely. Hyper's header-read timeout is configured, with
  the timer it needs to be enforced.
- **A section no route could ever reach is refused at startup.** An
  application or profile with a space in it, one starting with a dot, or one
  over 64 characters passed validation, and the server started and reported
  ready while every request for it was refused by the path predicate before
  the section map was consulted. `validate()` now applies the handlers' own
  predicate, as `Refusal::UnroutableSection`.
- **`client`: a password in the URL no longer reaches a diagnostic.** A
  `user:password@` authority is refused rather than sent, but the
  description built from the URL — quoted into every error, including that
  refusal, and returned by `describe()` — was built before the refusal and
  kept the password; the hand-written `Debug` printed the same field. Both
  are redacted now.
- **`client`: the fetch deadline covers the response body.** `with_timeout`
  documented one deadline for connect, handshake, request *and* body, and
  covered everything but the body: a server that sent headers and then
  stopped writing blocked a fetch forever. One budget is started per attempt
  and every step is bounded by what remains of it.

### Security

- **`explain` is redacted unconditionally**, on every path rather than only
  the ones a schema called secret, through the library's own
  `Explanation::redacted` rather than a second copy of it. In the library an
  explanation deliberately carries values; over a socket the same answer is a
  value that has left the process for a reason nobody weighed.
- **A non-loopback bind is refused** unless this server terminates TLS
  itself or `insecure` acknowledges that something in front does. The
  refusal names both.
- **`insecure = true` together with `[server.tls]` is a refusal.** The word
  acknowledges an unencrypted socket, and with TLS there is not one.
  Without this, a configuration carrying both would keep starting after the
  TLS section was deleted — in the clear, on a public address, pre-approved
  months earlier by somebody who meant something else.
- **A private key file that anything but its owner can read refuses to
  start**, on Unix, in the same breath as a token under 32 characters and
  for the same reason. The message names the fix, including
  `defaultMode: 0400` for a Kubernetes secret volume, whose `0644` default
  is exactly the case this catches.
- **No diagnostic carries a byte of a private key.** The two errors that
  would have — a PEM that will not parse — drop their source deliberately,
  because the one useful field of a parse error is the input it choked on.
  `tests/security.rs` plants a real key and asserts it reaches no error, no
  `Debug` and no log line.
- **A refused TLS handshake is audited**, once, as
  `endpoint=tls outcome=unauthenticated`, with no caller and no subject:
  enough to see that handshakes are failing, nothing about whose.
- **`Token`'s `Debug` prints `Token(***)`**, not even the length, with a
  planted-token test over it and over everything that holds one.
- **`[server.tls] crl` is a startup refusal**, because this server checks no
  certificate revocation and will not carry a key implying that it does. The
  key is *parsed* rather than unknown so that the refusal can explain,
  instead of serde answering an operator who wants revocation with `unknown
  field`, which reads as a misspelling.

  The decision was measured against rustls rather than read off its
  documentation, and both halves of the measurement are kept executable in
  `tests/tls.rs::the_measurement_behind_refusing_revocation_still_holds`. By
  default — `ExpirationPolicy::Ignore` — a CRL whose `nextUpdate` passed in
  2020 verifies a handshake today with no error and no log line, so the
  twenty obvious lines produce a server that reports it checks revocation
  and, from whenever the file stopped being refreshed, does not. That
  version even *tests green*: the natural test, revoke a certificate and
  assert the handshake fails, passes against a six-year-old list, because
  revocation keeps working and only freshness rots. The one switch that
  refuses a stale list, `enforce_revocation_expiration`, refuses every
  clean, unrevoked client along with it — making the CA's publishing cadence
  a liveness dependency of every service's configuration fetch.

  Re-reading the file on the section watcher was tried and does not rescue
  it: a watcher fires on a **write**, and what has to be noticed here is the
  *absence* of one. Noticing that needs a clock and a periodic wake-up — the
  polling loop this server does not have and whose absence is a stated
  property of it. An HTTP distribution point is a fetch loop; OCSP is a
  second protocol and a third dependency.

  What settles it is that a certificate here is a gate, not an identity: a
  stolen one buys a TCP connection and a 401, so a CRL would revoke the
  credential that does not authorise, on a freshness schedule this server
  cannot verify, while the credential that does authorise is a line in a
  file the operator already holds. **Issue short-lived certificates and
  revoke the token.** The reasoning is in the book's threat model, under
  *Not revocation*.

[Unreleased]: https://github.com/ctolon/dynamic-config/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/ctolon/dynamic-config/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/ctolon/dynamic-config/compare/v0.5.0...v0.6.0

