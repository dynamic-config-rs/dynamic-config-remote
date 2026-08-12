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

**Watching:** polling — Firestore's REST surface offers nothing better —
but by the document's `updateTime` field, not by re-reading the document;
a document with no `updateTime` refuses at `watch()` time rather than
polling forever. Stopping is noticed within a quarter second.

The [README](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-firestore)
carries the full story, tested against Google's own emulator; MSRV 1.85.
