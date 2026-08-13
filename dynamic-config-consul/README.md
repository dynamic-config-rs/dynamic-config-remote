# dynamic-config-consul

Read [`dynamic-config`] configuration from Consul's key/value store.

```toml
[dependencies]
dynamic-config = "0.6.0"
dynamic-config-consul = "0.6.0"
```

```rust
use dynamic_config_consul::{Auth, Consul};

DbConfig::set_remote(
    Consul::new("http://consul.internal:8500", "myapp/db.json")
        .with_auth(Auth::from_environment()),
);

// Fetching is explicit; the load that follows touches no network.
DbConfig::refresh_remote()?;
DbConfig::builder("db").init()?;
```

Consul's KV API is plain HTTP, so this implements the **blocking**
`RemoteSource` trait: nothing here needs an async runtime, and neither does
using it.

## What it reads

`GET {address}/v1/kv/{key}`, base64-decoding the single `Value` Consul returns.
**The stored value is a whole configuration document** — the same bytes that
would be in a config file — so the format comes from the key's extension, or
from `with_format`.

That is the opposite of [`dynamic-config-vault`], which wraps a secret's fields
under a section key. Vault stores a map of named secrets; Consul stores an
opaque blob; each is easiest to use as what it already is.

## Several keys as one document

```rust
use dynamic_config_consul::{Consul, Keys};

// Named keys: a list of layers, merged in call order — later wins.
Consul::new(address, Keys::several(["myapp/base.json", "myapp/local.json"]));

// A prefix: disjoint sections, and an overlap between two of them is an error
// naming both keys and the paths.
Consul::new(address, Keys::prefix("myapp/")).with_format(Format::Json);
```

| | Requests | Consistency | Ceiling |
|---|---|---|---|
| `Keys::several` | one **per key** — Consul's KV API has no batch read of a caller-chosen set | **not atomic** | the caller's list |
| `Keys::prefix` | one `?recurse` | one index | 512 keys |

Consul's transaction endpoint could read a named list in one request, at the
price of a write-shaped request and a sixty-four operation ceiling; that trade
is recorded rather than taken. Prefer a prefix where the keys are disjoint
anyway. A key ending in `/` with no value is a Consul folder and is skipped
rather than reported as a missing document.

One unreadable key fails the whole fetch, naming it. Provenance becomes
store-grained: the merged document is one layer, so `source_of` names the store
and the set rather than which key supplied a value. A **prefix can be watched**
and a **named list cannot**: a recursive blocking query's answer *is* the
subtree at one index, so the watch re-reads nothing at all, while a list has no
batch read to block on. A list refuses at `watch()` and says so; poll
`refresh_remote()` on a timer.

## Getting a token

| Method | Constructor | For |
|---|---|---|
| None | `Auth::Anonymous` | a Consul with ACLs off, which is ordinary in development |
| Token | `Auth::token(..)` | whatever the operator issued |
| Environment | `Auth::from_environment()` | `CONSUL_HTTP_TOKEN`, falling back to anonymous |
| Kubernetes | `Auth::kubernetes(method)` | a pod, with no secret to distribute |
| JWT / OIDC | `Auth::jwt(method, token)` | anything with a signed identity token |

The last two log in at `/v1/acl/login`, presenting a bearer token to a named
auth method. Consul's `Meta` is carried through for the audit log:

```rust
Auth::kubernetes("kubernetes").with_meta("pod", std::env::var("HOSTNAME")?)
```

**Logging in is lazy**: building a `Consul` reaches nothing, and the first read
does it. **A Kubernetes bearer token is re-read at every login**, because the
kubelet rotates projected service-account tokens and a copy taken at startup
expires with the pod still running.

`Auth::from_environment()` falls back to anonymous rather than failing when the
variable is unset — a Consul with ACLs disabled is exactly what the convenience
is useful for, and failing there would make it useless.

## Expiry

Consul has no renewal: it issues login tokens with an expiry and expects you to
log in again. So a token within thirty seconds of expiry is replaced, and a `403`
replaces one early — once, not in a loop, because a second refusal means the
policy is wrong and retrying would turn a clear failure into a hang.

`Auth::token` is the one variant that cannot recover on its own; there are no
credentials here to log in again with.

A `403` that survives that one retry is reported as `ErrorKind::Auth` rather
than `ErrorKind::Remote`. The difference is what a watch loop needs: an
unreachable agent comes back, and a wrong policy does not. Consul uses `403`
and nothing else for an ACL refusal, so there is no `401` case here — a `401`
in front of a Consul is a proxy, and a proxy's verdict is not the store's.

## Timeouts

`with_timeout(..)` is **the deadline for a single fetch attempt, excluding
retries the underlying client performs** — the same sentence every store in
this family answers to. Ten seconds by default, and `ureq` performs no retries
of its own, so the deadline is the whole story.

