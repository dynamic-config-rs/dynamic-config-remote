# Remote Stores

Configuration served from somewhere other than this machine — etcd, Consul,
NATS, Vault — arrives as a document and merges like a file, above the files and
below the environment.

| Crate | Store | Trait | Reads | Watches by | Authenticates with |
|---|---|---|---|---|---|
| [`dynamic-config-etcd`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-etcd) | etcd v3 | async | one key, several keys, or a range — a whole document | a watch stream | user/password, TLS |
| [`dynamic-config-consul`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-consul) | Consul KV | blocking | one key, several keys, or a subtree — a whole document | a blocking query | ACL token, Kubernetes, JWT/OIDC |
| [`dynamic-config-nats`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-nats) | NATS JetStream KV | async | one key or several keys — a whole document | a KV change stream | token, user/password, NKey, JWT, creds |
| [`dynamic-config-redis`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-redis) | Redis | blocking | one key, several keys, or a prefix — a whole document | keyspace notifications | in the URL, TLS |
| [`dynamic-config-vault`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-vault) | Vault KV v2 | blocking | one path or several — a map of fields | polling the version | token, AppRole, Kubernetes, JWT/OIDC, userpass, LDAP, cert |
| [`dynamic-config-s3`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-s3) | S3, and anything speaking it | async | one object, several objects, or a prefix — a whole document | polling the ETag | the AWS credential chain |
| [`dynamic-config-firestore`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-firestore) | Firestore | blocking | one document or several — a map of fields | polling `updateTime` | workload identity, an access token |
| [`dynamic-config-git`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-git) | any git host | blocking | one file, several, or a directory — out of one commit | polling the ref advertisement | HTTPS token, SSH agent or key, anonymous |

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

## Several keys as one document

A deployment that splits its configuration across a prefix — `myapp/db`,
`myapp/server` — has several keys where `fetch` returns one document.
Installing one source per section works and is a little tedious, so every
store reads the set itself and hands the loader one document. `Fetched`
still carries one text and one format: the merge happens inside the store,
before `fetch` returns, because widening `Fetched` would change a trait every
external store implements for an ergonomic gain in seven of eight.

```rust
// Named keys: a list of layers, in call order — later wins.
Etcd::new(endpoints, Keys::several(["myapp/base.json", "myapp/local.json"])).await?;

// A prefix: disjoint sections, and an overlap between two of them is an error.
Consul::new(address, Keys::prefix("myapp/")).with_format(Format::Json);
```

**The two shapes obey different rules, on purpose.** A caller who *names* keys
is expressing an order, exactly the way `.file("base.toml").file("local.toml")`
does — so a named list merges in call order and the later key wins, tables
deeply and arrays replaced whole. A caller who names a *prefix* is expressing
"these are the sections of my configuration", and the order the server lists
them in is nobody's decision — so two keys under one prefix supplying the same
path is a deployment bug, and the fetch fails naming both keys and the paths
they collided on. Naming the keys instead is how you say which one wins.

**A collision report names paths, never values.** It is a diagnostic, and the
rule the whole crate holds to holds here.

**Provenance becomes store-grained.** The merged document is one layer, so
`source_of` answers `from etcd … keys myapp/base.json, myapp/local.json` rather
than naming which key inside the set supplied a given value. That is the price
of merging before the loader sees it, and it is why `describe()` names the whole
set: one layer cannot say more, and naming the set is as close as it gets. A
deployment that needs per-key provenance should install one source per key,
which is exactly what it did before.

**A partial read is a failure, not half a configuration.** One unreadable key
out of five fails the whole fetch, naming the key. A refresh that fails leaves
the last known good document serving and says so; a configuration quietly
missing a section says nothing at all, and is discovered later by something
else.

**A prefix is caller input, and the answer to it is server input.** An empty
prefix, or one pointed at a whole tenant's key space, matches everything there
is — so a prefix matching more than 512 keys is refused rather than pulled into
memory, and a key the server answers with that is not under the prefix asked for
is refused rather than merged.

