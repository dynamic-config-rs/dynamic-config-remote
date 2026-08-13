# dynamic-config-vault

Read [`dynamic-config`] configuration from HashiCorp Vault's KV v2 store.

```toml
[dependencies]
dynamic-config = "0.6.0"
dynamic-config-vault = "0.6.0"
```

```rust
use dynamic_config_vault::{Auth, Vault};

DbConfig::set_remote(
    Vault::new("https://vault.internal:8200", "secret", "myapp/db")
        .with_auth(Auth::kubernetes("myapp")),
);

// Fetching is explicit; the load that follows touches no network.
DbConfig::refresh_remote()?;
DbConfig::builder("db").init()?;
```

Vault's API is plain HTTP, so this implements the **blocking** `RemoteSource`
trait: nothing here needs an async runtime, and neither does using it.

## What it reads

`GET {address}/v1/{mount}/data/{path}`, and takes `data.data` — the value half
of a KV v2 response. That object becomes the configuration document, so a secret
stored as `{"host": "db", "port": 5432}` maps onto a struct with those fields.

The document is handed over with the section key wrapped around it, because
Vault stores a section's *contents* rather than a whole configuration file.
That is the opposite of [`dynamic-config-consul`] and
[`dynamic-config-nats`], and the difference is not a whim: Vault stores a map of
named fields, a KV bucket stores an opaque blob, and each is easiest to use as
what it already is.

| Crate | Stores | Natural unit |
|---|---|---|
| this one | a map of named fields | the field |
| Consul, NATS | opaque bytes | the whole document |

## Several paths as one section

```rust
use dynamic_config_vault::{Keys, Vault};

// Merged in call order — later wins — and all under the one section key.
Vault::new(address, "secret", Keys::several(["myapp/db-defaults", "myapp/db-credentials"]));
```

| | Requests | Consistency | Ceiling |
|---|---|---|---|
| `Keys::several` | one **per path** — KV v2 has no batch read of a caller-chosen set | **not atomic** | the caller's list |

A list is layering, and it is the shape Vault's own access control produces: a
policy applies to a path, so splitting a section into a public half and a
restricted half is something only Vault can do. The price is one request per
path per fetch, and every one of those is a line in the audit log.

**There is deliberately no prefix form**, and `LIST` is not what is missing.
Folding a subtree into one section would make `myapp/db` and `myapp/server`
collide on `host` — the ordinary layout, refused — and naming a sub-section
after each secret's path would invent a convention no other store in this family
has, and would make a list of one path mean something different from one path. A
deployment that wants several sections installs one source per section, which is
what it did before.

One unreadable path fails the whole fetch, naming it. Provenance becomes
store-grained: the merged section is one layer, so `source_of` names the store
and the set rather than which path supplied a value. A multi-path source refuses
to be watched — the version counter that watch polls belongs to one secret, and
a set of them has none — so poll `refresh_remote()` on a timer.

## Logging in

Every Vault auth method ends in the same place — a client token with a lease — so
that is all `Auth` models.

| Method | Constructor | For |
|---|---|---|
| Token | `Auth::token(..)` | a token somebody already obtained |
| AppRole | `Auth::app_role(role_id, secret_id)` | a service outside Kubernetes |
| Kubernetes | `Auth::kubernetes(role)` | a pod, with no secret to distribute |
| JWT / OIDC | `Auth::jwt(token)` | anything with a signed identity token |
| Userpass | `Auth::userpass(user, password)` | operators, and development |
| LDAP | `Auth::ldap(user, password)` | a directory that already exists |
| TLS certificate | `Auth::certificate()` | a client certificate, presented by your own agent |

Mount the method wherever it lives, and name a role when the mount needs one:

```rust
Auth::app_role(role_id, secret_id).at_mount("approle-prod")
Auth::jwt(token).with_role("readers")
```

### A private CA, and a client certificate, without naming a `ureq` type

`with_tls` takes the same data-only `TlsConfig` every store in this family
takes — which is what a Vault behind an internal CA needs, and what
`VAULT_CACERT` already names:

```rust
use dynamic_config_vault::{TlsConfig, Vault};

let vault = Vault::new("https://vault.internal:8200", "secret", "myapp/db")
    .with_token(std::env::var("VAULT_TOKEN")?)
    .with_tls(
        TlsConfig::new()
            .with_ca_certificate_file("/etc/ssl/private-ca.pem")
            .with_client_certificate_files("/etc/ssl/app.crt", "/etc/ssl/app.key"),
    );
```

Vault expresses all of it, and no feature has to be turned on: `ureq` already
carries rustls. `with_agent` is still there for a proxy, a connection pool or
an option this crate has never heard of — but `with_agent` **and** `with_tls`
together are refused at the first request, because an agent already carries a
complete TLS configuration and applying a second one could only mean discarding
one of them.

Nothing is read at build time: a missing certificate is an error naming the
path. There is no way to turn verification off, and the book's [remote stores chapter](https://github.com/ctolon/dynamic-config/blob/main/book/src/remote-stores.md#tls-and-the-one-vocabulary-all-seven-speak) argues
that one. `Auth::certificate()` is a different thing and still means what it
did: Vault's `cert` *login* method, which authenticates with the certificate
the connection already presents.