A blocking query is the exception, and deliberately so: `watch` sizes its own
client timeout from `with_wait` plus this one plus the jitter Consul adds, or
every held-open query would end as a client timeout instead of an answer.

An HTTP client supplied through `with_agent` brings its own timeout, which
applies instead.

## Watching

Consul cannot push, but it can hold a request open until something changes — a
*blocking query*. `watch` is that loop, and it is genuinely change-driven rather
than a poll with extra steps: the agent answers the moment the key moves.

```rust
let watch = RemoteWatch::new();
let watching = watch.watching();

std::thread::spawn(move || consul.watch(&watching, move |document| sink.apply(document)));

// Dropping `watch` — or calling `watch.stop()` — ends the loop.
```

- The current value is **not** delivered at startup. The first query carries
  index 0 and Consul answers it immediately with whatever is stored; that value
  primes the index and reports nothing, the same way a file watcher does not
  announce an edit when it starts.
- **An identical write is not reported twice.** Consul bumps its index on every
  write, including one that changed nothing.
- A failed query does not end the watch — the agent restarting, a network blip,
  or a key that does not exist *yet* are all what a watch is there to survive.
- Surviving it is not the same as hiding it. `reporting_to(sink)` — the same
  sink the callback applies documents through — records every attempt that came
  back with nothing, so `dynamic_config_remote_up` reports the last *attempt*
  rather than the last delivery. Without it, a loop that has been erroring for
  an hour goes on looking healthy.
- Stopping is bounded by `with_wait`, one minute by default. Longer means fewer
  requests and a slower exit; that trade is yours to make.

## TLS

`with_tls` takes the same data-only `TlsConfig` every store in this family
takes — a certificate authority and a client certificate, each as a file path
or as PEM bytes, with no `ureq` type in the calling code:

```rust
use dynamic_config_consul::{Consul, TlsConfig};

let consul = Consul::new("https://consul.internal:8501", "myapp/db.json")
    .with_tls(
        TlsConfig::new()
            .with_ca_certificate_file("/etc/consul.d/consul-agent-ca.pem")
            .with_client_certificate_files(
                "/etc/consul.d/client.crt",
                "/etc/consul.d/client.key",
            ),
    );
```

Consul expresses all of it, and no feature has to be turned on: `ureq` already
carries rustls. Consul's own agent CA — `consul tls ca create` — is exactly the
case this exists for: an authority no public trust store has heard of. It
reaches the blocking query too, which builds its own client with a longer
timeout.

Nothing is read at build time: a missing certificate is an error naming the
path. There is no way to turn verification off, and the book's [remote stores chapter](https://github.com/ctolon/dynamic-config/blob/main/book/src/remote-stores.md#tls-and-the-one-vocabulary-all-seven-speak) argues
that one.

## Bringing your own HTTP client

```rust
Consul::new(address, "myapp/db.json").with_agent(agent)
```

For a caller with its own proxy settings, a private CA, a client certificate, or
a connection pool it would rather not have a second copy of. The agent's own
timeout applies — including to the long blocking query `watch` issues, so an
agent used for watching needs a timeout above `with_wait`.

## Builders

| Method | Default |
|---|---|
| `with_format(..)` | from the key's extension |
| `with_auth(..)` / `with_token(..)` | anonymous |
| `with_datacenter(..)` | the agent's own |
| `with_timeout(..)` | 10 seconds |
| `with_wait(..)` | 60 seconds (blocking queries; Consul's ceiling is 10 minutes) |
| `reporting_to(..)` | nobody — a watch's failed attempts are recorded nowhere |
| `with_agent(..)` | one built per source |
| `with_tls(..)` | the platform trust store, no client certificate |

## Example

| Example | Shows |
|---|---|
| [`consul_kubernetes`](examples/consul_kubernetes.rs) | Logging in against an auth method, reading a document, and watching with a blocking query. |

It needs a server, and its own doc comment says how to start one in a container
and put a document in it.

```sh
cargo run -p dynamic-config-consul --example consul_kubernetes
```

## Testing

The test suite drives a **real Consul in a container** — no mocks, including one
started with ACLs enabled and `default_policy = deny`. That is how the
first-query behaviour above was found: the initial version reported the starting
value as a change, so beginning to watch looked like an edit.

```sh
cargo test -p dynamic-config-consul    # needs a working Docker daemon
```

## MSRV

1.85 — higher than [`dynamic-config`]'s own 1.71, because an HTTP client stack
and a container-driving test harness both move faster than that crate wants to.
A companion pays for what it pulls in; the core stays where it is.

## License

MIT

[`dynamic-config`]: https://docs.rs/dynamic-config
[`dynamic-config-vault`]: https://docs.rs/dynamic-config-vault
