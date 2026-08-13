# Consul

[`dynamic-config-consul`](https://docs.rs/dynamic-config-consul) reads
configuration from Consul's KV store. The API is plain HTTP, so this is
the **blocking** `RemoteSource`: nothing here needs an async runtime, and
neither does using it.

```toml
[dependencies]
dynamic-config = "<version>"
dynamic-config-consul = "<version>"
```

```rust
use dynamic_config_consul::{Auth, Consul};

DbConfig::set_remote(
    Consul::new("http://consul.internal:8500", "myapp/db.json")
        .with_auth(Auth::from_environment()),
);
DbConfig::refresh_remote()?;
```

**What it reads:** one key, base64-decoded, holding a whole configuration
document — the opposite of [Vault](vault.md), which wraps a map of fields;
each store is used as what it already is.

**Several keys as one document:** `Keys::several([..])` merges named keys
in call order — later wins, the rule `.file(..)` already teaches — and
`Keys::prefix("myapp/")` merges the sections under a prefix, where an
overlap between two of them is an error naming both keys and the paths. A
prefix is one `?recurse` request answered at one index, so the subtree is
consistent; a named list is one request *per key*, because Consul's KV API
has no batch read of a caller-chosen set, and is therefore not atomic.
Prefer a prefix where the keys are disjoint anyway. A key ending in `/`
with no value is a Consul folder and is skipped, not treated as a missing
document. See
[several keys as one document](../remote-stores.md#several-keys-as-one-document)
for what this costs in provenance and in watching.

**Getting a token:** `Auth::Anonymous` (ACLs off — ordinary in
development), `Auth::token(..)`, `Auth::from_environment()`
(`CONSUL_HTTP_TOKEN`, falling back to anonymous rather than failing —
that fallback is the convenience's whole point), `Auth::kubernetes(..)`
and `Auth::jwt(..)` (both log in at `/v1/acl/login`; `with_meta` feeds the
audit log). Logging in is lazy, and a Kubernetes bearer token is re-read
at every login because the kubelet rotates it.

**TLS as data:** `.with_tls(TlsConfig::new().with_ca_certificate_file(..))`
takes a private certificate authority and a client certificate as paths or
PEM bytes, with no `ureq` type in the calling code. Consul's own agent CA —
`consul tls ca create` — is exactly the case: an authority no public trust
store has heard of. No feature to turn on; `ureq` already carries rustls.
Setting `with_agent` as well is refused at the first request rather than
resolved. [TLS, and the one vocabulary all eight speak](../remote-stores.md#tls-and-the-one-vocabulary-all-eight-speak)

**Expiry:** Consul does not renew — it expects a fresh login. A token
within thirty seconds of expiry is replaced proactively; a `403` replaces
one early, once. `Auth::token` is the variant that cannot recover — there
are no credentials to log in again with.

**Watching:** a *blocking query* — genuinely change-driven, not a poll
with extra steps: the agent holds the request open and answers the moment
the key moves. It runs on a thread and takes a `Watching` stop token; the
first query primes the index and reports nothing, an identical rewrite is
not reported twice, and a failed query retries rather than ending the
loop. Stopping is bounded by `with_wait` (a minute by default — fewer
requests, slower exit; the trade is yours). An agent supplied via
`with_agent` needs its own timeout *above* `with_wait`, or every blocking
query dies as a client timeout.

**A prefix can be watched; a named list cannot.** A prefix watch is the
cheapest correct watch on a set anywhere in this family, and the only one
that re-reads nothing: a recursive blocking query's *answer is the subtree
at one index*, so the document is folded from the very bytes the agent
blocked to send, and there is no window between "the set changed" and "read
the set". A named list has no batch read to fold — Consul's KV API has
none — so it would mean blocking on one key and then reading the others,
and it is refused at `watch()`:
[spurious, never torn](../remote-stores.md#spurious-never-torn).

**A failing watch says so:** `.reporting_to(sink)` hands the loop the same
`RemoteSink` its callback applies documents through, and every attempt that
comes back with nothing is recorded on it — a blocking query that errored, a
watched key that holds no value, a subtree that cannot be folded into a
document. Without it, `dynamic_config_remote_up` reports the last *delivery*
rather than the last *attempt*, and the two look identical from outside: a key
nobody changed today and an agent that stopped answering an hour ago both
deliver nothing, and only one of them is healthy. A failed attempt moves the
failure streak and nothing else, so
`dynamic_config_remote_last_fetch_seconds` keeps ageing while `remote_up` goes
to zero — the pair that says both *the agent is not answering* and *how stale
what it last said has become*. Reporting is infallible and silent: a loop is
never handed a failure to report a failure. A `fetch()` needs none of it,
because a fetch already records itself.
[The remote store's own numbers](../telemetry.md#the-remote-stores-own-numbers)

The [README](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-consul)
carries the full story, the builder-defaults table and the
`consul_kubernetes` example; MSRV 1.85.
