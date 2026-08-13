# Writing a Store

A store is one trait implementation, and — if the protocol allows it — a
watch loop. Everything else is polish. Seven of these ship from this
repository, deliberately alike, and the likeness is the point: they are the
reference corpus, and the fastest way to a correct eighth is to copy the
closest one rather than starting from the shape in your head.

| If the client is… | Read | Trait |
|---|---|---|
| plain HTTP, blocking | `dynamic-config-consul` | `RemoteSource` |
| plain HTTP, with tokens | `dynamic-config-vault` | `RemoteSource` |
| plain HTTP, a cloud API | `dynamic-config-firestore` | `RemoteSource` |
| a connection-holding client | `dynamic-config-redis` | `RemoteSource` |
| async (gRPC, streaming) | `dynamic-config-etcd` | `AsyncRemoteSource` |
| an async SDK | `dynamic-config-s3` | `AsyncRemoteSource` |

Pick the trait the client already is. Wrapping an async client in `block_on`
to reach the blocking trait, or spawning a thread to reach the async one,
hides a runtime requirement the caller cannot see — and the caller's side
already copes with either: `refresh_remote_async()` accepts both kinds, so
your choice is not a constraint on them.

## Fetching

```rust
impl RemoteSource for MyStore {
    fn fetch(&self) -> Result<Fetched, Error> {
        let text = self.http_get("/config")?;

        Ok(Fetched::new(text, Format::Json))
    }

    fn describe(&self) -> String {
        format!("my-store {}", self.address)
    }
}
```

Three contracts hide in those ten lines:

- **Constructing a source is not I/O.** Building `MyStore` reaches nothing;
  the first `fetch` (or the first login it needs) does. Configuration that
  hits the network in a constructor is how a startup gets mysteriously
  slow, and it puts an error where no one can handle it.
- **One key holds a whole document** — unless your store is a map of named
  fields, the way Vault and Firestore are. Then `fetch` returns the
  section's *contents* wrapped under the section key, so the document
  merges like any other source. Say which model your store uses in the
  crate's first paragraph, and say why.
- **`describe()` lands in error messages.** Name the address and the key —
  `"consul http://consul:8500 myapp/db.json"` — because "where did this
  configuration fail to come from" is the first question a diagnostic gets
  asked. And never put a credential in it: a password that arrived in a
  URL must be redacted before the URL appears anywhere, *including* the
  URL-parse error path, which is the message most likely to be pasted into
  a ticket.

A failed `fetch` costs nothing downstream — the previously fetched document
stays in place, so an unreachable store does not take a working process
down with it. Your job is only to return an honest error.

## Several keys as one document

`fetch` returns one document, and a deployment that splits configuration
across a prefix — `myapp/db`, `myapp/server` — has several. Merging them is
the store's job, because only the store knows what "a prefix" means in its
protocol, and it has to happen before `fetch` returns: `Fetched` carries one
text and one format on purpose, and widening it would change a trait every
external store implements.

The merge is not the store's job to *write*, though. `Value` parses, merges
and re-emits any format this build can read, so a store crate needs no
parser of its own:

```rust
# use dynamic_config::{Error, Fetched, Format, Value};
fn merged(documents: &[String], format: Format) -> Result<Fetched, Error> {
    let mut document = Value::parse(&documents[0], format)?;

    for later in &documents[1..] {
        document.merge(Value::parse(later, format)?);
    }

    Ok(Fetched::new(document.render(format)?, format))
}
```

`merge` is later-wins, tables deep, arrays replaced whole — the rule this
crate already teaches for files, so a caller who lists keys is expressing an
order the same way a caller who lists files is.

A **prefix** read means something different: keys under a prefix are sections
nobody intended to overlap, so a collision there is a deployment bug rather
than a precedence question. `Value::overlapping_paths` names the leaves two
documents both supply — paths only, never values, so it is safe to put in the
error a person will read:

