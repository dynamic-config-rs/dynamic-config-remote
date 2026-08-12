# NATS

[`dynamic-config-nats`](https://docs.rs/dynamic-config-nats) reads
configuration from a NATS JetStream key/value bucket. NATS is a streaming
protocol with an async client throughout, so this implements
**`AsyncRemoteSource`**.

```toml
[dependencies]
dynamic-config = { version = "<version>", features = ["async"] }
dynamic-config-nats = "<version>"
```

```rust
use dynamic_config_nats::Nats;

DbConfig::set_remote_async(
    Nats::new("nats://nats.internal:4222", "config", "db.json").await?,
);
DbConfig::refresh_remote_async().await?;
```

**What it reads:** one key in one bucket, holding a whole configuration
document. Two preconditions are reported honestly rather than papered
over: **JetStream must be enabled** (a KV bucket is a JetStream feature),
and **the bucket must already exist** — a configuration reader that
provisions storage would hide a misconfigured deployment behind an empty
one.

**Connecting:** every credential NATS understands — token, user/password,
NKey, JWT, a `.creds` file, TLS — lives on its own `ConnectOptions`,
re-exported. Unlike the gRPC stores this connects *eagerly*, so an
unreachable server is a construction failure. `from_client` reuses a
connection the program already holds; `from_store` goes one further for a
program that already has the `Store`.

**Reconnecting is the client's job, and it does it:** `async-nats`
reconnects indefinitely and re-establishes subscriptions, so this crate
deliberately adds no retry layer on top. Two consequences: a `fetch`
during a disconnect fails rather than hanging, and a `watch` survives a
reconnect silently — which is why the watch *ending at all* is an error.

**Watching:** the bucket is a stream, so `watch` is a future — spawn it,
drop it to cancel. No startup delivery, deletes and purges are not
changes, and it never returns `Ok`.

The [README](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-nats)
carries the full story and the `nats_watching` example; MSRV 1.88.
