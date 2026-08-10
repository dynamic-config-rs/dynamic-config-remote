# dynamic-config-firestore

Read [`dynamic-config`] configuration from a Google Cloud Firestore document.

```toml
[dependencies]
dynamic-config = "0.0.1"
dynamic-config-firestore = "0.0.1"
```

```rust
use dynamic_config_firestore::{Auth, Firestore};

DbConfig::set_remote(
    Firestore::new("my-project", "config/db").with_auth(Auth::metadata_server()),
);

DbConfig::refresh_remote()?;
```

Firestore's REST API is plain HTTP, so this implements the **blocking**
`RemoteSource` trait: nothing here needs an async runtime, and neither does
using it.

## What it reads

One document, at a path like `config/db` — collection, then document. Its fields
become the configuration, wrapped under the section key, which is the same shape
[`dynamic-config-vault`] uses and for the same reason: Firestore stores a map of
named fields, so the natural unit is the field.

### Firestore does not store JSON

It stores a tagged encoding, where every value names its own type and integers
arrive as strings — because JSON's number is a double and Firestore's is not:

```json
{"port": {"integerValue": "5432"}}
```

Handing that to serde would mean every configuration struct growing a
`#[serde(with = ..)]` per field, so it is decoded here, once, into the shape a
configuration file would have had. `timestampValue`, `bytesValue` and
`referenceValue` keep their string form, because a configuration file has no
better answer for one either.

## Authenticating

| Method | Constructor | For |
|---|---|---|
| Workload identity | `Auth::metadata_server()` | GKE, Cloud Run, GCE — no secret to distribute |
| An access token | `Auth::access_token(..)` | anything that already has one, including `gcloud auth print-access-token` |
| None | `Auth::Emulator` | the Firestore emulator |

A token from the metadata server is cached and replaced as it approaches expiry;
a `401` replaces one early, once. A supplied token cannot be renewed — there is
nothing here to renew it from — so a long-running process should use workload
identity.

**A service-account JSON key is deliberately not supported**, and that is a
recommendation rather than a gap: signing one means an RS256 stack in a
configuration library, and Google's own guidance is that a downloaded key is the
option of last resort. Workload identity covers GKE, Cloud Run, GCE and Cloud
Functions; for anything else, mint a token outside the process and pass it in.

## Watching

Firestore *can* push — the real-time API is a gRPC stream — and this deliberately
does not use it: that would put a gRPC stack in a crate whose whole point is a
plain HTTP read. Polling reads one small document and compares `updateTime`,
which for a configuration document checked every thirty seconds is a rounding
error against a project's quota.

```rust
firestore.watch(&watching, Duration::from_secs(30), DbConfig::apply_remote)
```

- The current document is **not** delivered at startup.
- A failed check does not end the watch. Stopping is noticed within a quarter
  second whatever the interval is.

## Builders

| Method | Default |
|---|---|
| `with_key(..)` | `"db"` — must match the `key` in `#[dynamic_config]` |
| `with_database(..)` | `(default)` |
| `with_auth(..)` | `Auth::Emulator`, which sends nothing |
| `with_endpoint(..)` | `https://firestore.googleapis.com` |
| `with_timeout(..)` | 10 seconds |
| `with_agent(..)` | one built per source |

## Testing

The test suite drives **Google's own Firestore emulator in a container**, pulled
from `gcr.io` rather than Docker Hub so a test run does not hit anonymous pull
limits. What is being checked is Firestore's value encoding and its REST
surface, and a mock of those would only confirm what we already believed about
them.

```sh
cargo test -p dynamic-config-firestore    # needs a working Docker daemon
```

## MSRV

1.85 — higher than [`dynamic-config`]'s own 1.71, because an HTTP client stack
and a container-driving test harness both move faster than that crate wants to.

## License

MIT

[`dynamic-config`]: https://docs.rs/dynamic-config
[`dynamic-config-vault`]: https://docs.rs/dynamic-config-vault
