# dynamic-config-redis

Read [`dynamic-config`] configuration from a Redis key.

```toml
[dependencies]
dynamic-config = "0.6.0"
dynamic-config-redis = "0.6.0"
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

## Several keys as one document

```rust
use dynamic_config_redis::{Keys, Redis};

// Named keys: a list of layers, merged in call order — later wins.
Redis::new(url, Keys::several(["myapp/base.json", "myapp/local.json"]))?;

// A prefix: disjoint sections, and an overlap between two of them is an error
// naming both keys and the paths.
Redis::new(url, Keys::prefix("myapp/"))?.with_format(Format::Json);
```

| | Commands | Consistency | Ceiling |
|---|---|---|---|
| `Keys::several` | one `MGET` | one operation | the caller's list |
| `Keys::prefix` | `SCAN`, then one `MGET` | **the scan is not atomic** | 512 keys |

`SCAN` and never `KEYS`: `KEYS` walks the whole key space in one blocking
operation and is the classic way to stall a production server. The price is the
non-atomic scan, so prefer a named list where the keys are known.

The prefix is matched as a **literal**. `SCAN MATCH` takes a glob, so `*`, `?`,
`[`, `]` and `\` in the prefix are escaped before the command goes out, and
every key that comes back is checked against the literal prefix — a tenant id
with a bracket in it selects itself and nothing else.

One unreadable key fails the whole fetch, naming it. Provenance becomes
store-grained: the merged document is one layer, so `source_of` names the store
and the set rather than which key supplied a value. A **named list can be
watched** and a **prefix cannot** — the reason is under [Watching](#watching).

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

### A private CA, and a client certificate, without naming a `redis` type

`with_tls` takes the same data-only `TlsConfig` every store in this family
takes — a certificate authority and a client certificate, each as a file path
or as PEM bytes:

```rust
use dynamic_config_redis::{Redis, TlsConfig};

let redis = Redis::with_tls(
    "rediss://app:password@cache.internal:6380",
    "myapp/db.json",
    &TlsConfig::new().with_ca_certificate_file("/etc/ssl/private-ca.pem"),
)?;
```

Redis expresses all of it, behind the same `tls` feature `rediss://` needs
anyway. TLS material on a `redis://` URL is refused, naming the scheme: it is a
deployment that believes it is encrypted and is not.

There is no way to turn verification off; the book's [remote stores chapter](https://github.com/ctolon/dynamic-config/blob/main/book/src/remote-stores.md#tls-and-the-one-vocabulary-all-seven-speak) argues that one, and
the client's own `#insecure` URL fragment stays where it is, under its own
frightening name.

## Timeouts

`with_timeout(..)` is **the deadline for a single fetch attempt, excluding
retries the underlying client performs** — the same sentence every store in
this family answers to. Ten seconds by default.

Redis has three separate knobs and this sets all of them from the one value:
connect, write, and read. All three, because a deadline covering only the
connect sails straight past a server that accepted the socket and then stopped
answering — which is what a wedged Redis actually looks like.

It bounds each read `watch` performs, not the watch itself: a subscription
waiting for the next notification is supposed to wait.

## Watching

**Keyspace notifications**, which are genuinely change-driven — no polling, no
timer. Redis publishes to `__keyspace@{db}__:{key}` when a key is written, and
this subscribes to exactly that channel — one per key of the set.

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
  configuration, so the running snapshot stays. For a named list, a member of
  the set holding nothing fails the read the same way a fetch does, and the
  loop treats that as transient — the next write notifies again.
- Stopping is noticed within a quarter second, whether or not anything arrives.

### A named list can be watched; a prefix cannot

The whole difference is `MGET`. It is **one command**, and Redis runs one
command as one operation, so the values it answers with are the set as of one
point in the command stream — the delivered document is a state the server
really held, never one key's new value beside another's old one. That is the
tear every other network store refused a multi-key watch over.

A prefix has to *find* its keys again before it can read them, and `SCAN` is a
cursor walked over many commands with writes free to land between them. So a
prefix is refused at `watch()`, before the first event rather than after a bad
one, naming `Keys::several` as the shape that works.

What the re-read is *not* is simultaneous with the notification — it follows
it. So a delivery may carry a state **newer** than the write that woke the
loop, and a set written with one `MSET` publishes once per key and is
delivered once, because the document the later notifications would carry is
the one already delivered. **Spurious, never torn**, and never older than the
delivery before it.

### A failing watch says so

`reporting_to(sink)` hands the loop the same `RemoteSink` it already delivers
through, and the failures **inside** the loop are reported to it:

```rust
let sink = DbConfig::remote_sink();

let watcher = Redis::new(url, "myapp/db.json")?.reporting_to(sink);

std::thread::spawn(move || watcher.watch(&watching, move |document| sink.apply(document)));
```

Without it a watch is the half of a store `dynamic-config` cannot see: only
deliveries are recorded, so `dynamic_config_remote_up` reports the last
*delivery* rather than the last *attempt*, and a Redis that stopped answering
an hour ago looks healthy until something calls `refresh_remote()`. A failure
moves the failure streak and the last failure and nothing else, so `remote_up`
goes to zero while `remote_last_fetch_seconds` keeps ageing — the pair an alert
wants: down, and stale for how long. Only a kind and a key path are recorded,
so the URL that carries the password stays out of it.

**Both shapes report, and the streak is what tells them apart.** A **re-read
that came back with nothing** — one `MGET` — is transient: the next write
notifies again, and one delivery clears the streak, so a blip looks like a blip
while a member of the set that has gone missing climbs. A **dead subscription**
ends the watch, and it is the failure nobody notices: the loop runs on a thread
whose result is usually dropped, so configuration silently stops updating.

**Refusals at the door do not report** — a prefix, no format, no keys,
notifications off, a subscription the server will not accept. `watch()` returns
those to the caller standing there, before there is a loop to be silent in, and
half of them are deployment mistakes rather than a store that stopped
answering. `on_change`'s own refusal does not report either: the store
answered, and a document that will not install is `ConfigStatus`'s half of the
picture.

## Builders

| Method | Default |
|---|---|
| `with_format(..)` | from the key's extension |
| `with_timeout(..)` | 10 seconds |
| `reporting_to(..)` | nothing reported; a failing watch is invisible |
| `from_client(..)` | opens its own connection |
| `with_tls(..)` *(constructor, `tls` feature)* | the platform trust store, no client certificate |

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
