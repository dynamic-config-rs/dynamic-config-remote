# dynamic-config-etcd

Read [`dynamic-config`] configuration from an etcd v3 key/value store.

```toml
[dependencies]
dynamic-config = { version = "0.6.1", features = ["async"] }
dynamic-config-etcd = "0.6.1"
```

```rust
use dynamic_config_etcd::Etcd;

DbConfig::set_remote_async(
    Etcd::new(["http://etcd.internal:2379"], "myapp/db.json").await?,
);

// Fetching is explicit; the load that follows touches no network.
DbConfig::refresh_remote_async().await?;
```

etcd speaks gRPC, so its client is async — which is why this implements the
**async** `AsyncRemoteSource` trait rather than the blocking one. Pretending
otherwise would only hide a `block_on` inside the crate.

## What it reads

One key, whose value is **a whole configuration document** — the same bytes that
would be in a config file. The format comes from the key's extension, or from
`with_format`.

## Several keys as one document

```rust
use dynamic_config_etcd::{Etcd, Keys};

// Named keys: a list of layers, merged in call order — later wins.
Etcd::new(endpoints, Keys::several(["myapp/base.json", "myapp/local.json"])).await?;

// A prefix: disjoint sections, and an overlap between two of them is an error
// naming both keys and the paths.
Etcd::new(endpoints, Keys::prefix("myapp/")).await?.with_format(Format::Json);
```

| | Requests | Consistency | Ceiling |
|---|---|---|---|
| `Keys::several` | one transaction of range reads | one revision | 128 keys, etcd's own `--max-txn-ops` |
| `Keys::prefix` | one range read | one revision | 512 keys |

One unreadable key fails the whole fetch, naming it — a configuration quietly
missing a section is worse than a refresh that failed and left the last document
serving. Provenance becomes store-grained: the merged document is one layer, so
`source_of` names the store and the set rather than which key supplied a value.
A **prefix can be watched** and a **named list cannot**: one stream over one
range says the range moved and carries the revision it moved at, and one range
read at that revision is the whole set as of one instant, while a list would be
one stream per key and none of them would say the set moved together. A list
refuses at `watch()` and says so; poll `refresh_remote_async()` on a timer,
which is the same one round trip.

## Connecting

Credentials and TLS go through etcd's own `ConnectOptions`, re-exported here so
using them needs no direct dependency on `etcd-client`. There is no second
vocabulary to learn, and options this crate has never heard of keep working.

```rust
use dynamic_config_etcd::{ConnectOptions, Etcd};

let etcd = Etcd::with_options(
    ["https://etcd.internal:2379"],
    "myapp/db.json",
    ConnectOptions::new()
        .with_user("myapp", std::env::var("ETCD_PASSWORD")?)
        .with_keep_alive(Duration::from_secs(30), Duration::from_secs(5)),
)
.await?;
```

TLS types — `TlsOptions`, `Identity`, `Certificate` — are behind this crate's
`tls` feature, because TLS pulls a whole stack in and a program talking to etcd
over a private network inside a cluster has no use for it. `tls-roots` also
trusts the platform's root store, which is what a public CA needs.

### A private CA, and a client certificate, without naming a `tonic` type

`with_tls` takes the same data-only `TlsConfig` every store in this family
takes — a certificate authority and a client certificate, each as a file path
or as PEM bytes:

```rust
use dynamic_config_etcd::{ConnectOptions, Etcd, TlsConfig};

let etcd = Etcd::with_tls(
    ["https://etcd.internal:2379"],
    "myapp/db.json",
    ConnectOptions::new().with_user("myapp", std::env::var("ETCD_PASSWORD")?),
    &TlsConfig::new()
        .with_ca_certificate_file("/etc/etcd/ca.pem")
        .with_client_certificate_files("/etc/etcd/client.crt", "/etc/etcd/client.key"),
)
.await?;
```

etcd expresses all of it. mTLS is not an afterthought here the way it is for
the HTTP stores: a cluster started with `--client-cert-auth` is the ordinary
hardened deployment. Behind the same `tls` feature; `ConnectOptions` keeps
carrying everything that is not TLS, and the `TlsConfig` owns the TLS slot —
`etcd-client` exposes no way to ask whether that slot is already filled, so use
one door or the other.

Nothing is read at build time: a missing certificate is an error naming the
path. There is no way to turn verification off, and the book's [remote stores chapter](https://github.com/ctolon/dynamic-config/blob/main/book/src/remote-stores.md#tls-and-the-one-vocabulary-all-seven-speak) argues
that one.

**`new` does not prove the server is reachable.** The client connects lazily, so
an unreachable etcd surfaces on the first read rather than at construction. That
is the client's behaviour, not a choice made here, and papering over it with an
eager round trip would make every construction cost one.

## Sharing a client

```rust
let etcd = Etcd::from_client(client, "myapp/db.json");
```

For a caller that already talks to etcd and would rather not open a second
connection. The client is `Clone` — cheaply, it is a handle — so sharing one
costs nothing, and a shared client recovers from an expired token like any other.

