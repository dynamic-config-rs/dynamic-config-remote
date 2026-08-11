# dynamic-config-consul

Read [`dynamic-config`] configuration from Consul's key/value store.

```toml
[dependencies]
dynamic-config = "0.2.0"
dynamic-config-consul = "0.2.0"
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

## Watching

Consul cannot push, but it can hold a request open until something changes — a
*blocking query*. `watch` is that loop, and it is genuinely change-driven rather
than a poll with extra steps: the agent answers the moment the key moves.

```rust
let watch = RemoteWatch::new();
let watching = watch.watching();

std::thread::spawn(move || consul.watch(&watching, DbConfig::apply_remote));

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
- Stopping is bounded by `with_wait`, one minute by default. Longer means fewer
  requests and a slower exit; that trade is yours to make.

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
| `with_agent(..)` | one built per source |

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