**Logging in is lazy.** Building a `Vault` reaches nothing; the first read logs
in. Constructing a source is not I/O, and configuration that hits the network on
a call nobody expected to block is how a startup ends up mysteriously slow.

**Kubernetes tokens are re-read at every login**, not cached at startup — the
kubelet rotates projected service-account tokens, and a copy taken at startup
expires with the pod still running.

## Expiry is handled twice, on purpose

**Before the request**, a token within thirty seconds of its expiry is renewed,
or replaced by a fresh login if it cannot be renewed. This is the path that
should normally fire.

**After the request**, a `403` is treated as *the token stopped working* and
triggers exactly one fresh login and retry. Clocks skew, Vault revokes, a lease
turns out shorter than it said — the proactive path cannot catch all of that, and
a configuration reader that gives up on the first `403` will eventually do so at
three in the morning.

Once, not in a loop: if a fresh token is also refused, the problem is the policy
rather than the lease, and retrying would only turn a clear failure into a hang.

`Auth::Token` is the one variant that cannot recover on its own — there are no
credentials to log in again with. A renewable token is still renewed.

A `403` that survives that one retry is reported as `ErrorKind::Auth` rather
than `ErrorKind::Remote`, and so is a login Vault refuses and a source with no
credentials at all. The difference is what a watch loop needs: a sealed Vault
un-seals, and a wrong policy does not.

## Timeouts

`with_timeout(..)` is **the deadline for a single fetch attempt, excluding
retries the underlying client performs** — the same sentence every store in
this family answers to. Ten seconds by default, and `ureq` performs no retries
of its own, so the deadline is the whole story.

It bounds each request the watch loop makes — the metadata check and the read
that follows a version change — not the loop, which is meant to run until it is
stopped.

An HTTP client supplied through `with_agent` brings its own timeout, which
applies instead.

## Watching

Vault is the one store [`dynamic-config`] talks to that cannot say when
something changed: no watch, no blocking query, no stream. So `watch` polls, and
says so rather than dressing a timer up as a subscription.

What it does *not* do is pull the secret every tick. KV v2 keeps a version
counter in its metadata, so each tick reads `{mount}/metadata/{path}` for
`current_version` and only reads the secret when that number moves. A secret that
has not changed is never transferred, never decrypted, and never written to an
audit log as a read.

```rust
let watch = RemoteWatch::new();
let watching = watch.watching();

std::thread::spawn(move || {
    vault.watch(&watching, Duration::from_secs(30), move |document| sink.apply(document))
});

// Dropping `watch` — or calling `watch.stop()` — ends the loop.
```

- The current value is **not** delivered at startup: a watch reports changes, and
  announcing the value the caller already has would make every restart look like
  an edit. Fetch first if the starting value matters.
- A failed check does not end the watch. An expired token, a sealed Vault, a
  network blip — that is what a watch is there to survive.
- Surviving it is not the same as hiding it. `reporting_to(sink)` — the same
  sink the callback applies documents through — records every tick that came
  back with nothing, so `dynamic_config_remote_up` reports the last *attempt*
  rather than the last delivery. This is the store where that matters most: a
  secret nobody rewrote and a Vault that sealed itself yesterday deliver the
  same nothing.
- Stopping is noticed within a quarter second whatever the interval is, so a
  sixty-second poll does not mean a sixty-second exit.

## Bringing your own HTTP client

```rust
Vault::new(address, "secret", "myapp/db").with_agent(agent)
```

For a caller with its own proxy settings, a private CA, a client certificate, or
a connection pool it would rather not have a second copy of. This is also how
`Auth::Certificate` gets its certificate: the agent presents it, and the `Auth`
variant only tells Vault to log in with it.

## Builders

| Method | Default |
|---|---|
| `with_key(..)` | `"db"` — must match the key given to `builder(..)` |
| `with_auth(..)` / `with_token(..)` | none; the first read says so |
| `with_namespace(..)` | none (Vault Enterprise) |
| `reporting_to(..)` | nobody — a watch's failed attempts are recorded nowhere |
| `with_timeout(..)` | 10 seconds |
| `with_agent(..)` | one built per source |
| `with_tls(..)` | the platform trust store, no client certificate |

## Example

| Example | Shows |
|---|---|
| [`vault_kubernetes`](examples/vault_kubernetes.rs) | Logging in the way a pod does, reading a secret, and watching it by version rather than by re-reading it. |

It needs a server, and its own doc comment says how to start one in a container
and put a document in it.

```sh
cargo run -p dynamic-config-vault --example vault_kubernetes
```

## Testing

The test suite drives a **real Vault in a container** — no mocks. That is how
three of the behaviours above got pinned down rather than assumed: `role-id` is a
GET while `secret-id` is a POST, a destroyed secret id does not invalidate a
token already issued, and a lease shorter than the refresh window exercises the
whole expiry path without waiting for a real token to age out.

```sh
cargo test -p dynamic-config-vault    # needs a working Docker daemon
```

## MSRV

1.85 — higher than [`dynamic-config`]'s own 1.71, because an HTTP client stack
and a container-driving test harness both move faster than that crate wants to.
A companion pays for what it pulls in; the core stays where it is.

## License

MIT

[`dynamic-config`]: https://docs.rs/dynamic-config
[`dynamic-config-consul`]: https://docs.rs/dynamic-config-consul
[`dynamic-config-nats`]: https://docs.rs/dynamic-config-nats
