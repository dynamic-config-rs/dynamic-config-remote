# Redis

[`dynamic-config-redis`](https://docs.rs/dynamic-config-redis) reads
configuration from a Redis key. Request/response over a plain protocol:
the **blocking** `RemoteSource`, no runtime required.

```toml
[dependencies]
dynamic-config = "<version>"
dynamic-config-redis = "<version>"
```

```rust
use dynamic_config_redis::Redis;

DbConfig::set_remote(Redis::new("redis://redis.internal:6379", "myapp/db.json")?);
DbConfig::refresh_remote()?;
```

**What it reads:** one key holding a whole configuration document. A
Redis *hash* — one field per setting — is deliberately not the mapping: a
hash cannot hold a nested table without inventing a flattening
convention, and a document already has one.

**Credentials:** in the URL, where Redis puts them and where every
deployment already has them — `redis://user:password@host:6379/0`, or
`rediss://` with the crate's `tls` feature (rustls; without the feature
the connection fails at connect time with the client's own message). A
password never reaches an error message: the URL is redacted before it is
used in one — including on the parse-error path. `from_client` takes a
client the program built, for anything a URL cannot say.

**Watching:** keyspace notifications — genuinely change-driven, no timer.
The crate subscribes to `__keyspace@{db}__:{key}`; because notifications
are **off by default in Redis**, it checks at startup and reports the
`CONFIG SET notify-keyspace-events KEA` to run rather than hanging on a
channel that will never speak. `del` and `expired` are not changes; a
dead subscription ends the watch with an error so a supervisor can
restart it; stopping is noticed within a quarter second.

The [README](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-redis)
carries the full story; MSRV 1.88.