```rust
# use dynamic_config::{Error, Value};
# fn refuse(first: &Value, second: &Value) -> Result<(), Error> {
let clashes = first.overlapping_paths(second);

if !clashes.is_empty() {
    return Err(Error::remote(format!(
        "two keys under this prefix both supply {}",
        clashes.join(", ")
    )));
}
# Ok(())
# }
```

Two things this costs, and both belong in your crate's own documentation
rather than in an incident. **Provenance is store-grained:** a merged
document is one layer, so `explain` names the store and not which key inside
it supplied a value. **A partial read is a failure, not half a
configuration** — `?` on the parse gives you that, and the previously fetched
document keeps serving.

The shipped stores that read several keys — all eight of them — do not
each write that merge. It is one implementation in
`dynamic-config-store-core`'s `documents` module, which is where the four
decisions that are the same everywhere live: the two ordering rules above,
the collision report, a ceiling on how many keys one prefix folds (512 —
a prefix is caller input and the answer to it is server input), and the
literal check that a key the server returned is actually under the prefix
that was asked for. Read it before writing your own; it is not a stable API,
but it is the corpus's answer, and an eighth store copying the closest
shipped one gets it for free.

Two more things the corpus settled, both worth copying:

- **Find the keys the way the protocol offers, in one call if it has one.**
  etcd has a range read and a transaction of range reads; Consul has
  `?recurse`; Redis has `SCAN` — and `SCAN` rather than `KEYS`, because `KEYS`
  blocks the server for the length of the whole key space; S3 has
  `ListObjectsV2`; Firestore has `:batchGet`. Where the one call does not
  exist, say so: a list Consul, Vault, NATS or S3 reads one key at a time is
  not read atomically, and their documentation says that rather than implying
  otherwise.
- **Where the budget is checked is part of the design.** A prefix is caller
  input and the answer to it is server input, so the ceiling has to bite on
  the *listing*: S3 asks each page for one key more than the budget allows,
  which refuses a prefix over a whole bucket after one request rather than
  after a million bodies. A count taken after everything is fetched is a
  ceiling on nothing.
- **A form the protocol cannot carry honestly is better left out than
  approximated.** NATS has no prefix form because the only listing its client
  exposes walks the whole bucket; Vault and Firestore have none because a
  secret is a section's contents, so a subtree folded into one section
  collides on every field name two of them share. A `Keys` enum without a
  `Prefix` variant says that in the type, where a caller meets it, rather than
  in an error at run time.
- **A multi-key source refuses to be watched.** Not for want of a loop: a
  watch on a set has to say *the set changed* and then re-read the set **as of
  one instant**, and a store with no batch read cannot do the second. Waking
  on one key and re-reading key by key installs a document that never existed
  — and it does it precisely while a deployment is writing, which is the
  moment a watch exists to catch. Refusing at `watch()` and pointing at
  polling is honest; a loop that can serve half an update is not.

## The watch loop

The core deliberately does not own this loop: a watch is long-lived and
protocol-shaped in a way one trait cannot honestly cover. Your store owns
it, and the conventions below are what make seven different protocols feel
like one feature. The ladder for *how* to detect a change:

1. **Push, if the protocol can push** — an etcd watch stream, NATS KV
   change notifications, Redis keyspace events.
2. **A blocking query, if it has one** — Consul's index-carrying reads.
3. **Polling something cheap, if it has neither** — an ETag, a version
   number, an update time. Never the document itself: the point of a watch
   is not to move the configuration across the network once a second.

And the signatures, split the same way the traits are:

```rust
// Blocking: a thread runs it, so it takes a stop token and a callback.
pub fn watch<F>(&self, watching: &Watching, on_change: F) -> Result<(), Error>
where
    F: FnMut(Fetched) -> Result<(), Error>;

// Async: a future runs it; dropping the future is the cancellation.
pub async fn watch<F>(&self, on_change: F) -> Result<(), Error>
where
    F: FnMut(Fetched) -> Result<(), Error>;
```

