# Remote Stores

Configuration served from somewhere other than this machine — etcd, Consul,
NATS, Vault — arrives as a document and merges like a file, above the files and
below the environment.

| Crate | Store | Trait | Reads | Watches by | Authenticates with |
|---|---|---|---|---|---|
| [`dynamic-config-etcd`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-etcd) | etcd v3 | async | one key, a whole document | a watch stream | user/password, TLS |
| [`dynamic-config-consul`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-consul) | Consul KV | blocking | one key, a whole document | a blocking query | ACL token, Kubernetes, JWT/OIDC |
| [`dynamic-config-nats`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-nats) | NATS JetStream KV | async | one key, a whole document | a KV change stream | token, user/password, NKey, JWT, creds |
| [`dynamic-config-redis`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-redis) | Redis | blocking | one key, a whole document | keyspace notifications | in the URL, TLS |
| [`dynamic-config-vault`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-vault) | Vault KV v2 | blocking | one path, a map of fields | polling the version | token, AppRole, Kubernetes, JWT/OIDC, userpass, LDAP, cert |
| [`dynamic-config-s3`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-s3) | S3, and anything speaking it | async | one object, a whole document | polling the ETag | the AWS credential chain |
| [`dynamic-config-firestore`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-firestore) | Firestore | blocking | one document, a map of fields | polling `updateTime` | workload identity, an access token |

Each has its own README with the whole story, and an example that runs against a
real server in a container.

Each is a separate crate so that reaching for one store does not put the
others' dependency trees — a gRPC stack, a streaming client, the AWS SDK,
several HTTP clients — into a build that never asked for them.

```rust
DbConfig::set_remote(Consul::new("http://consul:8500", "myapp/db.json")?);
DbConfig::refresh_remote()?;   // the network round trip, explicitly

DbConfig::builder("db")
    .file("config.toml")
    .init()?;                  // merges what came back; touches no network
```

## Fetching is explicit

A remote source is **not** read on every `load()`. Configuration is read on
nearly every request, so a network round trip there would be indefensible — and
it is also what would force every async question to become a blocking one.

```text
refresh_remote()   →  fetch, keep the document
load()             →  merge the kept document, no I/O
```

That one decision is what lets a blocking source and an async source sit side by
side with no `block_on` anywhere, on any runtime or none. Pair it with whatever
already schedules work in your program — a timer, a signal handler, a watch
stream.

## Two traits, because two kinds of client exist

```rust
pub trait RemoteSource: Send + Sync + 'static {
    fn fetch(&self) -> Result<Fetched, Error>;
    fn describe(&self) -> String;
}

#[cfg(feature = "async")]
pub trait AsyncRemoteSource: Send + Sync + 'static {
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<Fetched, Error>> + Send + '_>>;
    fn describe(&self) -> String;
}
```

Consul and Vault have plain HTTP APIs, so implementing the blocking trait costs
their users no runtime. etcd speaks gRPC and NATS is a streaming protocol, so
both of those clients are async to begin with and pretending otherwise would
just hide a `block_on`.

`refresh_remote_async()` accepts **either**, running a blocking source inline —
so swapping one implementation for the other is not a breaking change for the
caller. `refresh_remote()` refuses an async source and says which call to use
instead, rather than reaching for a runtime it was never given.

## Watching a store

Polling on a timer works, and is what Vault, S3 and Firestore have to do — but
etcd, NATS, Consul and Redis can say the moment a value moves. Each companion
crate owns that loop, because a
watch is long-lived and protocol-shaped in a way one trait cannot honestly
cover; what they all push through is `apply_remote`:

```rust
// etcd, NATS and S3: a future. Cancelled by dropping it, on any executor.
tokio::spawn(async move { etcd.watch(DbConfig::apply_remote).await });

// Consul, Vault, Redis and Firestore: a thread, so it takes a stop token.
let watch = RemoteWatch::new();
let watching = watch.watching();

std::thread::spawn(move || consul.watch(&watching, DbConfig::apply_remote));
```

`apply_remote` is the sink, and it is the *same reload path a file edit takes* —
validation, the reload hooks, the diff, the cache. A document that does not fit
leaves the previous snapshot serving and returns the error, exactly as a bad
file edit does.

Three things behave the same way across all seven, because they are decisions
rather than accidents:

- **The current value is not delivered at startup.** A watch reports changes;
  announcing the value the caller already has would make every restart look like
  an edit. Fetch first if the starting value matters — it usually does.
- **A deleted key is not a change.** No configuration is not a configuration, and
  neither replaying the last one nor pushing emptiness is better than leaving the
  running snapshot alone.
- **A transport failure retries rather than ending the watch** — the store
  restarting is precisely what a watch is there to survive — with two named
  exceptions that end it with an error so a supervisor can restart it: an etcd
  stream error no token refresh can cure (a refresh that works resumes from
  the last delivered revision), and a Redis subscription that died.
  An error from *your* callback always ends it, so a caller that wants to
  survive a bad document should log it and return `Ok`.

Cancellation splits along the same line the traits do. An async watch is a
future: drop it. A blocking watch is a thread, which cannot be dropped from
outside, so it takes a [`Watching`] token and checks it between requests —
dropping the matching `RemoteWatch` stops it, the same contract `WatchHandle`
has for files.

How long stopping takes is the one thing worth knowing per store:

| Crate | Worst case for noticing a stop |
|---|---|
| etcd, NATS | immediate — the future is cancelled |
| Consul | the blocking query's `wait`, one minute by default |
| Vault, Redis, S3, Firestore | a quarter second, whatever the poll interval is |

## Credentials, and keeping them working

Every store has its own way in, and every one of them expires. Three rules hold
across all seven crates:

**Logging in is lazy.** Building a source reaches nothing; the first read does
it. Constructing a source is not I/O, and configuration that hits the network on
a call nobody expected to block is how a startup ends up mysteriously slow.

**Expiry is handled on both sides.** A credential close to its expiry is renewed
or replaced *before* the request; one that turns out to be dead is replaced
*after* it, and the request retried — once. Clocks skew and tokens get revoked,
so the proactive path cannot catch everything; and a second refusal means the
policy is wrong, so retrying again would turn a clear failure into a hang.

**A credential read from a file is re-read at every login.** Kubernetes rotates
projected service-account tokens, and a copy taken at startup expires with the
pod still running.

Each crate speaks its store's own vocabulary rather than inventing one: etcd and
NATS take their own `ConnectOptions` (re-exported, so no direct dependency),
while Vault and Consul get an `Auth` enum because their login endpoints have no
equivalent type.

## Sharing a client you already have

```rust
Etcd::from_client(client, "myapp/db.json")          // etcd
Nats::from_client(client, "config", "db.json")      // NATS
Consul::new(address, key).with_agent(agent)         // Consul
Vault::new(address, mount, path).with_agent(agent)  // Vault
```

For a program that already talks to the store, or one with its own proxy
settings, private CA, client certificate or connection pool. A shared client is
not a second-class one: it recovers from an expired credential like any other,
because the credentials live in the client rather than in the source.

## Writing your own

Implement one trait, return the document and its format:

```rust
impl RemoteSource for MyStore {
    fn fetch(&self) -> Result<Fetched, Error> {
        let text = self.http_get("/config")?;

        Ok(Fetched::new(text, Format::Json))
    }

    fn describe(&self) -> String {
        format!("my-store {}", self.address)   // this lands in error messages
    }
}
```

A failed fetch leaves the previously fetched document in place, so an
unreachable store does not take a working process down with it.

[`Watching`]: https://docs.rs/dynamic-config/latest/dynamic_config/struct.Watching.html
