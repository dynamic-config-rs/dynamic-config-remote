# dynamic-config-redis

Read [`dynamic-config`] configuration from a Redis key.

```toml
[dependencies]
dynamic-config = "0.4.0"
dynamic-config-redis = "0.4.0"
```

```rust
use dynamic_config_redis::Redis;

DbConfig::set_remote(Redis::new("redis://redis.internal:6379", "myapp/db.json")?);

DbConfig::refresh_remote()?;
DbConfig::builder("db").init()?;
```

Redis speaks a plain request/response protocol, so this implements the
**blocking** `RemoteSource` trait: nothing here needs an async runtime, and
neither does using it.

## What it reads

One key, whose value is **a whole configuration document** — the same bytes that
would be in a config file. The format comes from the key's extension, or from
`with_format`.

A Redis hash would be the other obvious mapping — one field per setting — and is
deliberately not what this does. A hash cannot hold a nested table without
inventing a flattening convention, and a document already has one.

## Credentials

In the URL, which is where Redis puts them and where every deployment already
has them:

```text
redis://user:password@host:6379/0
rediss://user:password@host:6380/0     # TLS — needs the `tls` feature
```

`rediss://` needs the crate's `tls` feature (`dynamic-config-redis = { version
= "...", features = ["tls"] }`), which turns on the client's rustls stack.
Without it the connection fails at connect time with the client's own "TLS
support is not enabled" error.

A password never reaches an error message: the URL is redacted before it is
used in one. `from_client` takes a client the program already built, for
anything a URL cannot say.

## Watching

**Keyspace notifications**, which are genuinely change-driven — no polling, no
timer. Redis publishes to `__keyspace@{db}__:{key}` when a key is written, and
this subscribes to exactly that channel.

```rust
let watch = RemoteWatch::new();
let watching = watch.watching();

std::thread::spawn(move || redis.watch(&watching, move |document| sink.apply(document)));
```

**Notifications are off by default in Redis.** A server that has not enabled
them publishes nothing, and a watch waiting for them would hang — the worst way
for a feature to be unavailable. So this checks at start-up and reports:

```text
CONFIG SET notify-keyspace-events KEA
```

- The current value is **not** delivered at startup: a watch reports changes,
  and announcing the value the caller already has would make every restart look
  like an edit.
- **`del` and `expired` are not changes.** No configuration is not a
  configuration, so the running snapshot stays.
- Stopping is noticed within a quarter second, whether or not anything arrives.

## Builders

| Method | Default |
|---|---|
| `with_format(..)` | from the key's extension |
| `from_client(..)` | opens its own connection |

## Testing

The test suite drives a **real Redis in a container** — no mocks, with keyspace
notifications enabled the way a deployment would. The image is pinned to a
current release rather than the module's default of Redis 5, which is long out
of support.

```sh
cargo test -p dynamic-config-redis    # needs a working Docker daemon
```

## MSRV

1.88 — higher than [`dynamic-config`]'s own 1.71, because a Redis client stack
moves faster than that crate wants to. A companion pays for what it pulls in;
the core stays where it is.

## License

MIT

[`dynamic-config`]: https://docs.rs/dynamic-config
