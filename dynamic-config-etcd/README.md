# dynamic-config-etcd

Read [`dynamic-config`] configuration from an etcd v3 key/value store.

```toml
[dependencies]
dynamic-config = { version = "0.0.1", features = ["async"] }
dynamic-config-etcd = "0.0.1"
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

## Watching

etcd's watch is a real push stream, so `watch` is a future the caller spawns and
cancels by dropping — no runtime is imposed and no flag is polled.

```rust
let task = tokio::spawn(async move { etcd.watch(DbConfig::apply_remote).await });

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

## Builders

| Method | Default |
|---|---|
| `with_format(..)` | from the key's extension |
| `with_options(..)` | `ConnectOptions::new()` — no credentials, no TLS |
| `from_client(..)` | opens its own connection |

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
