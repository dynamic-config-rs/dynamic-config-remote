# The Config Server

[`dynamic-config-server`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-server)
serves configuration over HTTP: one resolved document per
**application** and **profile**, handed to a caller that presented a
credential scoped to that application.

The URL shape is [Spring Cloud Config
Server](https://spring.io/projects/spring-cloud-config)'s, deliberately —
an operator who has run one already knows what `GET /billing/prod`
returns, and that transfer was worth more than a scheme of our own. What
else this project owes to other people's work is in
[CREDITS.md](https://github.com/ctolon/dynamic-config/blob/main/CREDITS.md).

Everything else in this workspace hands configuration to the process that
called it. This crate hands it **over a socket**, which makes it a
security boundary rather than a convenience — so the design starts at
[the threat model](config-server/threat-model.md), and the rest of this
page follows from it.

It is a *user* of the library, not a second implementation of it. Each
served section is a `Dynamic<Document>`: the same loader, the same file
watcher, the same keep-serving-the-last-good-document behaviour when an
edit upstream is bad, and the same `ConfigStatus` behind `/status`. If
the server ever needs something the library cannot do, that is a library
change.

## The endpoints

| Endpoint | Returns |
|---|---|
| `GET /{application}/{profile}` | the resolved document — **values**, secrets included |
| `GET /{application}/{profile}/paths` | which keys exist; no values |
| `GET /{application}/{profile}/explain/{path}` | every layer's answer, every value `***` |
| `GET /{application}/{profile}/check` | would the next load succeed; key paths and origins |
| `GET /{application}/{profile}/status` | generation, health, staleness |
| `GET /{application}/{profile}/stream` | one event per install, carrying a generation |
| `GET /metrics` | Prometheus text for the sections this caller may read |
| `GET /healthz` | liveness. Unauthenticated |
| `GET /readyz` | readiness. Unauthenticated |

That vocabulary maps onto this crate's: *application* is the section key,
and *profile* selects which files are read. A profile here is a
different set of files chosen by the operator rather than the library's
`profile_env` — that one is a process-wide environment variable, and a
server serving two profiles cannot have two of those at once.

Every route is a `GET`. A config server that can be written to is a
different product with a different threat model.

`check`'s `unknown` list is always empty, and that is a property of the
shape rather than a gap: unknown-key detection compares resolved keys
against a *struct's* field names, and a config server does not know its
callers' structs — which is exactly why the served document is
schemaless. A caller that wants that check runs it in its own process,
where the type is. `check` is also the one endpoint that costs I/O per
request, because re-reading the sources is the question it answers.

## One endpoint returns values

The document endpoint is the handover — that is what a config server is
*for*. Every other endpoint returns shape, provenance or counts.

`explain` is where the line is drawn wider than the library draws it. In
the library, an explanation deliberately *does* carry values: you asked,
at a terminal, for one path. Over a socket, the same answer is a value
that has left the process for a reason nobody weighed — so the server
pushes every explanation through `Explanation::redacted()`, on every
path, not only the ones a schema called secret. It is the library's own
redaction rather than a second copy of it; applying it unconditionally is
the server's decision, and it is the same one
[the CLI](cli.md) makes when it prints `***` unless asked otherwise.

```json
{
  "application": "billing",
  "profile": "prod",
  "path": "pool.max_size",
  "winner": "file",
  "rows": [
    { "layer": "file", "origin": "in /etc/config/billing.toml", "value": "***" },
    { "layer": "environment", "origin": null, "value": null }
  ]
}
```

The origin is the useful half of an explanation and is exactly the half
that is safe to show.

**The audit log carries no values either**, and cannot: an `AuditEntry`
has no field one could occupy. One line per request — caller, application,
profile, endpoint, outcome, generation — and the application and profile
are recorded only after the request's shape has been checked, because a
path segment is attacker-controlled text and a log line is
newline-delimited.

## The change stream

`GET /{application}/{profile}/stream` is `text/event-stream`, and a
client that holds it open learns of an install within a wake rather than
within a polling interval:

```text
id: 7
event: generation
data: {"application":"billing","profile":"prod","generation":7}
```

**An event is a number.** Not the document, and not the changed paths
either. The document endpoint is the one endpoint that serves values, and
a stream that carried them would be a second one — with a longer
lifetime, a different failure mode, and a body that outlives the request
that authorised it. Changed paths would disclose nothing `/paths` does
not already tell the same caller, but they would have to be diffed per
install and carried per connection, which is the memory question this
design exists not to have. So the event says *something landed, here is
its number*, and the client re-fetches the endpoint it was already using.

That one decision is what makes the three hard parts disappear:

- **Resumption is a comparison, not a buffer.** A generation is
  monotonic, so the current one subsumes every one before it.
  `Last-Event-ID: 6` against a section at 9 is a single event carrying 9.
  There is no ring of recent events — so there is no bound to choose, and
  no answer needed for a client that reconnects past the end of one. A
  `Last-Event-ID` that is not a number is ignored rather than refused: a
  proxy mangling a header must not turn a reconnect into a failure, and
  one redundant event is the cheaper mistake.
- **Memory is flat.** Per connection: one `Changes` handle — an `Arc`
  clone and a `u64` — one registered waker, and the two names from the
  URL. Nothing proportional to the document, and nothing per event. A
  thousand pods reconnecting after a restart cost a thousand of that and
  one shared install.
- **Backpressure needs no policy.** The stream carries a *level* rather
  than a log. A client that stops reading is simply not polled; when it
  is polled again it gets the latest generation, having missed nothing
  that the latest does not already say. Nothing queues, so nothing has to
  be dropped.

It is an endpoint like every other one: authenticated, authorised against
the caller's grants, and refused with the same 404 having done the same
work. The audit log records the *subscription*, once, with the generation
it opened at — a line per install per connection would drown the log that
matters, and the events say no more than a `/status` poll would.

```toml
[server]
max_stream_connections = 4096   # zero turns the endpoint off entirely
```

The ceiling is a backstop, not a rate limit: it bounds the sockets one
process will hold on this endpoint, so a client reconnecting in a loop
cannot take the process with it. Beyond it, a 503 with a `Retry-After`,
so a herd backs off rather than spins. Per-*caller* limiting stays with
the thing in front, for the same reason rate limiting does. At zero the
endpoint answers exactly like a path this server does
not have — a deployment that does not want long-lived connections says so
once, here.

**The client half is here**, behind the `client` feature:
`dynamic_config_server::client::ConfigServer` is a `RemoteSource` that
reads `GET /{application}/{profile}` with a bearer token and the same
`TlsConfig` the store crates take. Both halves live in one crate so they
are tested against each other rather than against a fixture of what each
believes the other returns — including the case that matters most, where
the server is killed mid-run and its clients keep serving from their last
known good document.

It fetches; it does not subscribe. Following this stream is a dozen lines
— read the generation, call `refresh_remote()` when it moves — and they
belong to whoever owns the reload cadence, because a task with a backoff
and a reconnect policy is not a choice this crate should make on an
application's behalf.

## Metrics, and why they need a credential

`GET /metrics` is the library's [telemetry](telemetry.md) rendering of
the same `ConfigStatus` that `/status` returns — one set of numbers, two
shapes — labelled `application` and `profile`, for the sections the
calling principal may read and no others. Six families, `6 × sections`
series per scrape.

It takes the same bearer token as everything else. `/healthz` and
`/readyz` are open because they answer a boolean and say nothing else:
not how many sections there are, not which one is unhappy. That is
exactly what lets them be open, and it is exactly what a useful metrics
endpoint cannot do — a series that cannot name the section it describes
is a series nobody can alert on. An open `/metrics` would enumerate every
application the fleet configures to anyone who could reach the port,
which is the [not-an-oracle](config-server/threat-model.md) property
undone by a scrape.

So a scraper is a client like any other: give it a token and grant it the
applications it should see. Prometheus has read `authorization` and
`bearer_token_file` from its scrape configuration for years, so this
costs a deployment two lines. A principal granted nothing gets a
well-formed empty scrape rather than a refusal — it is somebody, and
there is nothing to tell it.

The alternative — an open endpoint with no labels, counting sections in
aggregate — was rejected: it says less than `/readyz` already does and
still cannot be alerted on.

**No label can carry a key path, a file name or a value.** Every sample
comes from a `ConfigStatus`, which holds none of them, and the two labels
this server adds are an application and a profile that its own
configuration named and that the request-shape check already bounds.

## TLS, and the client certificate that goes with it

The server terminates TLS when it is asked **twice**: the `tls` Cargo
feature, and a `[server.tls]` section. Neither alone does anything, and a
`[server.tls]` section in a build without the feature refuses to start
rather than being ignored — a key that is silently dropped is a port an
operator believes is encrypted.

```toml
[server]
bind = "0.0.0.0:8443"

[server.tls]
certificate = "/etc/dynamic-config/server.pem"   # leaf first, then intermediates
key = "/etc/dynamic-config/server.key"           # PKCS#8, PKCS#1 or SEC1
client_ca = "/etc/dynamic-config/clients-ca.pem" # optional; see below
```

```sh
cargo install dynamic-config-server --features tls
```

**Off by default is the point.** The reason this crate shipped without
TLS was that a second TLS stack is CVE surface in the one program that
holds every service's secrets, and that reason has not stopped being
true — it has stopped being the *only* thing that is. A deployment with
a terminator in front installs the same binary it always did and links
no TLS at all; a deployment that needs its own socket encrypted, or that
wants a client certificate, turns on a feature. The
[threat model](config-server/threat-model.md#tls-and-why-it-is-opt-in)
carries the full reasoning, including what TLS here does *not* protect.

Only the serving line changes. The router, the sections, the tokens and
the grants are identical:

```rust,ignore
serve_tls(listener, router(Arc::clone(&server)), &server, shutdown).await?;
```

The protocol versions and cipher suites are rustls's defaults and the
`ring` provider's, chosen by neither this crate nor its operator: a
hand-picked suite list is one crate's opinion frozen on the day it was
written. The one thing set here is ALPN — `http/1.1`, and only that,
because this build speaks nothing else and a client that negotiated `h2`
would fail *after* the handshake instead of during it.

### `client_ca`: mutual TLS

With `client_ca` set, every caller must present a certificate that chains
to it or the handshake does not complete — there is no third state where
a certificate is asked for and accepted missing, because a certificate
whose absence is tolerated is decorative.

**A certificate is a gate, not an identity.** It does not replace the
bearer token and it does not name a caller: a caller with a valid
certificate and no token gets a 401, and a caller with a valid
certificate and a token scoped to `billing` gets the same 404 for
`payroll` as anybody else. The alternative — mapping a certificate
subject to a principal — was rejected because it hands authorisation to
whoever holds the CA key, who is usually a platform team rather than
whoever maintains this server's roster. The
[threat model](config-server/threat-model.md#what-a-client-certificate-is-and-what-it-is-not)
argues it in full.

A refused handshake is recorded once, as
`endpoint=tls outcome=unauthenticated`, with no caller and no subject:
enough for an operator to see that handshakes are failing, and nothing
about whose.

**No revocation is checked**, so a certificate that chains here is good
until it expires — issue short-lived ones. A `crl` key is a startup
refusal rather than a file this server reads, and the reasoning, which
was measured against rustls rather than assumed, is in [Not
revocation](config-server/threat-model.md#not-revocation). The credential
this server can withdraw is the bearer token.

### The private key

Two rules, both enforced rather than advised:

- **A key file that anything but its owner can read refuses to start**
  on Unix. The message names the fix. A Kubernetes secret volume mounts
  `0644` by default, so this is a `defaultMode: 0400` away — which is
  what the message says.
- **No diagnostic ever carries a byte of it.** The errors that would
  have — a PEM that will not parse — drop their source deliberately and
  report a path and a sentence instead.

### Running the whole thing

```sh
cargo run -p dynamic-config-server --features tls --example tls_mutual
```

It generates a CA, a server certificate and a client certificate into a
temporary directory, writes the `server.toml` that names them, starts the
server, and makes three requests: with a certificate and a token
(served), with a certificate and no token (401), and with no certificate
(refused at the handshake). Nothing is checked in and no `openssl`
binary is involved — a repository with a private key in it has a private
key in it, whatever the README says about the key being a test one.

## Configuring it

The server's own configuration is a `[server]` section, read — of course
— with `dynamic-config`:

```toml
[server]
bind = "127.0.0.1:8080"
watch_debounce_ms = 250

[[server.sections]]
application = "billing"
profile = "prod"
files = ["/etc/config/billing.toml", "/etc/config/billing-prod.toml"]

[[server.sections]]
application = "billing"
profile = "staging"
files = ["/etc/config/billing.toml", "/etc/config/billing-staging.toml"]

[[server.clients]]
name = "billing-pod"
token = "a-token-of-at-least-32-characters"
applications = ["billing"]
```

```sh
dynamic-config-server /etc/dynamic-config-server.toml
```

The section key *inside* those files is the application name: what is
served as `billing` is the `[billing]` table. One fact rather than two,
and it keeps a URL and a file readable against each other.

A file that carries no such header — one another tool writes, whose whole
contents are the configuration — is read by saying so on that section:

```toml
[[server.sections]]
application = "billing"
profile = "prod"
files = ["/etc/config/billing.json"]   # {"host": "…", "port": 8080}
whole_document = true
```

Per section, because two sections may read files of different shapes.
Everything downstream is unchanged — the environment prefix, the watcher,
`/paths`, `/explain` and the audit log. See
[Document Shape](document-shape.md).

Unknown keys in this file are refused rather than ignored. A misspelled
key in a security-relevant file is a key the operator believes is doing
something.

## Embedding it

The binary is a thin `main`. The router is the API, so a service that
already runs axum can mount it:

```rust,ignore
use std::sync::Arc;
use dynamic_config_server::{router, Server, ServerConfig};

let server = Arc::new(Server::start(&config)?);
let listener = tokio::net::TcpListener::bind(server.address()).await?;

axum::serve(listener, router(server)).await?;
```

`Server::start_with` takes an `AuditSink`, which is where a deployment
sends its audit trail when stderr is not it.

## Nothing polls

A section reloads because the library's file watcher noticed, and
`/status` is a handful of atomic loads with no I/O — so an idle server
costs no CPU however many sections it holds, and a scrape per second
costs nothing either.

`watch_debounce_ms = 0` disables watching, for a deployment that reloads
by restarting.

## When a source goes bad

The previous document keeps serving — that is the point of fronting a
store — and the server says so where a pipeline will see it:
`consecutive_failures` moves on `/status`, and `/readyz` answers 503.
Callers see no outage; the deployment sees the problem before the next
restart turns "stale but working" into "will not start".

A section that will not load at **startup** is fatal instead. A config
server that comes up serving nothing for one application is a silent
outage for whoever needed it.

## What is not here

Named, because a config server invites all of it and the line has to be
somewhere:

- **An OpenTelemetry SDK.** An OTLP exporter means four dependency trees
  and a background exporter task in the one program here that holds every
  service's secrets, whose whole posture is a small CVE surface. It is the
  same trade [TLS](config-server/threat-model.md#tls-and-why-it-is-opt-in)
  makes and the opposite answer, and the difference is what the
  dependency buys: TLS off the default build is one feature away for the
  deployment that needs it, and an exporter here would buy a deployment
  nothing its sidecar is not already giving it. The router is the API, so
  a service that wants request spans and `traceparent` propagation mounts
  it inside its own axum application, where both are its own choice — and
  the library's own spans already reach OTLP through
  `tracing-opentelemetry` in that application's graph.
- **JWT credentials.** One credential shape, complete, beats two with one
  tested. A client certificate is not a second shape: it is a gate in
  front of the same one — and mutual TLS shipped without touching the
  `Authenticator` seam at all, which is the evidence that the seam is
  under no pressure. A second shape would also be the first thing here
  able to grant an application from outside this server's own roster,
  which is precisely the design the [threat
  model](config-server/threat-model.md#what-a-client-certificate-is-and-what-it-is-not)
  rejects for certificates.
- **Certificate revocation.** `client_ca` configures no CRL and checks
  none, and a `tls.crl` key is a startup refusal rather than a file this
  server reads: by default rustls uses a CRL whose `nextUpdate` passed
  years ago without a word, and the switch that refuses a stale one
  refuses every valid client with it. Issue short-lived certificates and
  revoke the token. The measurement and the full reasoning are in [Not
  revocation](config-server/threat-model.md#not-revocation).
- **Rate limiting.** It belongs to the thing in front, which is the only
  place that sees every replica's share of one caller, and is also where
  a fleet-wide restart is best absorbed. `max_stream_connections` is not
  a substitute and does not pretend to be: it bounds one process's
  sockets on one endpoint. A real limiter is per caller, needs a clock
  and a store to age entries out, and needs a `Retry-After` on its 429 —
  none of which this server has, and all of which the thing in front
  already does.
- **Labels** (`/{application}/{profile}/{label}`). Not for want of a git
  store — `dynamic-config-git` landed, and it can name a branch, a tag or
  a revision. The problem is the coordinate: a label is chosen by the
  *caller*, so it names a resolution this server has not done, on the
  request path, over a key space no grant bounds — where every served
  section today is resolved once at startup and reloaded by a watcher.
  Wanting two refs served at once is two `[[sections]]` entries, which is
  static, bounded, authorised by the grants that already exist, and
  answers the same question without a new URL segment.
- **A container image or a compose file.** Packaging rather than code.
  The binary takes one argument and reads one file, and a base image is a
  thing that has to be patched on somebody's schedule — which should be
  the schedule of whoever operates it, not a release cadence of this
  workspace's.

MSRV 1.80 — above the workspace floor of 1.71, because axum 0.8 declares
1.80 and there is no version of a server that carries no web framework.
Nothing depends on this crate, so the library's floor is untouched. The
`tls` feature does not move it either: rustls 0.23 and tokio-rustls 0.26
both declare 1.71, and `cargo +1.80 check --all-features` passes.
