# Threat Model

A config server holds **every service's configuration**, which means an
authentication mistake is every secret at once. This page is where the
crate's design starts; the [endpoints](../config-server.md) follow from
it rather than the other way round.

## Who may read what

Nothing, by default. A caller presents a credential and is authorised
**per application**, not per server. That is the decision everything else
turns on: a credential scoped to the server would make one leaked pod
token equal to every secret in the fleet, and the blast radius of a leaked
credential has to be the pod's own section.

Grants are exact — no wildcards, no prefixes. A grant language is a place
to make a mistake that reads as a working deployment, and nothing here
needs one.

## What a credential is

A bearer token in `Authorization`, and only that. A client certificate
is **not** a second credential shape — see [What a client certificate
is](#what-a-client-certificate-is-and-what-it-is-not) — and JWT
validation is *absent* rather than sketched. Two credential shapes with
one of them tested is a worse posture than one shape done completely:
the one nobody exercises is where the bug lives, and this is the crate
where that bug is expensive.

Tokens are compared without stopping at the first differing byte, so the
time a comparison takes does not reveal how much of a guess was right,
and every configured token is compared even after one has matched, so it
does not reveal *which* client is calling either. What remains visible is
a token's **length**, which is fixed by whoever issued it, is not the
secret, and is bounded below by 32 characters.

A header that is *present but unusable* — a wrong scheme, an unknown
token — is nobody, not the anonymous caller. A caller that presented a
credential meant to present that credential, and silently serving it the
anonymous grants instead is how an expired token becomes a deployment
that appears to work.

## Refusing to start

The failure this crate exists to prevent is a server that starts, looks
healthy, and is serving `billing` to anyone who asks. So the checks are
refusals rather than warnings, and each one names the key that would fix
it:

| Refused | Because |
|---|---|
| no `clients` | nothing could ever be read; almost certainly a truncated file |
| no `sections` | the server would serve nothing at all |
| a `token` under 32 characters | a four-character token is no authentication |
| a client with no `token`, without `allow_anonymous` | an omitted token must never be the accident that opens a server up |
| two clients with no `token` | there is one anonymous caller, so it has one set of grants |
| two clients sharing a `token` | the first listed would silently win, and the audit log would name the wrong caller |
| two clients sharing a `name` | a name identifies a caller in the audit log |
| two sections claiming one application and profile | one pair, one section |
| a grant naming an application no section serves | a typo that reads as a working deployment right up to the first 404 |
| a non-loopback `bind` with neither `tls` nor `insecure` | see below |
| a `bind` that is not `address:port` | which of a hostname's addresses a server lands on is not a thing to discover at startup |
| `insecure` together with `[server.tls]` | the word acknowledges an unencrypted socket, and there is not one |
| `[server.tls]` in a build without the `tls` feature | a key that is silently ignored is a port an operator believes is encrypted |
| a `tls.key` that is readable by more than its owner | a key anybody on the host can read is not a key |
| a `tls.key` that is not the certificate's key | otherwise the first handshake finds out, which looks like a client problem |
| a `tls.client_ca` that is not a certificate authority | otherwise the server starts and rejects every client, which looks like a client problem too |
| a `tls.crl` | this server checks no revocation, and a key it would ignore is a check an operator believes is happening — see [Not revocation](#not-revocation) |

**Anonymous access needs two switches thrown**: a client with no `token`,
and `allow_anonymous = true`. It is then a principal like any other, with
its own grants — so "open for development" still cannot mean "open to
everything".

## TLS, and why it is opt-in

This server can terminate TLS, and does not unless it is asked twice: the
`tls` Cargo feature, and a `[server.tls]` section. **Neither alone does
anything**, and a `[server.tls]` section in a build without the feature
is a refusal rather than a key that is quietly ignored.

This page used to say the opposite — that a second TLS stack doubles the
CVE surface of a program whose job is holding other people's secrets, and
that every deployment already has a terminator. The first half is a real
cost and is why the feature is off by default: a deployment that
terminates in front keeps exactly the dependency graph it had, and the
binary it installs contains no TLS code at all. The second half was never
a fact about deployments, only about the ones that had been looked at.
Two shapes it was wrong about:

- **A server on a machine with no ingress.** A VM, a bare-metal host, a
  developer's cluster. "Put a terminator in front of it" is a second
  program to run, configure and patch, in front of a program that already
  links a TLS client for its own stores.
- **A deployment that wants the config server's own socket to demand a
  client certificate.** That one cannot be delegated at all: a terminator
  in front can verify a certificate, but what reaches this process is
  then a plain HTTP request that says a header, and this server has no
  way to tell the difference between a terminator's assertion and a
  caller's claim. If the certificate is to mean anything here, the
  handshake has to happen here.

The cost, stated rather than implied: with the feature on, this process
links rustls, `ring` and webpki, and their advisories become this
server's advisories. What makes that a smaller step than it sounds is
that all three are already in this workspace's dependency graph — the
NATS and S3 stores pull them — so the feature adds no crate the lockfile
did not already have.

### What it does and does not protect

TLS here protects **the wire between a caller and this process**: a
document, its secrets included, is encrypted in transit, and a caller
that verifies the server's certificate knows it is talking to the config
server rather than to whatever answered.

It protects nothing else, and in particular:

- **Not the store behind it.** That connection is the store crate's own
  TLS, configured where the store is configured.
- **Not the file the tokens live in.** Whoever can read the server's
  configuration has the credentials, encrypted socket or not.
- **Not against a caller that skips verification.** A client that does
  not check the certificate gets an encrypted connection to somebody, and
  which somebody is exactly what it declined to find out.
- **Not revocation.** See below.

### Not revocation

`client_ca` configures no CRL and checks none, so a client certificate is
valid until it expires. A `tls.crl` key is a **startup refusal** rather
than a file this server reads, because the alternative is a check that
reports itself as happening and is not.

That is a decision rather than an omission, and both halves of it were
measured against rustls rather than read off its documentation:

- **A stale CRL is used silently by default.** rustls's default
  `ExpirationPolicy` is `Ignore`, so a list whose `nextUpdate` passed in
  2020 verifies a handshake in 2026 with no error and no log line. A
  server built the obvious way would report that it checks revocation and
  would stop doing so the moment whatever writes the file stopped —
  invisibly. It would also *test green*: the natural test, revoke a
  certificate and assert the handshake fails, passes against a six-year-old
  list, because revocation keeps working. Only freshness rots, and nothing
  watches it.
- **The switch that fixes that breaks everything else.**
  `enforce_revocation_expiration` refuses a stale list — and refuses every
  clean, unrevoked client with it, for as long as it is stale. That makes
  the CA's publishing cadence a liveness dependency of every service's
  configuration fetch, inside the one process a fleet cannot start without.

The shape that looked most promising was re-reading the file on the same
watcher the sections already use, and it does not hold: a watcher fires on
a **write**, and what has to be noticed here is the *absence* of one. No
filesystem event says "this should have been refreshed an hour ago".
Noticing that needs a clock and a periodic wake-up, which is the polling
loop this server does not have and whose absence is one of its stated
properties. An HTTP distribution point is a fetch loop; OCSP is a second
protocol and a third dependency.

What settles it is the property this page has already established: a
certificate is a **gate, not an identity**. A stolen certificate on its own
buys a TCP connection and a 401 — reading anything needs the bearer token.
So a CRL here would revoke the credential that does not authorise, on a
freshness schedule this server cannot verify, while the credential that
does authorise is a line in a file the operator already holds: delete it,
restart, done in seconds, with no certificate authority, no cadence and no
new way for the server to fail.

**So: issue short-lived client certificates, and revoke the token.** The
measurement behind this lives in
`dynamic-config-server/tests/tls.rs`, as
`the_measurement_behind_refusing_revocation_still_holds` — it fails if
either rustls default moves, which is the signal to reconsider rather than
to discover the reasoning has quietly expired.

### A terminator in front is still fine

Nothing about the old shape stopped working. `insecure = true` still
means *something in front of this process is doing the encryption*, and
it is still the only way to bind a non-loopback address in the clear.
What changed is that the acknowledgement is now one of two answers rather
than the only one, and the two may not be given together: `insecure` with
`[server.tls]` is a refusal, because the word acknowledges an unencrypted
socket and there is not one. That refusal is not pedantry — without it, a
configuration carrying both would keep starting after the TLS section was
deleted, in the clear, on a public address, having been pre-approved
months earlier by somebody who meant something else.

The whole matrix:

| `[server.tls]` | `bind` | `insecure` | outcome |
|---|---|---|---|
| absent | loopback | either | starts; plaintext, reachable from this host only |
| absent | not loopback | not set | **refused** — it names both `tls` and `insecure` |
| absent | not loopback | `true` | starts; the operator says a terminator is in front |
| present | either | not set | starts; this process terminates TLS |
| present | either | `true` | **refused** — an acknowledgement of something untrue |

## What a client certificate is, and what it is not

With `client_ca` configured, **every caller must present a certificate
that chains to it**, or the handshake does not complete and no request
exists to answer. That is a second factor beside the bearer token, and
for many deployments it is the reason to run TLS here at all.

**It is a gate, not an identity.** A certificate gets a caller a
connection and nothing else: the caller is still nobody until it presents
a bearer token, and the token is still what produces a principal, what
the grants hang off, and what the audit log records. Authorisation did
not move; the [not-an-oracle](#it-refuses-to-be-an-oracle) property did
not move; a caller with an impeccable certificate and a token scoped to
`billing` gets the same 404 for `payroll` as anybody else.

Two alternatives were available and both were rejected:

- **A certificate *instead of* a token.** That makes the certificate a
  way to bypass the token, which is the opposite of what a second factor
  is for.
- **A certificate that *names* a client**, mapping its subject to a
  principal. This is the tempting one, and it is the one that quietly
  moves authorisation out of this server: the subject is a string chosen
  by whoever holds the CA key, which in most fleets is a platform team,
  an ACME client or a mesh — none of which has heard of this server's
  grants. Anybody who could get a certificate saying `CN=billing-pod`
  would read `billing`. It also creates a second roster to keep in step
  with the first, and the failure mode of the two disagreeing is silent.

So the certificate says *this connection came from a machine the
deployment provisioned*, and the token says *this caller may read
`billing`*. Two independent facts, both required, neither able to stand
in for the other. `dynamic-config-server/tests/tls.rs` asserts the second
half of that in as many words: a valid certificate with no token is a
401, and a valid certificate with the wrong grant is the same 404 as a
section nobody serves.

### The private key

It is the sharpest secret in this repository's surface — the one file
whose bytes have no legitimate destination at all. So:

- **No diagnostic carries them.** The two errors that would have — a PEM
  that will not parse — deliberately drop their source, because the one
  useful field of a parse error is the input it choked on. What a refusal
  carries is a path, a configuration key and a sentence.
- **A key file that anything but its owner can read is a startup
  refusal** on Unix, in the same breath as a token under 32 characters
  and for the same reason. The message names the fix — `chmod 600`, or
  `defaultMode: 0400` for a Kubernetes secret volume, whose default of
  `0644` is precisely the case this refusal exists to catch.
- **It stays in memory for the process's life**, because rustls needs it
  to answer handshakes, and no attempt is made to pretend otherwise. A
  process that can be made to dump core was already every secret it
  serves; the key is not a new class of exposure, and a zeroizing
  allocator here would be theatre rather than a boundary.

`dynamic-config-server/tests/security.rs` is the enforcement, planting a
real key and asserting its bytes reach no error, no `Debug` and no log
line.

## It refuses to be an oracle

A caller that may not read `billing` and a caller asking for an
application nobody serves get the **same 404, with the same body, having
done the same work**. Authorisation is decided from the caller's grants
alone, and the section map is never consulted for an application the
caller was not granted — so there is nothing to time and nothing to read.

The same 404 covers a malformed application, a malformed profile, a
malformed key path and a route this server does not have. The only
distinction a caller can draw is 401 (your credential did not work — a
fact about the caller, not about any section) from 404 (everything else).

## Where values may go

The library's rule is that a value never appears in a diagnostic. A server
*exists* to serve values, so the rule becomes a boundary:

- **The document endpoint returns values.** That is the handover.
- **Nothing else does.** `paths`, `check` and `status` are built from
  library types that carry no values by construction; `explain` goes
  through `Explanation::redacted()` on every path.
- **The log never does**, and cannot: an `AuditEntry` has no field a
  value could occupy — a caller name and an endpoint from fixed sources,
  an application and profile that have already passed the shape check, an
  outcome and a generation number. No amount of later editing puts a
  value there.
- **A metric label carries less still.** `/metrics` is built from the
  same value-free `ConfigStatus` as `status`, and the boundary there is
  drawn one notch tighter than anywhere else: not the key path either,
  which `/status` *is* allowed to return to an authorised caller. A label
  is unbounded cardinality as well as a disclosure, and a scrape leaves
  the process on a schedule rather than because somebody asked.

`/metrics` is **authenticated and scoped to the caller's grants**, which
`/healthz` and `/readyz` are not. Those two answer a boolean and disclose
nothing — that is what lets them be open. A metrics endpoint that
disclosed as little would be useless, and one that names sections
enumerates every service the fleet configures: an open one would undo the
not-an-oracle property on a timer. A scraper is therefore a client like
any other, with a token and its own grants, and it sees exactly the
applications it was granted.

**A change stream carries less than any of them.** `/{application}/{profile}/stream`
is a long-lived body — the one response that outlives the request which
authorised it — so what travels down it is a generation number and the
two names the caller supplied in the URL. Not the document, which would
make it a second door for values with a longer lifetime than the first;
and not the changed key paths either, which would disclose nothing
`/paths` does not already tell the same caller but would have to be
diffed per install and held per connection.

It is authenticated and authorised on the same path as everything else,
before anything is looked up, so a subscription to a section the caller
may not read is the same 404, with the same body, having done the same
work. The audit log records the subscription once; the events after it
say no more than a `/status` poll would.

Its resource cost is bounded and stated: one `Changes` handle and one
waker per connection, nothing proportional to the document, and a
configured `max_stream_connections` beyond which the answer is a 503 with
a `Retry-After`. That ceiling is a backstop against a client reconnecting
in a loop, not a rate limit — per-caller limiting belongs to the thing in
front, with everything else that a herd is best absorbed by. Setting it
to zero turns the endpoint off, and it then answers like a path this
server does not have.

`dynamic-config-server/tests/security.rs` is the enforcement, planting a
value and asserting it reaches exactly one response body and no log line
— the same pattern as the library's own `tests/security.rs`.

## What this does not defend

**The store behind it.** This server is a cache and a fan-out, not an
authority. It does not sign what it serves, so a client that needs
provenance it can verify has to verify it at the store.

**Availability of its clients.** A client that stops working when this
server is down has misconfigured its own last-known-good cache; that
cache is what makes a config server a convenience rather than a new single
point of failure.

**Whoever can read the server's configuration file.** It holds the
tokens. It should be mounted the way a secret is mounted.
