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
