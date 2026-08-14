//! An HTTP configuration server for
//! [dynamic-config](https://docs.rs/dynamic-config): one resolved document
//! per application and profile, served over HTTP under per-caller
//! authorisation.
//!
//! The client half already existed — a service that consumes a config server
//! is one more `RemoteSource` — so what is new here is the server, and a
//! server is a different kind of artefact from everything else in this
//! workspace. Every library in it hands configuration to the process that
//! called it. This one hands configuration **over a socket**, which makes it
//! a security boundary, and the design below follows from that rather than
//! the other way round.
//!
//! # The threat model
//!
//! A config server holds every service's configuration, so an authentication
//! mistake here is every secret at once. Three sentences:
//!
//! 1. **Nothing is readable without a credential, and a credential is scoped
//!    to applications rather than to the server** — a leaked pod token
//!    reads that pod's section and nothing else.
//! 2. **The server refuses to start** rather than start permissively: no
//!    clients, a token under 32 characters, a client with no token where
//!    `allow_anonymous` was not set, or a non-loopback bind where `insecure`
//!    was not set are each a refusal that names the key that would fix it.
//! 3. **It will not be an oracle**: a caller not granted an application
//!    cannot learn whether that application exists — same status, same body,
//!    same work — and the only values that leave the process do so through
//!    the one endpoint whose job that is.
//!
//! What it is *not* defending: the store behind it. This server is a cache
//! and a fan-out, not an authority. It does not sign what it serves, and a
//! client that needs provenance it can verify should verify it at the store.
//!
//! # What each endpoint returns
//!
//! | Endpoint | Returns |
//! |---|---|
//! | `GET /{application}/{profile}` | the resolved document — **values**, secrets included |
//! | `GET /{application}/{profile}/paths` | which keys exist; no values |
//! | `GET /{application}/{profile}/explain/{path}` | every layer's answer, every value `***` |
//! | `GET /{application}/{profile}/check` | would the next load succeed; key paths and origins |
//! | `GET /{application}/{profile}/status` | generation, health, staleness; numbers and timestamps |
//! | `GET /{application}/{profile}/stream` | `text/event-stream`: one event per install, carrying a generation |
//! | `GET /metrics` | Prometheus text for the sections this caller may read |
//! | `GET /healthz` | liveness. Unauthenticated, and says nothing else |
//! | `GET /readyz` | readiness. Unauthenticated, and says nothing else |
//!
//! One row of that table returns values. The rest cannot, by construction
//! rather than by care — the stream included, which is the row where it
//! would have been easiest to lose: `explain` goes through
//! [`Explanation::redacted`](dynamic_config::Explanation::redacted) — the
//! library's own machinery, not a second copy of it — on **every** path
//! rather than only the ones a schema called secret, and `check`, `status`
//! and `paths` are built from types the library already guarantees carry no
//! values.
//!
//! **The audit log contains no values either**, and cannot: an
//! [`AuditEntry`] has no field one could occupy. See [`audit`].
//!
//! # Metrics
//!
//! `/metrics` is the library's [`telemetry`](dynamic_config::telemetry)
//! rendering of the same [`ConfigStatus`](dynamic_config::ConfigStatus)
//! that `/status` returns — one set of numbers, two shapes — labelled with
//! the application and the profile and with nothing else. Six families,
//! `6 × sections` series per scrape; no key path, file name or value can
//! be a label, because nothing a label is built from holds one.
//!
//! **It is authenticated, and scoped to the caller's grants.** `/healthz`
//! and `/readyz` are open because they answer a boolean and disclose
//! nothing; a metrics endpoint that could say as little would be no use,
//! and one that names sections is an enumeration of every service the
//! fleet configures. A scraper is a client like any other — Prometheus
//! reads a bearer token from its scrape configuration — and it sees exactly
//! the applications it was granted.
//!
//! # The change stream
//!
//! `GET /{application}/{profile}/stream` is `text/event-stream`, one event
//! per install:
//!
//! ```text
//! id: 7
//! event: generation
//! data: {"application":"billing","profile":"prod","generation":7}
//! ```
//!
//! **A number, not a document and not a diff.** That one decision is what
//! makes the endpoint small enough to be safe:
//!
//! - **Resumption is a comparison.** A generation is monotonic, so the
//!   current one subsumes every one before it. `Last-Event-ID: 6` against a
//!   section at 9 is one event carrying 9 — there is no ring of recent
//!   events, so no bound to pick and no "reconnected past the end of it"
//!   case to answer.
//! - **Memory is flat.** Per connection: one `Changes` handle, one
//!   registered waker, two short strings. Nothing proportional to the
//!   document, and nothing per event. A fleet-wide restart costs one of
//!   those per pod and one shared install.
//! - **Backpressure needs no policy.** The stream carries a level rather
//!   than a log: a client that stops reading is not polled, and when it is
//!   polled again it gets the *latest* generation. Nothing queues, so
//!   nothing has to be dropped.
//!
//! It is authenticated and authorised exactly as every other endpoint is,
//! and a subscription to a section the caller may not read is the same 404
//! having done the same work. `max_stream_connections` bounds how many are
//! open at once — the excess gets a 503 with a `Retry-After`, and **zero
//! turns the endpoint off**, whereupon it answers like a path this server
//! does not have.
//!
//! # The other half
//!
//! Behind the `client` feature, [`client::ConfigServer`] is a
//! [`RemoteSource`](dynamic_config::RemoteSource) that reads
//! `GET /{application}/{profile}` from a server like this one, with a bearer
//! token and the same `TlsConfig` the store crates take. Both halves live in
//! one crate so they are tested against each other rather than against a
//! fixture of what each believes the other returns — `tests/client.rs`
//! drives the source at the real router on a real socket, including the case
//! that matters most, where the server is killed mid-run and its clients go
//! on serving from their last known good document.
//!
//! It fetches; it does not subscribe. Following [the change
//! stream](#the-change-stream) is a dozen lines belonging to whoever owns
//! the reload cadence, and a task with a backoff and a reconnect policy is
//! not something this crate should choose on an application's behalf. See
//! [`client`].
//!
//! # Observability
//!
//! Two surfaces, and **no OpenTelemetry SDK**.
//!
//! [`/metrics`](#metrics) is the numbers; [`AuditSink`] is the record of who
//! read what. Both are this crate's own, and the second is a trait rather
//! than a `tracing` call precisely so a deployment can put its audit trail
//! somewhere other than stderr.
//!
//! An OTLP exporter would mean `opentelemetry`, `opentelemetry-otlp`,
//! `tracing-opentelemetry` and a gRPC or HTTP client — four dependency
//! trees and a background exporter task — in the one program in this
//! workspace that holds every service's secrets and whose stated posture is
//! a small CVE surface: axum with three features, no multipart, no
//! websockets, and no TLS stack unless a deployment asked for one. It is
//! the same trade [TLS](#tls) makes and the opposite answer, because the
//! two differ in what the dependency *buys*: TLS off the default build is
//! still available to the deployment that needs it, one feature away, and
//! an exporter here would buy a deployment nothing its sidecar is not
//! already giving it.
//!
//! The library side of it costs nothing and is already done — the spans
//! `dynamic-config` emits reach OTLP through `tracing-opentelemetry` in the
//! *application's* dependency graph. [`router`] is the API, so a service
//! that wants request spans, `traceparent` propagation and an exporter
//! mounts this router inside its own axum application, where those are its
//! own choices, and gets all three without this crate depending on any of
//! them.
//!
//! # Authentication
//!
//! One credential shape: a bearer token in `Authorization`, compared without
//! stopping at the first differing byte, against a roster in the server's
//! own configuration. A client certificate is **not** a second credential
//! shape — see [TLS](#tls) — and JWT validation is *absent* rather than
//! sketched.
//!
//! Anonymous access exists and needs two switches thrown: a client with no
//! `token`, and `allow_anonymous = true`. It is then a principal like any
//! other, with its own grants, so "open for development" still cannot mean
//! "open to everything".
//!
//! # TLS
//!
//! **Opt-in twice**: the `tls` Cargo feature, and a `[server.tls]` section.
//! Neither alone does anything, a `[server.tls]` section in a build without
//! the feature is a refusal rather than a key that is quietly ignored, and a
//! build without the feature contains no TLS code at all — which is the
//! honest half of the reasoning this crate used to record as "no TLS ever":
//! a deployment that already terminates TLS in front keeps exactly the
//! dependency graph it had, and the CVE surface it did not want stays out of
//! it.
//!
//! ```toml
//! [server.tls]
//! certificate = "/etc/dynamic-config/server.pem"
//! key = "/etc/dynamic-config/server.key"
//! client_ca = "/etc/dynamic-config/clients-ca.pem"   # optional
//! ```
//!
//! `client_ca` is the one that matters for a config server. With it,
//! **every caller must present a certificate that chains to it** or the
//! handshake does not complete — a second, independent factor beside the
//! bearer token, checked before a byte of HTTP exists. Without it, the
//! server authenticates itself to callers and asks for nothing back.
//!
//! **A certificate is a gate, never an identity.** It is not an alternative
//! to the bearer token, and it does not name a caller: the token is still
//! what produces a [`Principal`], what the grants hang off and what the
//! audit log records. The reasoning, and the two rejected designs, are in
//! [`tls`].
//!
//! Two refusals keep the matrix coherent. A non-loopback `bind` with neither
//! TLS nor `insecure` is refused as before; and `insecure = true` *with*
//! TLS is refused too, because the word acknowledges an unencrypted socket
//! and there is not one — leaving it set would mean that deleting the TLS
//! section later reopened the port in the clear instead of refusing.
//!
//! **The private key is the sharpest secret this program handles.** Its
//! bytes reach no log, no error, no `Debug` and no audit line: the two
//! errors that would have carried them — a PEM that will not parse — carry a
//! path and a sentence instead. And a key file that anything but its owner
//! can read is a startup refusal on Unix, for the same reason a token under
//! 32 characters is one.
//!
//! [`serve_tls`] is the serving half, and [`router`] is unchanged: nothing
//! about authorisation moves because a connection is encrypted.
//!
//! # It is a user of the library, not a reimplementation
//!
//! Each served section is a [`Dynamic<Document>`](dynamic_config::Dynamic):
//! the same loader, the same file watcher, the same
//! keep-serving-the-last-good-document behaviour when an edit upstream is
//! bad, and the same [`ConfigStatus`](dynamic_config::ConfigStatus) behind
//! `/status`. Nothing polls — a section reloads because the watcher said so
//! — and `/status` is a handful of atomic loads, so an idle server costs no
//! CPU however many sections it holds.
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use dynamic_config_server::{router, Server, ServerConfig};
//!
//! # async fn run(config: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
//! let server = Arc::new(Server::start(&config)?);
//! let listener = tokio::net::TcpListener::bind(server.address()).await?;
//!
//! axum::serve(listener, router(server)).await?;
//! # Ok(())
//! # }
//! ```
//!
//! With a `[server.tls]` section, the same three lines with the serving one
//! swapped — the router, the sections and the authorisation are identical,
//! which is the point:
//!
//! ```no_run
//! # #[cfg(feature = "tls")]
//! # async fn run(config: dynamic_config_server::ServerConfig)
//! #     -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::Arc;
//!
//! use dynamic_config_server::{router, serve_tls, Server};
//!
//! let server = Arc::new(Server::start(&config)?);
//! let listener = tokio::net::TcpListener::bind(server.address()).await?;
//!
//! serve_tls(
//!     listener,
//!     router(Arc::clone(&server)),
//!     &server,
//!     async { let _ = tokio::signal::ctrl_c().await; },
//! )
//! .await?;
//! # Ok(())
//! # }
//! ```
//!
//! `cargo run -p dynamic-config-server --features tls --example tls_mutual`
//! is the whole thing end to end: it generates a CA, a server certificate
//! and a client certificate, starts the server over TLS, presents the
//! client certificate, and then shows what a caller without one gets.
//!
//! The server's own configuration is TOML, JSON or YAML, read — of course —
//! with `dynamic-config`:
//!
//! ```toml
//! [server]
//! bind = "127.0.0.1:8080"
//!
//! [[server.sections]]
//! application = "billing"
//! profile = "prod"
//! files = ["/etc/config/billing.toml", "/etc/config/billing-prod.toml"]
//!
//! [[server.clients]]
//! name = "billing-pod"
//! token = "a-token-of-at-least-32-characters"
//! applications = ["billing"]
//! ```
//!
//! The section key *inside* those files is the application name: what is
//! served as `billing` is the `[billing]` table. One fact rather than two.
//!
//! # What is not here
//!
//! Named, because a config server invites all of it and the line has to be
//! somewhere:
//!
//! - **An OpenTelemetry SDK.** This crate carries none, deliberately: see
//!   [Observability](#observability).
//! - **JWT credentials.** One credential shape, complete, beats two with one
//!   tested. A client certificate is not a second one: it is a gate in front
//!   of the same one — and mutual TLS shipped without touching the
//!   [`Authenticator`] seam, which is the evidence that seam is not under
//!   any pressure. A second shape would also be the first thing here able to
//!   grant an application from outside this server's own roster, which is
//!   the design [`tls`] rejects at length.
//! - **Certificate revocation.** `client_ca` configures no CRL and checks
//!   none, so a client certificate is valid until it expires — and
//!   `[server.tls] crl` is a **startup refusal** rather than a key that is
//!   read. Measured, not assumed: rustls accepts a CRL whose `nextUpdate`
//!   passed years ago without a word, and the one switch that refuses a
//!   stale list refuses every client with it, so the choice is between a
//!   check that stops checking and an outage that arrives with the CA's next
//!   hiccup. Issue short-lived certificates and revoke the bearer *token* —
//!   which this server can withdraw by removing a line. See
//!   [`Refusal::RevocationUnsupported`] and
//!   `tests/tls.rs::the_measurement_behind_refusing_revocation_still_holds`.
//! - **Rate limiting.** Belongs to the thing in front: it is the only place
//!   that sees every replica's share of one caller, and it is where a
//!   fleet-wide restart is best absorbed. `max_stream_connections` is not it
//!   and does not pretend to be — it bounds one process's sockets on one
//!   endpoint.
//! - **Labels (`/{application}/{profile}/{label}`).** Not for want of a git
//!   store — that landed. A label is a coordinate the *caller* picks, so it
//!   is a resolve the server has not done, on the request path, over a key
//!   space no grant bounds. Two refs wanted at once is two `[[sections]]`,
//!   which is static, bounded and already authorised.
//! - **Writing configuration.** Every route is a `GET`. A server that could
//!   be written to is a different product with a different threat model.
//! - **A container image or a compose file.** Packaging rather than code:
//!   the binary takes one argument and reads one file, and a base image is a
//!   thing to patch on somebody's schedule rather than this crate's.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(clippy::must_use_candidate)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod audit;
pub mod auth;
#[cfg(feature = "client")]
#[cfg_attr(docsrs, doc(cfg(feature = "client")))]
pub mod client;
mod config;
mod document;
mod routes;
#[cfg(feature = "tls")]
mod serve;
mod server;
#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
pub mod tls;

