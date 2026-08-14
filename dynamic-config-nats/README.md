# dynamic-config-nats

Read [`dynamic-config`] configuration from a NATS JetStream key/value bucket.

```toml
[dependencies]
dynamic-config = { version = "0.6.1", features = ["async"] }
dynamic-config-nats = "0.6.1"
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

## Several keys as one document

```rust
use dynamic_config_nats::{Keys, Nats};

// Named keys: a list of layers, merged in call order — later wins.
Nats::new(server, "config", Keys::several(["base.json", "local.json"])).await?;
```

| | Requests | Consistency | Ceiling |
|---|---|---|---|
| `Keys::several` | one get **per key** — the KV API has no batch read | **not atomic** | the caller's list |

**There is deliberately no prefix form**, and this one is the client's doing
rather than a preference: `Store::keys()` is the only listing `async-nats`
exposes and it walks the whole bucket (`$KV.{bucket}.>`, headers only), with the
filtered-consumer constructor kept private. A prefix would therefore be a
full-bucket scan wearing a prefix's name — the 512-key bound would be a bound on
the *bucket*, and a bucket of a hundred thousand keys would stream a hundred
thousand headers to find three. Name the keys, or give the set its own bucket,
which is the partition NATS actually offers.

One unreadable key fails the whole fetch, naming it. Provenance becomes
store-grained: the merged document is one layer, so `source_of` names the store
and the set rather than which key supplied a value. A multi-key source refuses to
be watched — `watch_many` could say the set moved, but nothing here could then
re-read the set as of one instant — so poll `refresh_remote_async()` on a timer.

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

## TLS

`with_tls` takes the same data-only `TlsConfig` every store in this family
takes, with no `async-nats` type in the calling code:

```rust
use dynamic_config_nats::{ConnectOptions, Nats, TlsConfig};

let nats = Nats::with_tls(
    "tls://nats.internal:4222",
    "config",
    "db.json",
    ConnectOptions::new(),
    &TlsConfig::new()
        .with_ca_certificate_file("/etc/nats/ca.pem")
        .with_client_certificate_files("/etc/nats/client.crt", "/etc/nats/client.key"),
)
.await?;
```

**NATS is the one store here that cannot express the whole of it.**
`async-nats` opens the files itself, and the only byte-taking door is a
hand-built `rustls::ClientConfig` — a direct `rustls` dependency and a
crypto-provider decision, for one spelling. So `with_ca_certificate_pem` and
`with_client_certificate_pem` are **refused**, naming the call and pointing at
the file spellings. They are not ignored: a caller who supplied an authority
and got the platform trust store has a program that believes it is pinned and
is not. Writing the material to a temporary file is deliberately not done — it
would put a private key on a disk that never asked for one.

Naming a certificate authority also sets `require_tls`, so a `nats://` URL
fails rather than quietly negotiating plaintext. There is no way to turn
verification off, and the book's [remote stores chapter](https://github.com/ctolon/dynamic-config/blob/main/book/src/remote-stores.md#tls-and-the-one-vocabulary-all-seven-speak) argues that one.

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

## Errors

A credential the server refuses — a nonce it will not sign for, an authorization
violation — is reported as `ErrorKind::Auth` rather than `ErrorKind::Remote`,
and it happens at construction. A later read refused for want of permission
arrives as an undifferentiated KV error and stays `Remote`: guessing there would
stop a watch loop that a reconnect would have fixed.

A credential in the *URL* — `nats://token@host:4222` is a shape NATS accepts —
is redacted before the address is stored, because the address is quoted into
every error message and into `Debug`.

## Timeouts

`with_timeout(..)` is **the deadline for a single fetch attempt, excluding
retries the underlying client performs** — the same sentence every store in
this family answers to. Ten seconds by default.

It is the second of two, and they cover different halves.
`ConnectOptions::request_timeout`, passed through `with_options`, bounds the
client's own requests and is set before there is a connection to bound. This one
wraps the KV read, so a server that accepted the request and then went quiet
ends the fetch rather than parking it.

Neither applies to `watch`, which is long-lived on purpose.

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

### A failing watch says so

`reporting_to(sink)` hands the loop the same `RemoteSink` it already delivers
through, and every attempt that comes back with nothing is reported to it: the
watch that could not be established, the stream erroring, a value that is not a
document, and the stream closing.

```rust
let sink = DbConfig::remote_sink();

nats.reporting_to(sink).watch(move |document| sink.apply(document)).await
```

Without it a watch is the half of a store `dynamic-config` cannot see: only
deliveries are recorded, so `dynamic_config_remote_up` reports the last
*delivery* rather than the last *attempt*. A failure moves the failure streak
and the last failure and nothing else, so `remote_up` goes to zero while
`remote_last_fetch_seconds` keeps ageing — the pair an alert wants.
`on_change`'s own refusal is not reported: the store answered, `apply` counted
the delivery, and a document that would not install is `ConfigStatus`'s half of
the picture.

What this covers follows from the section above: **a server that goes away is
not a failed watch**, because `async-nats` keeps recreating the subscription for
as long as it takes and the loop waits through it. What reaches this crate is a
stream that stopped — a deleted bucket, a consumer that is gone.

## Builders

| Method | Default |
|---|---|
| `with_format(..)` | from the key's extension |
| `with_options(..)` | `ConnectOptions::new()` — no credentials |
| `with_timeout(..)` | 10 seconds |
| `reporting_to(..)` | nothing reported; a failing watch is invisible |
| `from_client(..)` / `from_store(..)` | opens its own connection |
| `with_tls(..)` *(constructor)* | the platform trust store, no client certificate; **file paths only** |

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
