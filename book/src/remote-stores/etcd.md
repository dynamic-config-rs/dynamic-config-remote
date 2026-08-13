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

**Several keys as one document:** `Keys::several([..])` merges named keys
in call order — later wins, the rule `.file(..)` already teaches — and
`Keys::prefix("myapp/")` merges the sections under a prefix, where an
overlap between two of them is an error naming both keys and the paths.
Both are one round trip and both are read at a single etcd revision: a
list goes as a transaction of range reads, a prefix as one range read, so
a write landing mid-read cannot tear the document. A list is capped at
etcd's own `--max-txn-ops` (128); a prefix is capped at 512 keys. See
[several keys as one document](../remote-stores.md#several-keys-as-one-document)
for what this costs in provenance and in watching.

**Connecting:** credentials and TLS go through etcd's own
`ConnectOptions`, re-exported so there is no second vocabulary and no
direct `etcd-client` dependency. TLS types sit behind the crate's `tls`
feature (a private-network deployment has no use for the stack), and
`tls-roots` adds the platform's root store. `from_client` shares a
connection the program already has. One honest sharp edge: **the client
connects lazily**, so an unreachable etcd surfaces on the first read, not
at construction.

**TLS as data:** `Etcd::with_tls(endpoints, keys, options, tls)` takes a
private certificate authority and a client certificate as paths or PEM
bytes, with no `tonic` type in the calling code — the same `TlsConfig`
every store in this family takes. Behind the `tls` feature, because that
is what buys the stack. `ConnectOptions` still carries everything that is
not TLS, and the `TlsConfig` owns the TLS slot. mTLS matters more here
than elsewhere: an etcd started with `--client-cert-auth` is the ordinary
hardened deployment. [TLS, and the one vocabulary all eight speak](../remote-stores.md#tls-and-the-one-vocabulary-all-eight-speak) is the whole story, including
why there is no way to turn verification off.

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

**A prefix can be watched; a named list cannot.** One stream over one range
says *the range moved* and carries the revision it moved at, and one range
read at that revision is the whole set as of one instant — so the document
delivered is a state the cluster really was in. A named list would be one
stream per key, and nothing about N independent streams says the set moved
together, so it is refused at `watch()`. Under a prefix a deletion *is* a
change, because the set is what is left after it:
[spurious, never torn](../remote-stores.md#spurious-never-torn).

**A failing watch says so:** `reporting_to(sink)` hands the loop the same
`RemoteSink` it already delivers through, and every attempt that comes back
with nothing is reported to it — the stream erroring, etcd cancelling the
watch, the range read at an event's revision failing, a document that will
not merge, and the connection closing under it. Without it a watch is the
half of a store `dynamic-config` cannot see: only deliveries are recorded,
so `dynamic_config_remote_up` describes the last *delivery* rather than the
last *attempt*, and a store that stopped answering an hour ago reads as
healthy until something calls `refresh_remote_async()`. What a failure moves
is narrow on purpose — the streak and the last failure, never the staleness
clock — so `up` goes to zero while `remote_last_fetch_seconds` keeps ageing,
which is the pair an alert wants. See [telemetry](../telemetry.md).

**A replaced auth token is not a failure.** etcd's simple tokens expire, so a
watch that outlives one logs in again and resumes from the last delivered
revision; nothing is reported for that, because the store answered and no
event was lost. Reporting it would drive `up` to zero every time a
five-minute token turned over on a healthy cluster — and, since only a
delivery or a fetch clears the streak, it would stay there until the next
change. A re-authentication that *fails*, a stream that will not
re-establish, and a recovery cap that runs out all report.

The [README](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-etcd)
carries the full story, the builder-defaults table and the
`etcd_watching` example; MSRV 1.85.
