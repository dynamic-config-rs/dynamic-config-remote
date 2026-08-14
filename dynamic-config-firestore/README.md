# dynamic-config-firestore

Read [`dynamic-config`] configuration from a Google Cloud Firestore document.

```toml
[dependencies]
dynamic-config = "0.6.1"
dynamic-config-firestore = "0.6.1"
```

```rust
use dynamic_config_firestore::{Auth, Firestore};

DbConfig::set_remote(
    Firestore::new("my-project", "config/db").with_auth(Auth::metadata_server()),
);

DbConfig::refresh_remote()?;
```

Firestore's REST API is plain HTTP, so this implements the **blocking**
`RemoteSource` trait: nothing here needs an async runtime, and neither does
using it.

## What it reads

One document, at a path like `config/db` — collection, then document. Its fields
become the configuration, wrapped under the section key, which is the same shape
[`dynamic-config-vault`] uses and for the same reason: Firestore stores a map of
named fields, so the natural unit is the field.

### Several documents as one section

```rust
use dynamic_config_firestore::{Firestore, Keys};

// Merged in call order — later wins — and all under the one section key.
Firestore::new("my-project", Keys::several(["config/db", "overrides/db"]));
```

| | Requests | Consistency | Ceiling |
|---|---|---|---|
| `Keys::several` | **one** `:batchGet` | **not one snapshot** — without a transaction each document is read at its own time | the caller's list |

`:batchGet` is Firestore's own answer to a set rather than a loop wearing a
batch's name, and two things follow from what the API promises. The service
returns the documents in whatever order it likes and says so, so they are put
back into call order here — the order a caller wrote is the precedence, the order
a service replies in is not. And one request is not one snapshot: no transaction
is opened, because an open read-only transaction is state on the service that a
configuration read would have to remember to release.

**There is deliberately no collection form**, and `documents.list` is not what is
missing. Folding a collection into one section would make `config/db` and
`config/server` collide on `host` — the ordinary layout, refused — and naming a
sub-section after each document's id would invent a convention no other store in
this family has, and would make a list of one document mean something different
from one document.

One missing document fails the whole fetch, naming it; a document nobody asked
for, one answered twice, and one the store says nothing at all about are each
refused. Provenance becomes store-grained. A multi-document source refuses to be
watched — the `updateTime` that watch compares belongs to one document — so poll
`refresh_remote()` on a timer.

### Firestore does not store JSON

It stores a tagged encoding, where every value names its own type and integers
arrive as strings — because JSON's number is a double and Firestore's is not:

```json
{"port": {"integerValue": "5432"}}
```

Handing that to serde would mean every configuration struct growing a
`#[serde(with = ..)]` per field, so it is decoded here, once, into the shape a
configuration file would have had. `timestampValue`, `bytesValue` and
`referenceValue` keep their string form, because a configuration file has no
better answer for one either.

## Authenticating

| Method | Constructor | For |
|---|---|---|
| Workload identity | `Auth::metadata_server()` | GKE, Cloud Run, GCE — no secret to distribute |
| An access token | `Auth::access_token(..)` | anything that already has one, including `gcloud auth print-access-token` |
| None | `Auth::Emulator` | the Firestore emulator |

A token from the metadata server is cached and replaced as it approaches expiry;
a `401` replaces one early, once. A supplied token cannot be renewed — there is
nothing here to renew it from — so a long-running process should use workload
identity.

**A service-account JSON key is deliberately not supported**, and that is a
recommendation rather than a gap: signing one means an RS256 stack in a
configuration library, and Google's own guidance is that a downloaded key is the
option of last resort. Workload identity covers GKE, Cloud Run, GCE and Cloud
Functions; for anything else, mint a token outside the process and pass it in.

A `401` and a `403` are both reported as `ErrorKind::Auth` rather than
`ErrorKind::Remote` — the token was rejected, or the identity behind it is not
allowed to read the document, and neither comes right on its own. Exhausted
quota is a `429` and stays `Remote`, because that one does.

## TLS

`with_tls` takes the same data-only `TlsConfig` every store in this family
takes — a certificate authority and a client certificate, each as a file path
or as PEM bytes, with no `ureq` type in the calling code:

```rust
use dynamic_config_firestore::{Firestore, TlsConfig};

let firestore = Firestore::new("my-project", "config/db")
    .with_endpoint("https://firestore.internal")
    .with_tls(TlsConfig::new().with_ca_certificate_file("/etc/ssl/private-ca.pem"));
```

Firestore expresses all of it, and no feature has to be turned on. Rarely what
you want against Google's own endpoint, whose certificates chain to an
authority the platform already trusts — it is for the deployments that do not
go there directly: an enterprise TLS-inspecting proxy, or an emulator behind
`with_endpoint`. `with_agent` **and** `with_tls` together are refused at the
first request, because an agent already carries a complete TLS configuration.

There is no way to turn verification off, and the book's [remote stores chapter](https://github.com/ctolon/dynamic-config/blob/main/book/src/remote-stores.md#tls-and-the-one-vocabulary-all-seven-speak) argues that one.

## Timeouts

`with_timeout(..)` is **the deadline for a single fetch attempt, excluding
retries the underlying client performs** — the same sentence every store in
this family answers to. Ten seconds by default, and `ureq` performs no retries
of its own, so the deadline is the whole story.

It bounds each read the watch loop makes, not the loop itself, and it also
covers fetching a token from the metadata server — that request goes through the
same client.

## Watching

Firestore *can* push — the real-time API is a gRPC stream — and this deliberately
does not use it: that would put a gRPC stack in a crate whose whole point is a
plain HTTP read. Polling reads one small document and compares `updateTime`,
which for a configuration document checked every thirty seconds is a rounding
error against a project's quota.

```rust
firestore.watch(&watching, Duration::from_secs(30), move |document| sink.apply(document))
```

- The current document is **not** delivered at startup.
- A failed check does not end the watch. Stopping is noticed within a quarter
  second whatever the interval is.
- Surviving it is not the same as hiding it. `reporting_to(sink)` — the same
  sink the callback applies documents through — records every poll that came
  back with nothing, so `dynamic_config_remote_up` reports the last *attempt*
  rather than the last delivery. Without it, a document nobody edited today and
  a project that stopped answering look identical.

## Builders

| Method | Default |
|---|---|
| `with_key(..)` | `"db"` — must match the key given to `builder(..)` |
| `with_database(..)` | `(default)` |
| `with_auth(..)` | `Auth::Emulator`, which sends nothing |
| `with_endpoint(..)` | `https://firestore.googleapis.com` |
| `with_timeout(..)` | 10 seconds |
| `with_agent(..)` | one built per source |
| `with_tls(..)` | the platform trust store, no client certificate |
| `reporting_to(..)` | nobody — a watch's failed attempts are recorded nowhere |

## Testing

The test suite drives **Google's own Firestore emulator in a container**, pulled
from `gcr.io` rather than Docker Hub so a test run does not hit anonymous pull
limits. What is being checked is Firestore's value encoding and its REST
surface, and a mock of those would only confirm what we already believed about
them.

```sh
cargo test -p dynamic-config-firestore    # needs a working Docker daemon
```

## MSRV

1.85 — higher than [`dynamic-config`]'s own 1.71, because an HTTP client stack
and a container-driving test harness both move faster than that crate wants to.

## License

MIT

[`dynamic-config`]: https://docs.rs/dynamic-config
[`dynamic-config-vault`]: https://docs.rs/dynamic-config-vault
