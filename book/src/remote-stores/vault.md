# Vault

[`dynamic-config-vault`](https://docs.rs/dynamic-config-vault) reads
configuration from HashiCorp Vault — its KV v2 store, or since 0.10 a
dynamic-secret engine — over the plain-HTTP API: the **blocking**
`RemoteSource`.

```toml
[dependencies]
dynamic-config = "<version>"
dynamic-config-vault = "<version>"
```

```rust
use dynamic_config_vault::{Auth, Vault};

DbConfig::set_remote(
    Vault::new("https://vault.internal:8200", "secret", "myapp/db")
        .with_auth(Auth::kubernetes("myapp")),
);
DbConfig::refresh_remote()?;
```

**What it reads:** `{mount}/data/{path}`, taking the value half of the
KV v2 response — a *map of named fields*, wrapped under the section key
(`with_key`, `"db"` by default — it must match the key given to
`builder(..)`). That is the opposite of Consul and NATS, and not a whim:
Vault stores fields, a KV bucket stores opaque bytes, and each is easiest
to use as what it already is.

**Several paths, one section:** `Keys::several(["myapp/db-defaults",
"myapp/db-credentials"])` reads both and merges them under that same section
key, in call order — later wins. That is layering, and it is the shape
Vault's own access control produces: a policy applies to a path, so
splitting a section into a public half and a restricted half is something
only Vault can do, and this reads it back as one section. The price is
stated rather than hidden — KV v2 has no batch read, so a list is **one
request per path and is not atomic**, and every one of those requests is a
line in the audit log on every fetch. One unreadable path fails the whole
fetch, naming it.

There is **no prefix form**, and `LIST` is not what is missing:
folding a subtree into one section would make `myapp/db` and `myapp/server`
collide on `host`. [The reason in
full](../remote-stores.md#two-stores-hold-fields-not-documents).

**Logging in:** every Vault auth method ends in the same place — a client
token with a lease — so that is all `Auth` models: `token`, `app_role`,
`kubernetes`, `jwt`, `userpass`, `ldap`, `certificate` (presented by your
own agent), with `at_mount` and `with_role` where the mount needs them.
Logging in is lazy; Kubernetes tokens are re-read at every login.

**TLS as data:** `.with_tls(TlsConfig::new().with_ca_certificate_file(..))`
takes a private certificate authority and a client certificate as paths or
PEM bytes, with no `ureq` type in the calling code — which is what a
Vault behind an internal CA needs, and what a `VAULT_CACERT` in the
environment already names. Setting `with_agent` as well is refused at the
first request rather than resolved. [TLS, and the one vocabulary all eight speak](../remote-stores.md#tls-and-the-one-vocabulary-all-eight-speak)

**Expiry is handled twice, on purpose:** a token within thirty seconds of
expiry is renewed or replaced *before* the request, and a `403` replaces
one *after* it — once. Clocks skew and Vault revokes; the proactive path
cannot catch everything, and a reader that gives up on the first `403`
will eventually do so at three in the morning.

**Watching:** Vault is the one store here that cannot say when something
changed — no watch, no blocking query — so `watch` polls and says so. It
does *not* pull the secret every tick: KV v2 keeps a version counter in
its metadata, so each tick reads `current_version` and transfers the
secret only when the number moves — never decrypted, never in the audit
log as a read, until it actually changed. Stopping is noticed within a
quarter second whatever the interval. A **multi-path source refuses to be
watched**: that version counter belongs to one secret, and a set of them
has none of its own.

**A failing watch says so:** `.reporting_to(sink)` hands the loop the same
`RemoteSink` its callback applies documents through, and every tick that comes
back with nothing is recorded on it — a metadata check that failed, a version
that moved beside a secret that will not be read, a mount that turns out not
to keep a version counter at all. The gap is wider here than anywhere else in
this family precisely because this watch polls a *counter*: a secret nobody
has rewritten and a Vault that sealed itself yesterday deliver exactly the
same nothing, and without this `dynamic_config_remote_up` reports the last
delivery rather than the last attempt. A failed attempt moves the failure
streak and nothing else, so `dynamic_config_remote_last_fetch_seconds` keeps
ageing while `remote_up` goes to zero — the pair that says both *the Vault is
not answering* and *how stale what it last said has become*. Reporting is
infallible and silent: a loop is never handed a failure to report a failure. A
`fetch()` needs none of it, because a fetch already records itself.
[The remote store's own numbers](https://dynamic-config-rs.github.io/telemetry.html#the-remote-stores-own-numbers)

## Dynamic secrets

`with_dynamic()` reads an engine that *mints* credentials —
`database/creds/my-role`, `pki/issue/…`, `aws/creds/…` — rather than a
document somebody wrote:

```rust
use dynamic_config_vault::{Auth, Vault};

DbConfig::set_remote(
    Vault::new("https://vault.internal:8200", "database", "creds/app")
        .with_auth(Auth::kubernetes("myapp"))
        .with_dynamic(),
);
```

Three things change.

**The path.** `{mount}/{path}` rather than `{mount}/data/{path}`, and the
values come from `data` rather than `data.data`. The mode is stated rather
than detected from the response, because a mistyped mount should be an
error and not a quiet fall into the other shape — and each mode's refusal
names the builder to add or drop.

**The lease.** The response's `lease_id`, `lease_duration` and `renewable`
ride along on the `Fetched`. Renewing and revoking are
[`RenewableSource`](https://docs.rs/dynamic-config): `renew` through
`sys/leases/renew`, `revoke` through `sys/leases/revoke`. **What a renewal
answers is authoritative** — a role's `max_ttl` is a ceiling the caller
cannot see, so schedule the next renewal from what Vault granted, never from
what was asked for.

**The certificate's own clock, for `pki/issue`.** A PKI role can hand back
a lease longer than the certificate it issued: the lease is Vault's
accounting record, and `notAfter` is what a TLS peer enforces. The `Lease`
therefore carries **whichever runs out first**. Vault reports `notAfter` as
`data.expiration`, seconds since the epoch, so this needs no X.509 parser —
the number is already in the response.

That timestamp is Vault's wall clock read against the caller's, and the two
are not the same clock, so an `expiration` that has *already* passed is
treated as skew and the lease's own duration wins. Vault would not issue an
expired certificate; believing the arithmetic instead would re-issue in a
tight loop against a server that thinks everything is fine.

**One path.** Dynamic mode reads a single path and refuses a list. Every
read mints a credential with a lease of its own, and a document carries one
— merging several would leave every lease but one unrenewed and unrevoked,
which is a credential leak wearing a merge's clothes.

The watch capability drops to `Interval`, because there is nothing cheap to
poll: a dynamic engine has no version counter and no metadata endpoint, and
asking *has it changed* is not separable from asking for a new one.

This is a **capability**, not a replacement for the credential the client
authenticates with. That one is already managed — refreshed before expiry,
re-obtained when refused — and the two lifetimes stay separate: one belongs
to the connection, the other to the document.

The [README](https://github.com/dynamic-config-rs/dynamic-config-remote/tree/main/dynamic-config-vault)
carries the full story, the auth and builder tables and the
`vault_kubernetes` example; MSRV 1.85.
