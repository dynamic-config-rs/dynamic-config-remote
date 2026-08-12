# Vault

[`dynamic-config-vault`](https://docs.rs/dynamic-config-vault) reads
configuration from HashiCorp Vault's KV v2 store, over its plain-HTTP API:
the **blocking** `RemoteSource`.

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

**Logging in:** every Vault auth method ends in the same place — a client
token with a lease — so that is all `Auth` models: `token`, `app_role`,
`kubernetes`, `jwt`, `userpass`, `ldap`, `certificate` (presented by your
own agent), with `at_mount` and `with_role` where the mount needs them.
Logging in is lazy; Kubernetes tokens are re-read at every login.

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
quarter second whatever the interval.

The [README](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-vault)
carries the full story, the auth and builder tables and the
`vault_kubernetes` example; MSRV 1.85.
