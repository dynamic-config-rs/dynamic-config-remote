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

**Several keys as one document:** `Keys::several([..])` merges named keys
in call order — later wins, the rule `.file(..)` already teaches — and
`Keys::prefix("myapp/")` merges the sections under a prefix, where an
overlap between two of them is an error naming both keys and the paths. A
named list is one `MGET`, which Redis runs as one operation. A prefix is a
`SCAN` and then an `MGET`, and **never `KEYS`**: `KEYS` walks the whole key
space in one blocking operation and is the classic way to stall a
production server. The price is that `SCAN` is not atomic, so prefer a
named list where the keys are known. The prefix is matched as a literal —
`*`, `?`, `[`, `]` and `\` in it are escaped before the `MATCH` goes out,
and every key that comes back is checked against the literal prefix — so a
tenant id with a bracket in it selects itself and nothing else. Capped at
512 keys. See
[several keys as one document](../remote-stores.md#several-keys-as-one-document)
for what this costs in provenance and in watching.

**Credentials:** in the URL, where Redis puts them and where every
deployment already has them — `redis://user:password@host:6379/0`, or
`rediss://` with the crate's `tls` feature (rustls; without the feature
the connection fails at connect time with the client's own message). A
password never reaches an error message: the URL is redacted before it is
used in one — including on the parse-error path. `from_client` takes a
client the program built, for anything a URL cannot say.

**TLS as data:** `Redis::with_tls(url, keys, tls)` takes a private
certificate authority and a client certificate as paths or PEM bytes, with
no `redis` type in the calling code. Behind the `tls` feature, which is
what `rediss://` needs anyway. TLS material on a `redis://` URL is refused
here rather than three layers down: it is a deployment that believes it is
encrypted and is not. [TLS, and the one vocabulary all eight speak](../remote-stores.md#tls-and-the-one-vocabulary-all-eight-speak)

**Watching:** keyspace notifications — genuinely change-driven, no timer.
The crate subscribes to `__keyspace@{db}__:{key}`, one channel per key of
the set; because notifications are **off by default in Redis**, it checks
at startup and reports the `CONFIG SET notify-keyspace-events KEA` to run
rather than hanging on a channel that will never speak. `del` and
`expired` are not changes; a dead subscription ends the watch with an
error so a supervisor can restart it; stopping is noticed within a quarter
second.

**A named list can be watched; a prefix cannot.** The whole difference is
`MGET`: it is one command, and Redis runs one command as one operation, so
the set it answers with is a state the server really held rather than one
key's new value beside another's old one. A prefix has to *find* its keys
again first, and `SCAN` is a cursor walked over many commands with writes
free to land between them — so it is refused at `watch()`, naming
`Keys::several` as the shape that works. The read still *follows* the
notification rather than being simultaneous with it, so a delivery may
carry a state newer than the write that woke it, and a set written with one
`MSET` publishes once per key and is delivered once:
[spurious, never torn](../remote-stores.md#spurious-never-torn).

**A failing watch says so:** `Redis::new(url, keys)?.reporting_to(sink)`
takes the same `remote_sink()` the loop already pushes documents through, and
reports the failures *inside* the loop to it. A watch is otherwise the half of
a store `dynamic-config` cannot see: a delivery keeps the `RemoteStatus`
current because `apply` records one, so `dynamic_config_remote_up` reports the
last **delivery** rather than the last **attempt**, and a Redis that stopped
answering an hour ago looks healthy until something calls `refresh_remote()`.
A reported failure moves the failure streak and nothing else, so
`dynamic_config_remote_last_fetch_seconds` keeps ageing while
`dynamic_config_remote_up` goes to zero — which is the pair an alert wants:
down, and stale for how long. Only the failure's kind and key path are
recorded, so the URL that carries the password stays out of it.

Redis fails at a watch in two shapes and both report, because the streak is
what tells them apart rather than the API. A **re-read that came back with
nothing** — one `MGET`, for a named list — is transient: the next write
notifies again, and one delivery clears the streak, so a blip looks like a
blip and a credential the server has started refusing climbs. A **dead
subscription** ends the watch, and it is the failure nobody notices: the loop
runs on a thread whose result is usually dropped, so configuration silently
stops updating. What is deliberately *not* reported is a refusal at the door —
a prefix, no format, no keys, notifications off — because `watch()` returns
those to the caller standing there, before there is a loop to be silent in,
and half of them are deployment mistakes rather than a store that stopped
answering.

The [README](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-redis)
carries the full story; MSRV 1.88.
