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

**Getting a token:** `Auth::Anonymous` (ACLs off — ordinary in
development), `Auth::token(..)`, `Auth::from_environment()`
(`CONSUL_HTTP_TOKEN`, falling back to anonymous rather than failing —
that fallback is the convenience's whole point), `Auth::kubernetes(..)`
and `Auth::jwt(..)` (both log in at `/v1/acl/login`; `with_meta` feeds the
audit log). Logging in is lazy, and a Kubernetes bearer token is re-read
at every login because the kubelet rotates it.

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

The [README](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-consul)
carries the full story, the builder-defaults table and the
`consul_kubernetes` example; MSRV 1.85.
