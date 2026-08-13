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

**Several keys, one document:** `Keys::several(["base.json", "local.json"])`
reads both and merges them in call order — later wins. The KV API has no
batch read, so that is **one get per key and is not atomic**, and one
unreadable key fails the whole fetch.

There is deliberately **no prefix form**, and this one is the client's doing
rather than a preference: `Store::keys()` is the only listing `async-nats`
exposes and it walks the whole bucket, with the filtered constructor kept
private. A prefix would therefore be a full-bucket scan wearing a prefix's
name — the 512-key bound would be a bound on the bucket, and a bucket of a
hundred thousand keys would stream a hundred thousand headers to find
three. Name the keys, or give the set its own bucket, which is the
partition NATS actually offers.

**Connecting:** every credential NATS understands — token, user/password,
NKey, JWT, a `.creds` file, TLS — lives on its own `ConnectOptions`,
re-exported. Unlike the gRPC stores this connects *eagerly*, so an
unreachable server is a construction failure. `from_client` reuses a
connection the program already holds; `from_store` goes one further for a
program that already has the `Store`.

**TLS as data:** `Nats::with_tls(server, bucket, key, options, tls)` takes
the same `TlsConfig` the rest of the family does — with one honest gap.
`async-nats` opens the files itself, so the **PEM-bytes spellings are
refused**, naming the call and pointing at the file spellings; they are not
ignored, because a caller who supplied an authority and got the platform
trust store has a program that believes it is pinned and is not. Naming a
CA also sets `require_tls`, so a `nats://` URL fails rather than quietly
negotiating plaintext. [TLS, and the one vocabulary all eight speak](../remote-stores.md#tls-and-the-one-vocabulary-all-eight-speak)

**Reconnecting is the client's job, and it does it:** `async-nats`
reconnects indefinitely and re-establishes subscriptions, so this crate
deliberately adds no retry layer on top. Two consequences: a `fetch`
during a disconnect fails rather than hanging, and a `watch` survives a
reconnect silently — which is why the watch *ending at all* is an error.

**Watching:** the bucket is a stream, so `watch` is a future — spawn it,
drop it to cancel. No startup delivery, deletes and purges are not
changes, and it never returns `Ok`. A **multi-key source refuses to be
watched**: `watch_many` could say the set moved, but nothing here could
then re-read the set as of one instant.

**A failing watch says so:** `reporting_to(sink)` hands the loop the same
`RemoteSink` it already delivers through, and every attempt that comes back
with nothing is reported to it — the watch that could not be established, the
stream erroring, a value that is not a document, and the stream closing.
Without it a watch is the half of a store `dynamic-config` cannot see: only
deliveries are recorded, so `dynamic_config_remote_up` describes the last
*delivery* rather than the last *attempt*, and a store that stopped answering
an hour ago reads as healthy until something calls `refresh_remote_async()`.
What a failure moves is narrow on purpose — the streak and the last failure,
never the staleness clock — so `up` goes to zero while
`remote_last_fetch_seconds` keeps ageing, which is the pair an alert wants.
See [telemetry](../telemetry.md).

Worth knowing what that does *not* cover here, because it follows from the
paragraph above: a server that goes away is not a failed watch. `async-nats`
keeps recreating the subscription for as long as it takes, so the loop waits
rather than fails — which is the behaviour a program wants and the reason
this crate adds no retry layer. What reaches the loop is a stream that
stopped: a deleted bucket, a consumer that is gone, a value that is not a
document.

The [README](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-nats)
carries the full story and the `nats_watching` example; MSRV 1.88.