/// How long a connection has to finish once shutdown has been asked for.
///
/// Graceful shutdown means finishing the request in flight — and one of the
/// requests this server serves, `/{application}/{profile}/stream`, is a
/// response body that never ends by design. Waiting on every body would mean
/// waiting for every subscriber to disconnect, so a rollout would hang on
/// exactly the clients that were paying attention.
///
/// So the drain has a deadline, and both serving paths hold to it. A
/// subscriber loses its stream and reconnects, which is what an
/// `EventSource` does by itself and what the `Last-Event-ID` rules exist
/// for; a fetch genuinely in flight has thirty seconds, which is far longer
/// than any endpoint here takes.
pub const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub use audit::{AuditEntry, AuditSink, NoAudit, Outcome, StderrAudit};
pub use auth::{Authenticator, Principal, Token, MIN_TOKEN_LEN};
pub use config::{ClientConfig, Refusal, SectionConfig, ServerConfig, TlsConfig};
pub use document::Document;
pub use routes::router;
#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
pub use serve::{serve_tls, HANDSHAKE_TIMEOUT, HEADER_TIMEOUT};
pub use server::{Section, Server, StartupError, StreamPermit};
#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
pub use tls::{Tls, TlsError};

/// This crate's version, for a deployment that has to say which server it is
/// running.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