A blocking loop checks `watching.keep_going()` between requests and sleeps
in slices (`watching.sleep_for`), so a stop is noticed in a bounded time
rather than after the whole retry pause. Publish what that bound is — the
poll interval, or the blocking query's wait — because "how long does
stopping take" is the one operational fact every deployment asks.

Three rules every shipped store follows, because they are decisions rather
than accidents:

- **The current value is not delivered at startup.** A watch reports
  *changes*; announcing the value the caller already has would make every
  restart look like an edit. Prime whatever marker you compare against —
  Consul's first blocking query, at index zero, answers immediately with
  the stored value, and the loop swallows exactly that one answer — and
  let the caller `fetch` first if the starting value matters. It usually
  does, which is why the wiring examples fetch-then-watch.
- **A deleted key is not a change.** No configuration is not a
  configuration; neither replaying the last document nor pushing emptiness
  is better than leaving the running snapshot alone.
- **A transport failure retries; a callback error ends the watch.** The
  store restarting is precisely what the loop exists to survive — pause
  briefly, try again, and let only the stop token end it. An error from
  the *callback* is different: it is the caller saying stop, and — since a
  replaced source's sink refuses by returning one — it is also how a stale
  loop winds itself down. Report it and return.

Two refinements the corpus learned the hard way: a store that bumps its
change marker on every write — including a write that changed nothing —
should compare the document text and stay quiet when it is identical; and
a watch that can *never* fire must refuse at `watch()` time with an error,
not poll forever — a key with no recognisable format, a Vault v1 mount, a
Firestore document with no `updateTime`.

## Credentials

Every store has its own way in, and every one of them expires. The rules
that hold across the corpus:

- **Logging in is lazy** — the first request that needs the credential
  performs it, not the constructor.
- **Expiry is handled on both sides.** A credential close to its expiry is
  renewed *before* the request; one that turns out dead is replaced
  *after* it, and the request retried — **once**. Clocks skew and tokens
  get revoked, so the proactive path cannot catch everything; and a second
  refusal means the policy is wrong, so retrying again would turn a clear
  failure into a hang.
- **A credential read from a file is re-read at every login.** Kubernetes
  rotates projected service-account tokens; a copy taken at startup
  expires with the pod still running.
- **Speak the store's vocabulary.** If the client models credentials on a
  type, re-export it (etcd and NATS take their own `ConnectOptions`); only
  invent an `Auth` enum when there is nothing to re-export.

And one rule about *deciding* anything from a failure: match the client's
typed status, never the error's text. The key or path appears in every
message, so a key named `403.json` makes every error read as a refused
token the moment anyone greps a string for a status code.

## What the tests must pin

The shipped stores test against real servers in containers — a mock of
etcd would only ever confirm what its author already believed about etcd.
The minimum set, for any store:

- the happy path: a stored value becomes a configuration document;
- the document loads into a struct through the normal merge;
- a missing key is an error that names the key;
- an unreachable server is an error, not a hang;
- a change reaches the watch callback;
- a deletion does **not** reach it.

Anything decided on a typed status — which token failures earn the one
retry, above all — needs a *scripted* server as well: a `TcpListener`
speaking just enough HTTP/1.1 to count requests. A container test can show
the retry works; it cannot see a wasted one.

Treat everything a real server sends as untrusted while you are at it: a
lease duration goes through `checked_add`, a body may not be JSON, a value
may not be UTF-8. The server being *yours* in the test does not make it
yours in production.

## Wiring it up

The application-side choreography — `set_remote`, the explicit
`refresh_remote()`, and the `remote_sink()` your watch loop's callback
pushes into, taken once at wiring time — is the
[parent chapter's](../remote-stores.md#watching-a-store); nothing about it
is store-specific, which is the point. If the store you wrote deserves a
place in this repository as a companion crate, the contributor guide in
[docs/CONTRIBUTOR-ONBOARDING.md](https://github.com/ctolon/dynamic-config/blob/main/docs/CONTRIBUTOR-ONBOARDING.md)
carries the workspace plumbing that this chapter deliberately leaves out.