**Three multi-key sources can be watched, and the rest refuse at `watch()`** —
before the first event rather than after a bad one, pointing at polling
`refresh_remote()` on a timer, which is the one round trip the fetch always was.
[Which three, and why](#watching-a-multi-key-source), is worth its own section:
where a store refuses it is not a missing loop but a missing guarantee.

What each store can do is what its protocol offers, and the differences are
worth knowing before choosing where to put the keys:

| Crate | A named list | A prefix |
|---|---|---|
| etcd | one transaction of range reads — one round trip, one revision; capped at etcd's `--max-txn-ops` (128) | one range read — one round trip, one revision |
| Consul | one request *per key*, so **not** atomic | one `?recurse` request, at one index |
| Redis | one `MGET` — one command, one operation | `SCAN` then `MGET`, **never `KEYS`**; the scan is not atomic |
| NATS | one get *per key*, so **not** atomic | no: the only listing the client exposes walks the whole bucket |
| Vault | one read *per path*, so **not** atomic — and one audit-log line each | no: `LIST` exists, the mapping is what does not |
| S3 | one `GetObject` *per key*, so **not** atomic | one `ListObjectsV2` (paginated) then one `GetObject` per key |
| Firestore | one `:batchGet` — one round trip, and still not one snapshot | no: `documents.list` exists, the mapping is what does not |
| Git | one commit's tree, so atomic for free | a **directory** rather than a string prefix, out of that same tree |

Where a store's list read is not atomic, a write landing between two requests
can produce a document that never existed as a set. That is a real difference
between the stores rather than a bug in one of them, and it is the reason etcd
reads a named list as a transaction and Redis reads one as `MGET`. Firestore is
the instructive middle: `:batchGet` is genuinely one request, and Google's own
documentation says each document is read at its own time unless a transaction
is opened — so one round trip bought fewer failures, not atomicity, and the
crate says so rather than letting the request count imply it.

### Two stores hold fields, not documents

Vault and Firestore store a *section's contents* — a map of named fields —
rather than a whole configuration file, which is why both wrap what they read
under a section key. So "several keys as one document" means something
different there, and it is a mapping decision rather than a protocol one:

**Every path lands under the same section key, and the paths layer.** A list is
a shared secret and an override, which for Vault is the natural shape because
the unit its policies apply to *is* the path — splitting a section into a
public half and a restricted half is something only Vault can do, and this is
how you read it back as one section.

**Neither offers a prefix or collection form**, and both APIs have one to
offer. Folding a subtree into a single section would make `myapp/db` and
`myapp/server` — the ordinary layout — collide on `host`, so the prefix rule
would refuse nearly every real deployment. Naming a sub-section after each
secret's path would fix that by inventing a convention no other store here has,
and would make a list of one path mean something different from one path. A
deployment that wants several sections installs one source per section, which
is what it did before.

### Watching a multi-key source

Whether a store can do this is decided per store, not by one rule applied
uniformly for tidiness. A watch that fires on a set has to answer two questions,
and both answers have to be yes:

1. **Does the store say when anything in the set changed?**
2. **Can the set then be re-read as of one instant?**

The second is what usually fails. Re-reading a set with one request per key
means a change to `myapp/db` wakes the loop, and the re-read then collects the
new `myapp/db` and whatever `myapp/server` happens to be halfway through a
deployment — a document that never existed at any instant, installed by the
loader, and served until the next change. A `refresh_remote()` on a timer can
tear the same way, but a caller chooses when it runs and the next tick corrects
it; a watch fires *during* the write, so it turns a rare accident into the
normal case. That is the whole argument, and it is why a store that cannot
answer both questions refuses at `watch()` — before the first event, not after a
bad one.

| Crate | Says the set changed | Re-reads the set at one instant | So |
|---|---|---|---|
| Consul, prefix | yes — one recursive blocking query, one index over the subtree | yes — that same answer *is* the subtree at that index, so there is no re-read at all | **watched** |
| etcd, prefix | yes — one prefix watch stream | yes — one range read at the event's own revision | **watched** |
| Redis, named list | yes — a keyspace notification per key | yes — one `MGET`, and Redis runs one command as one operation | **watched** |
| Git, several or directory | yes — the ref advertisement says the repository moved, which is a superset of the set | yes — one commit, one tree, every path read out of it | **watched** |
| Redis, prefix | yes | **no** — the re-read has to *find* the keys with `SCAN`, a cursor over many commands | refused |
| NATS, named list | yes — `watch_many` filters the stream on the set | **no** — no batch read; the re-read is one get per key | refused |
| Vault, S3 | no — neither pushes; the loop polls a version or an ETag, and those belong to one secret or one object | **no** | refused |
| Firestore, named list | no — the push API is a gRPC stream this crate deliberately does not carry | **no** — `:batchGet` is one request but not one snapshot | refused |

Consul's is the cheapest of the four and the only one that re-reads nothing: its
answer already *is* the document. etcd and Redis each re-read, and each pins the
re-read to something that makes it one instant — a revision for etcd, and for
Redis the fact that `MGET` is a single command.

### Spurious, never torn

None of the four promises one delivery per write, and the distinction is the one
worth holding on to:

- **Never torn.** Every document delivered is a state the store really held. That
  is the property the refusals above exist to protect, and each of the four has a
  test that stamps one generation into every key of the set, changes them all
  repeatedly, and asserts that every delivery agrees with itself.
- **Spurious, and possibly coalesced.** Except for Consul, the read follows the
  event rather than being simultaneous with it, so a delivery can carry a state
  *newer* than the write that woke it, and two rapid writes can arrive as one
  delivery of the later state. Git adds its own version: a commit touching
  nothing the source reads still moves the ref. A delivery is never *older* than
  the one before it, which is what makes this a cost rather than a hazard.

A **torn deployment** is the other half, and it is not this crate's to fix: an
operator who writes `myapp/db` and then `myapp/server` in two separate
operations really did put the store into the intermediate state, and a watch that
reports it is reporting the truth. Write the set the way the store lets you —
one etcd transaction, one Consul `/v1/txn`, one Redis `MSET`, one commit — and
there is no intermediate state to report.

## Watching a store

Polling on a timer works, and is what Vault, S3, Firestore and Git have to do — but
etcd, NATS, Consul and Redis can say the moment a value moves. Each companion
crate owns that loop, because a
watch is long-lived and protocol-shaped in a way one trait cannot honestly
cover; what they all push through is a sink taken at wiring time:

```rust
// etcd, NATS and S3: a future. Cancelled by dropping it, on any executor.
let sink = DbConfig::remote_sink();
tokio::spawn(async move { etcd.watch(move |doc| sink.apply(doc)).await });

// Consul, Vault, Redis and Firestore: a thread, so it takes a stop token.
let watch = RemoteWatch::new();
let watching = watch.watching();

let sink = DbConfig::remote_sink();
std::thread::spawn(move || consul.watch(&watching, move |doc| sink.apply(doc)));
```

`remote_sink()` is taken **once, where the loop starts** — it remembers which
source was installed, and a sink whose source has since been replaced refuses
to deliver, which also ends the stale loop: its refusal is a callback error.
`apply` is the *same reload path a file edit takes* —
validation, the reload hooks, the diff, the cache. A document that does not fit
leaves the previous snapshot serving and returns the error, exactly as a bad
file edit does.

Three things behave the same way across all eight, because they are decisions
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
| Vault, Redis, S3, Firestore, Git | a quarter second, whatever the poll interval is |

### A failing watch says so

A watch is the half of a store this library cannot see. A *fetch* records
itself — `RemoteStatus` counts it, times it and remembers its failure — but a
watch loop that is failing delivers nothing, and a store that reports nothing
looks exactly like a store with nothing to report. `dynamic_config_remote_up`
would go on describing the last delivery while the loop behind it had been
erroring for an hour.

`reporting_to(sink)` closes that, and every network store takes it with the
same signature:

```rust
Consul::new(address, "myapp/db.json")
    .reporting_to(DbConfig::remote_sink())
    .watch(&watching, on_change)?;
```

What it records is narrow on purpose: the failure streak and the last
failure's *kind*, never a store's address. `last_fetch` deliberately keeps
ageing, because the pair is the alert — *up went to zero* **and** *the
document being served is an hour old*. A failure that reset the clock would
hide the second half.

Two things deliberately do **not** report. A refusal at the door — no format,
a shape the store cannot watch, a subscription that will not open — is
returned to the caller standing there, before there is a loop to be silent
in; charging it to `remote_up` would page somebody about Redis for a typo.
And a *callback's* own error is not the store's failure: the store answered,
and what the document then does is
[`ConfigStatus`](reload-lifecycle.md#operating-a-configuration)'s half of the
picture.

git needs none of it. Its watch is a poll, a poll is a fetch, and a fetch
already records itself.

## Credentials, and keeping them working

Every store has its own way in, and every one of them expires. Three rules hold
across all eight crates:

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

Those three rules are one implementation, in `dynamic-config-store-core` — the
crate that turns up in the dependency tree under every store crate. It exists
because the alternative was three copies of *when to refresh a token* drifting
apart, and it holds only what was genuinely identical: the margin, the expiry
arithmetic, and the lock held while a credential is obtained so that eight
threads finding an empty cache produce one login rather than eight. What stayed
in each store crate is what differs — whether a lease can be renewed (Vault
alone), which status means *this token is dead* (`403` for Consul and Vault,
`401` for Firestore), and what a store with no credentials at all looks like.
The crate is published because cargo requires it and carries no stable API;
nothing in it is meant to be named directly.

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

## TLS, and the one vocabulary all eight speak

A deployment behind a **private certificate authority** — an internal CA, a
TLS-inspecting proxy, a MinIO with its own certificate — needs to trust one
more certificate than the platform does. A hardened one needs to *present*
one as well. Every store here takes both, spelled the same way, through one
data-only type:

```rust
use dynamic_config_vault::{TlsConfig, Vault};

let vault = Vault::new("https://vault.internal:8200", "secret", "myapp/db")
    .with_tls(
        TlsConfig::new()
            .with_ca_certificate_file("/etc/ssl/private-ca.pem")
            .with_client_certificate_files("/etc/ssl/app.crt", "/etc/ssl/app.key"),
    );
```

`TlsConfig` lives in `dynamic-config-store-core` and every store re-exports
it. There are four settings, in two spellings each — a file path or PEM
bytes — and **nothing else**: no client type appears in any signature.

That is the whole design decision, and it is worth saying why, because
[sharing a client you already have](#sharing-a-client-you-already-have) was
the answer before and still is. Handing a store *its client's own* type —
etcd's `ConnectOptions`, a `ureq::Agent` — means options this project has
never heard of keep working, which is a real property and one nothing here
took away. It also had two costs. Four of the eight stores had no such door
at all, so an enterprise behind a private CA could not use them. And none
of it could ever cross into the [Python wheels](python/remote-stores.md),
because there is no Python spelling for a `tonic` TLS configuration or a
`ureq` agent — which is exactly why the new surface is **data**, and why a
later binding has something to bind to.

### What each store can express

The clients differ, and where one cannot express a setting **the store
refuses the whole configuration** naming the call and what to use instead.
A silently ignored `ca_certificate` is a program that believes it is pinned
and is not, which is worse than a program that will not start.

| Crate | CA from a file | CA from bytes | Client certificate | Behind a feature |
|---|---|---|---|---|
| etcd | yes | yes | yes | `tls` — a gRPC TLS stack, which a private-network cluster has no use for |
| Consul | yes | yes | yes | no — `ureq` already carries rustls |
| Vault | yes | yes | yes | no |
| Firestore | yes | yes | yes | no |
| Redis | yes | yes | yes | `tls` — `rediss://` needs a stack the client does not carry by default |
| NATS | yes | **no** | file paths only | no |
| S3 | yes | yes | **no** | no |
| Git | — | — | — | its own transport; not part of this |

**NATS takes paths, not bytes.** `async-nats` opens the files itself, and
the only byte-taking door is a hand-built `rustls::ClientConfig` — a direct
`rustls` dependency and a crypto-provider decision, for one spelling. So the
byte forms are refused, pointing at the file forms. The obvious workaround
is deliberately not taken: writing the material to a temporary file would put
a private key on a disk that never asked for one.

**S3 has no client certificate.** The AWS SDK reaches TLS through
`aws-smithy-http-client`, whose TLS context is a trust store and nothing
else — there is no slot to fill. mTLS to an S3-compatible server means
building the connector yourself and handing over the finished client, which
is what `from_client` is for.

Two smaller things hold everywhere. **A named CA replaces the platform trust
store** rather than joining it: naming a private authority is saying the
public ones do not apply to this host, and a deployment that needs both puts
both in the one file. And **nothing is read at build time** — the files are
opened when the client is built, so a missing certificate is an error naming
the path rather than a panic in a builder chain, and a rotated CA is picked
up by rebuilding the source.

### With the escape hatch

Both doors reach the same slot, so setting both is a question with no honest
answer. Where a store can tell, it **refuses**:

| Crate | Both set | What happens |
|---|---|---|
| Consul, Vault, Firestore | `with_agent` and `with_tls` | refused at the first request, naming both calls |
| Redis, S3 | `from_client`/`with_config` and `with_tls` | different constructors; both cannot be called |
| NATS | `ConnectOptions` roots and `with_tls` | both sets are added, which is what `async-nats` does with them |
| etcd | `ConnectOptions::with_tls` and `with_tls` | the `TlsConfig` wins — `etcd-client` exposes no way to ask whether that slot is already filled, so this one is documented rather than refused |

An agent already carries a complete TLS configuration, so "apply this too"
could only mean discarding one of them — and the one that would be discarded
is a certificate authority the caller believes is pinned.

### There is no way to turn verification off

Not an omission. Three arguments, and the first is the one that settles it:

**It could not be uniform.** `tonic` offers no such switch, and neither does
the AWS SDK's TLS context; `async-nats` reaches it only through a hand-built
`rustls::ClientConfig`, and git's transport refuses one on principle. A word
in this vocabulary that half the stores had to refuse would be a word that
mostly means "error".

**It answers nothing `with_ca_certificate_file` does not.** The two
situations people reach for it in — a development server with a self-signed
certificate, an enterprise private CA — are both a matter of trusting one
more certificate. That is one line, and it leaves the server authenticated.
Turning verification off does not weaken TLS in the way a checklist means;
it removes it, and leaves a connection anyone on the path can read and
rewrite.

**The escape hatch is still there for the case nobody anticipated.** Every
client underneath has its own dangerous switch under its own frightening
name — `ureq`'s `disable_verification`, Redis' `#insecure` URL fragment. A
caller who genuinely needs one names that API in their own code, where a
reviewer sees it, rather than reaching for a short word on a type whose
other options are safe.

### The private key

It is the sharpest secret this crate family handles, and three rules follow
from that. `TlsConfig`'s `Debug` prints shape only — a path where there is
one, `<redacted>` where the key is bytes — and never material, which
`dynamic-config-store-core` has a planted-key test for. A file read that
fails names the path and the operating system's reason, never the contents.
And **no PEM parse error is ever wrapped**: `rustls-pki-types` renders the
line it choked on, and the line it choked on in a private key file is
private key material. Every store reports "this is not PEM-encoded material
of the kind expected" in its own words and drops the parser's.

One consequence worth knowing: S3 parses the CA certificate itself, purely
in order to refuse. The SDK's rustls connector calls `.expect("cert
parsable")` on the material, so a certificate it cannot read would otherwise
be a panic at the first connection — a long way from the call that supplied
it.

### Worked examples

Two, and both run:
[`vault_private_ca`](https://github.com/ctolon/dynamic-config/blob/main/dynamic-config-vault/examples/vault_private_ca.rs)
reads a secret from a Vault behind an authority the machine has never heard
of, and
[`etcd_client_certificate`](https://github.com/ctolon/dynamic-config/blob/main/dynamic-config-etcd/examples/etcd_client_certificate.rs)
presents a client certificate to an etcd started with `--client-cert-auth`.

## Writing your own

The full how-to — the watch loop and its conventions, credential refresh,
what the tests must pin — is [its own chapter](remote-stores/writing-a-store.md).
The short version: implement one trait, return the document and its format:

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

## Timeouts

`with_timeout(..)` means one thing everywhere: **the deadline for a
single fetch attempt, excluding retries the underlying client performs.**
What each store implements it with differs, because their clients do:

| Crate | What the deadline covers |
|---|---|
| etcd | the request, wrapped — *not* `ConnectOptions::with_timeout`, which only bounds connecting and says nothing about a member that accepts and then goes quiet |
| NATS | the KV get, wrapped. `ConnectOptions::request_timeout` is its connect-side twin and is set separately |
| Redis | connect, write and read, all three from the one value |
| Consul, Vault | the HTTP request, including the blocking query's own wait where there is one |
| Firestore | the HTTP request, and the metadata-server token fetch behind it |
| S3 | one *attempt*. The SDK retries underneath, so a 5 s timeout with three attempts is a 15 s call — the crate's README does that arithmetic |
| Git | negotiating and transferring, which is where a git fetch spends its time. *Not* establishing the connection: `gix`'s HTTP transport bounds that at twenty seconds of its own and does not expose the knob |

A store that answers slowly but inside the deadline still succeeds; one
that never answers fails with an error that says it timed out, and the
previous snapshot keeps serving.
