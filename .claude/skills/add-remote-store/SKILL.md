---
name: add-remote-store
description: Use when adding a new remote configuration store as a companion crate (Redis, S3, etcd and friends) — covers the crate layout, the trait to pick, watching, authentication, and the container tests that make it trustworthy.
---

# Adding a remote store

Seven of these exist. They are deliberately alike, and the likeness is the
point: somebody who has read one can read the next. Copy the closest one rather
than starting from the shape in your head.

| If the client is… | Copy | Trait |
|---|---|---|
| plain HTTP, blocking | `dynamic-config-consul` | `RemoteSource` |
| plain HTTP, with tokens | `dynamic-config-vault` | `RemoteSource` |
| plain HTTP, a cloud API | `dynamic-config-firestore` | `RemoteSource` |
| a connection-holding client | `dynamic-config-redis` | `RemoteSource` |
| async (gRPC, streaming) | `dynamic-config-etcd` | `AsyncRemoteSource` |
| an async SDK | `dynamic-config-s3` | `AsyncRemoteSource` |

## The decisions already made — do not re-litigate them

**Fetching is explicit.** A store is read by `refresh_remote()`, never by
`load()`. Configuration is read on nearly every request; a network round trip
there would be indefensible. This is also what lets a blocking and an async
store coexist with no `block_on` anywhere.

**Pick the trait the client already is.** Do not wrap an async client in
`block_on` to reach the blocking trait, and do not spawn a thread to reach the
async one. Both hide a runtime requirement the caller cannot see.

**One key holds a whole document**, unless the store is a map of named fields
(Vault, Firestore) — then it holds the section's *contents*, wrapped under the
section key. Say which in the crate's first paragraph, and say why.

**The store's own vocabulary.** If the client already models credentials on a
type (`ConnectOptions`), re-export it. Only invent an `Auth` enum when there is
nothing to re-export.

## The checklist

1. **`Cargo.toml`** — `rust-version` of its own with a comment (a companion pays
   for what it pulls in), `readme = "README.md"`, `documentation`,
   `exclude.workspace = true`, and the crate in the workspace `members`.
2. **`src/lib.rs`** — `#![forbid(unsafe_code)]`, `#![deny(missing_docs)]`, the
   trait impl, `describe()` naming the address and the key.
3. **`from_client` / `with_agent`** — a caller who already talks to the store
   should not open a second connection.
4. **Watch**, the way the protocol allows. Push if it can push; a blocking query
   if it has one; polling *something cheap* if it has neither — an ETag, a
   version, an update time, never the document itself.
5. **Container tests.** Real server, no mocks. Include: the happy path, the
   document loading into a struct, a missing key, an unreachable server, a
   change reaching the callback, and a deletion *not* reaching it.
6. **Scripted-server tests** for anything decided on a typed HTTP status —
   which token failures earn a retry, above all. A `TcpListener` speaking
   just enough HTTP/1.1 (copy `tests/mock_agent.rs` in the consul crate) runs
   without Docker and counts requests; a container test cannot see a wasted
   retry.
7. **README + CHANGELOG** of its own, MSRV stated, and the crate added to the
   root README's companion table **and its MSRV table**.
8. **CI and release plumbing** — the crate in ci.yml's `containers` job (tests
   *and* the examples build list), its image in the pre-pull list, an MSRV
   matrix row, `release.yml`'s third wave, `publish-dry-run.yml`'s two crate
   lists, and `security.yml`'s forbid-unsafe list.
9. **The justfile** — `containers`, `examples`, `msrv` and (if it has mock
   tests) `mocks` recipes all name crates explicitly.
10. **The paperwork** — the feature-request template's crate dropdown,
    `docs/CONTRIBUTOR-ONBOARDING.md`'s store table, and `RELEASING.md`'s wave
    diagram.

## Three rules every store here follows

- **The current value is not delivered at startup.** A watch reports changes;
  announcing what the caller already has makes every restart look like an edit.
- **A deleted key is not a change.** No configuration is not a configuration.
- **A transport failure retries; a callback failure stops.** The store going
  away is what a watch exists to survive.

## Things that have bitten

- Pre-pull images in CI: a Docker Hub 429 looks exactly like a broken test.
  Prefer a registry without anonymous limits (`quay.io`, `gcr.io`).
- `--no-default-features` on an SDK can remove its HTTP client. Enable one
  explicitly and say why in a comment.
- Anything a server sends is untrusted: a lease duration goes through
  `checked_add`, a body may not be JSON, a value may not be UTF-8.
- A password in a URL must be redacted before it reaches an error message —
  including the URL-parse error path, which is the one most likely to be
  pasted somewhere.
- Never decide a retry by searching an error's *text* for a status code: the
  key or path appears in every message, and a key named `403.json` makes every
  error read as a refused token. Match the client's typed status
  (`ureq::Error::StatusCode(403)`) before anything becomes a string.
- A watch that can never fire must refuse at start (`watch()` returns an
  error), not poll forever: a missing format, a v1 Vault mount, a Firestore
  document with no `updateTime`.