## Expiry

etcd issues simple auth tokens with a TTL, five minutes by default, and a
long-lived configuration reader outlives one. On `invalid auth token` this asks
for a new one and retries, once.

Not a reconnect: the gRPC channel looks after itself, and the client kept the
credentials, so the thing that actually expired is the only thing replaced. That
is also what lets a *shared* client recover — replacing a client the caller owns
would not be this crate's to do.

Once, not in a loop: if a fresh token is refused too, the credentials are wrong,
and retrying would turn a clear failure into a hang.

## Timeouts

`with_timeout(..)` is **the deadline for a single fetch attempt, excluding
retries the underlying client performs** — the same sentence every store in
this family answers to. Ten seconds by default.

etcd's own `ConnectOptions::with_timeout` bounds *connecting*, which is a
different thing: it does nothing for a connection established minutes ago. So
this one wraps the request itself, which is what catches a member that accepts
the call and then goes quiet. Set both — they cover different halves.

Neither applies to `watch`, which is long-lived on purpose.

## Errors

A refusal etcd words as its own — `invalid auth token`, `authentication
failed`, `permission denied` — is reported as `ErrorKind::Auth` rather than
`ErrorKind::Remote`. The difference is what a watch loop needs: an unreachable
member comes back, and a wrong password does not.

## Watching

etcd's watch is a real push stream, so `watch` is a future the caller spawns and
cancels by dropping — no runtime is imposed and no flag is polled.

```rust
let task = tokio::spawn(async move { etcd.watch(move |document| sink.apply(document)).await });

// Dropping or aborting the task stops the watch.
task.abort();
```

- The current value is **not** delivered at startup: a watch reports changes, and
  announcing the value the caller already has would make every restart look like
  an edit. Fetch first if the starting value matters.
- **A deletion is not a change.** No configuration is not a configuration, and
  neither replaying the last one nor pushing emptiness beats leaving the running
  snapshot alone.
- **It never returns `Ok`.** A watch either runs or has failed; a silent success
  would leave a spawned task finished and a configuration frozen with nothing
  said about either. A cancelled watch — compaction is the usual reason — and a
  closed connection are both reported. Loop around it to reconnect.

### A failing watch says so

`reporting_to(sink)` hands the loop the same `RemoteSink` it already delivers
through, and every attempt that comes back with nothing is reported to it: the
stream erroring, etcd cancelling the watch, the range read at an event's
revision failing, a document that will not merge, and the connection closing
under it.

```rust
let sink = DbConfig::remote_sink();

etcd.reporting_to(sink).watch(move |document| sink.apply(document)).await
```

Without it a watch is the half of a store `dynamic-config` cannot see: only
deliveries are recorded, so `dynamic_config_remote_up` reports the last
*delivery* rather than the last *attempt*. A failure moves the failure streak
and the last failure and nothing else, so `remote_up` goes to zero while
`remote_last_fetch_seconds` keeps ageing — the pair an alert wants.

**A replaced auth token is not a failure.** A watch that outlives its token logs
in again and resumes from the last delivered revision; nothing is reported for
that, because the store answered and no event was lost — and since only a
delivery or a fetch clears the streak, reporting it would hold `remote_up` at
zero on a healthy cluster until the next change. A re-authentication that
*fails*, a stream that will not re-establish, and a recovery cap that runs out
all report. `on_change`'s own refusal does not: the store answered, `apply`
counted the delivery, and a document that would not install is `ConfigStatus`'s
half of the picture.

## Builders

| Method | Default |
|---|---|
| `with_format(..)` | from the key's extension |
| `with_options(..)` | `ConnectOptions::new()` — no credentials, no TLS |
| `with_timeout(..)` | 10 seconds |
| `reporting_to(..)` | nothing reported; a failing watch is invisible |
| `from_client(..)` | opens its own connection |
| `with_tls(..)` *(constructor, `tls` feature)* | the platform trust store, no client certificate |

## Example

| Example | Shows |
|---|---|
| [`etcd_watching`](examples/etcd_watching.rs) | Connecting with `ConnectOptions`, watching a push stream, and a task awaiting reloads alongside it. |

It needs a server, and its own doc comment says how to start one in a container
and put a document in it.

```sh
cargo run -p dynamic-config-etcd --example etcd_watching
```

## Testing

The test suite drives a **real etcd in a container** — no mocks, including one
started with authentication enabled and a one-second token TTL. That is how the
lazy-connect behaviour above was found: a mock would have confirmed what we
already believed instead.

```sh
cargo test -p dynamic-config-etcd    # needs a working Docker daemon
```

The image comes from `quay.io`, which has no anonymous pull limits.

## MSRV

1.85 — higher than [`dynamic-config`]'s own 1.71, because a gRPC stack moves
faster than that crate wants to. A companion pays for what it pulls in; the core
stays where it is.

## License

MIT

[`dynamic-config`]: https://docs.rs/dynamic-config
