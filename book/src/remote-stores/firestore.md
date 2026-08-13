# Firestore

[`dynamic-config-firestore`](https://docs.rs/dynamic-config-firestore)
reads configuration from a Google Cloud Firestore document, over its REST
API: the **blocking** `RemoteSource`.

```toml
[dependencies]
dynamic-config = "<version>"
dynamic-config-firestore = "<version>"
```

```rust
use dynamic_config_firestore::{Auth, Firestore};

DbConfig::set_remote(
    Firestore::new("my-project", "config/db").with_auth(Auth::metadata_server()),
);
DbConfig::refresh_remote()?;
```

**What it reads:** one document at `collection/document`; its fields
become the configuration, wrapped under the section key — the same shape
as [Vault](vault.md), for the same reason: a map of named fields.

**Several documents, one section:** `Keys::several(["config/db",
"overrides/db"])` reads both and merges them under that same section key, in
call order — later wins. That is **one `:batchGet` request**, which is
Firestore's own answer to a set rather than a loop wearing a batch's name,
and two things follow from what the API promises. The service returns the
documents in whatever order it likes and says so, so they are put back into
call order here — the order a caller wrote is the precedence, the order a
service replies in is not. And one request is not one snapshot: without a
transaction each document is read at its own time, and none is opened,
because an open read-only transaction is state on the service a
configuration read would have to remember to release.

There is deliberately **no collection form**, and `documents.list` is not
what is missing. [The reason in
full](../remote-stores.md#two-stores-hold-fields-not-documents).

**Firestore does not store JSON.** It stores a tagged encoding where
every value names its type and integers arrive as strings
(`{"port": {"integerValue": "5432"}}`) — because JSON's number is a
double and Firestore's is not. The crate decodes that once, into the
shape a configuration file would have had, so no struct grows a
`#[serde(with = ..)]` per field. `timestampValue`, `bytesValue` and
`referenceValue` keep their string form — a config file has no better
answer for one either.

**Authenticating:** `Auth::metadata_server()` (workload identity on GKE,
Cloud Run, GCE — no secret to distribute), `Auth::access_token(..)`
(anything that already has one, `gcloud auth print-access-token`
included), or `Auth::Emulator`, which sends nothing. A metadata-server
token is cached and replaced as it approaches expiry; a `401` replaces
one early, once.

**TLS as data:** `.with_tls(TlsConfig::new().with_ca_certificate_file(..))`
takes a private certificate authority and a client certificate as paths or
PEM bytes, with no `ureq` type in the calling code. Rarely wanted against
Google's own endpoint, whose certificates chain to an authority the
platform already trusts — it is for the deployments that do not go there
directly: an enterprise TLS-inspecting proxy, or an emulator behind
`with_endpoint`. Setting `with_agent` as well is refused at the first
request rather than resolved. [TLS, and the one vocabulary all eight speak](../remote-stores.md#tls-and-the-one-vocabulary-all-eight-speak)

**Watching:** polling — Firestore's REST surface offers nothing better —
but by the document's `updateTime` field, not by re-reading the document;
a document with no `updateTime` refuses at `watch()` time rather than
polling forever. A **multi-document source refuses** there too: that
`updateTime` belongs to one document, and a set of them has none. Stopping
is noticed within a quarter second.

**A failing watch says so:** `.reporting_to(sink)` hands the loop the same
`RemoteSink` its callback applies documents through, and every poll that comes
back with nothing is recorded on it — a read that failed, a document that came
back without the `updateTime` this watch is built on. A poll is where the two
cases are indistinguishable from outside: a document nobody has edited today
and a project that stopped answering an hour ago deliver the same nothing, and
without this `dynamic_config_remote_up` reports the last *delivery* rather
than the last *attempt*. A failed attempt moves the failure streak and nothing
else, so `dynamic_config_remote_last_fetch_seconds` keeps ageing while
`remote_up` goes to zero — the pair that says both *the store is not
answering* and *how stale what it last said has become*. Reporting is
infallible and silent: a loop is never handed a failure to report a failure. A
`fetch()` needs none of it, because a fetch already records itself.
[The remote store's own numbers](../telemetry.md#the-remote-stores-own-numbers)

The [README](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-firestore)
carries the full story, tested against Google's own emulator; MSRV 1.85.
