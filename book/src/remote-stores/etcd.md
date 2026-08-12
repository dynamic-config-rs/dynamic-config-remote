# etcd

[`dynamic-config-etcd`](https://docs.rs/dynamic-config-etcd) reads
configuration from an etcd v3 key/value store. etcd speaks gRPC, so its
client is async and this crate implements **`AsyncRemoteSource`** —
pretending otherwise would only hide a `block_on` inside it.

```toml
[dependencies]
dynamic-config = { version = "<version>", features = ["async"] }
dynamic-config-etcd = "<version>"
```

```rust
use dynamic_config_etcd::Etcd;

DbConfig::set_remote_async(
    Etcd::new(["http://etcd.internal:2379"], "myapp/db.json").await?,
);
DbConfig::refresh_remote_async().await?;   // the round trip, explicitly
```

**What it reads:** one key holding a whole configuration document — the
same bytes a config file would hold; the format comes from the key's
extension, or `with_format`.

**Connecting:** credentials and TLS go through etcd's own
`ConnectOptions`, re-exported so there is no second vocabulary and no
direct `etcd-client` dependency. TLS types sit behind the crate's `tls`
feature (a private-network deployment has no use for the stack), and
`tls-roots` adds the platform's root store. `from_client` shares a
connection the program already has. One honest sharp edge: **the client
connects lazily**, so an unreachable etcd surfaces on the first read, not
at construction.

**Expiry:** etcd's auth tokens carry a TTL (five minutes by default). On
`invalid auth token` the crate logs in again and retries — once; a fresh
token refused too means the policy is wrong, and retrying would turn a
clear failure into a hang.

**Watching:** a real push stream — `watch` is a future the caller spawns
and cancels by dropping, on any executor. The current value is not
delivered at startup, a deletion is not a change, and the future never
returns `Ok`: a watch either runs or has failed (compaction and a closed
connection are the usual reasons) — loop around it to reconnect. A token
refresh that works resumes from the last delivered revision.

The [README](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-etcd)
carries the full story, the builder-defaults table and the
`etcd_watching` example; MSRV 1.85.
