# dynamic-config-nats

Read [`dynamic-config`] configuration from a NATS JetStream key/value bucket.

```toml
[dependencies]
dynamic-config = { version = "0.5.0", features = ["async"] }
dynamic-config-nats = "0.5.0"
```

```rust
use dynamic_config_nats::Nats;

DbConfig::set_remote_async(
    Nats::new("nats://nats.internal:4222", "config", "db.json").await?,
);

// Fetching is explicit; the load that follows touches no network.
DbConfig::refresh_remote_async().await?;
```

NATS is a streaming protocol and its client is async throughout, so this
implements the **async** `AsyncRemoteSource` trait rather than the blocking one.

## What it reads

One key in one bucket, whose value is **a whole configuration document** — the
same bytes that would be in a config file. The format comes from the key's
extension, or from `with_format`.

Like Consul and unlike [`dynamic-config-vault`], and deliberately: a KV bucket
stores opaque bytes, so the natural unit is the document; Vault's KV v2 stores a
JSON object of fields, so the natural unit there is the field.

**JetStream must be enabled.** A key/value bucket is a JetStream feature, and a
server started without it answers with a "JetStream is not enabled" error, which
is reported as it arrives rather than translated into something vaguer.

**The bucket must already exist.** This crate does not create it: a configuration
reader that provisions storage would hide a misconfigured deployment behind an
empty one.

## Connecting

Every credential NATS understands — a token, a user and password, an NKey, a JWT,
a `.creds` file, TLS — lives on `ConnectOptions`, which is NATS' own type
re-exported here so using it needs no direct dependency on `async-nats`.

```rust
use dynamic_config_nats::{ConnectOptions, Nats};

let nats = Nats::with_options(
    "nats://nats.internal:4222",
    "config",
    "db.json",
    ConnectOptions::with_credentials_file("/etc/myapp/nats.creds").await?,
)
.await?;
```

Unlike a gRPC client this connects eagerly, so an unreachable server *is* a
construction failure.

## Sharing a client

```rust
let nats = Nats::from_client(client, "config", "db.json").await?;
```

For a caller already connected to NATS: reusing the connection beats opening a
second one to the same server, and the client is `Clone` — cheaply, it is a
handle. `from_store` goes one step further, for a program that already holds the
`Store` itself.

## Reconnecting is the client's job, and it does it

`async-nats` reconnects on its own, indefinitely, and re-establishes
subscriptions when it does. So there is deliberately no retry logic here: adding
one would mean a second, worse implementation of something the client already
does properly, layered on top of it.

Two consequences worth knowing. A `fetch` during a disconnect fails rather than
blocking until the connection returns — configuration that hangs is worse than
configuration that reports. And a `watch` survives a reconnect without the caller
noticing, which is why it ending at all is treated as an error.

## Watching

A KV bucket is a stream, so `watch` is a future the caller spawns and cancels by
dropping — no runtime is imposed and no flag is polled.

```rust
let task = tokio::spawn(async move { nats.watch(move |document| sink.apply(document)).await });

// Dropping or aborting the task stops the watch.
task.abort();
```

- The current value is **not** delivered at startup: a watch reports changes, and
  announcing the value the caller already has would make every restart look like
  an edit. Fetch first if the starting value matters.
- **Deletes and purges are not changes.** No configuration is not a
  configuration, and neither replaying the last one nor pushing emptiness beats
  leaving the running snapshot alone.
- **It never returns `Ok`.** A watch either runs or has failed; a silent success
  would leave a spawned task finished and a configuration frozen with nothing
  said about either.

## Builders

| Method | Default |
|---|---|
| `with_format(..)` | from the key's extension |
| `with_options(..)` | `ConnectOptions::new()` — no credentials |
| `from_client(..)` / `from_store(..)` | opens its own connection |

## Example

| Example | Shows |
|---|---|
| [`nats_watching`](examples/nats_watching.rs) | One client shared by the reader and the watcher, over a JetStream KV bucket. |

It needs a server, and its own doc comment says how to start one in a container
and put a document in it. (The `nats` CLI lives in the `nats-box` image, not in
the server image.)

```sh
cargo run -p dynamic-config-nats --example nats_watching
```

## Testing

The test suite drives a **real NATS server in a container**, started with
`--jetstream` — no mocks. That is how the missing-bucket behaviour above got
pinned rather than assumed.

```sh
cargo test -p dynamic-config-nats    # needs a working Docker daemon
```

## MSRV

1.88 — higher than [`dynamic-config`]'s own 1.71, because a streaming client
stack moves faster than that crate wants to. A companion pays for what it pulls
in; the core stays where it is.

## License

MIT

[`dynamic-config`]: https://docs.rs/dynamic-config
[`dynamic-config-vault`]: https://docs.rs/dynamic-config-vault
