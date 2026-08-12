# Store Crates at a Glance

One row per store, with the contract columns that differ between them. The
prose behind every column is in [Remote Stores](../remote-stores.md); each
store also has its own chapter here — [etcd](etcd.md), [Consul](consul.md),
[NATS](nats.md), [Redis](redis.md), [Vault](vault.md), [S3](s3.md),
[Firestore](firestore.md) — and its README has the whole story.

## The async family — a watch is a future

Cancelled by dropping the future, on any executor. Stop latency is immediate
for the streaming protocols; S3 is async but has no change stream, so it polls.

| Crate | Store | Watches by | Worst case for noticing a stop | Startup delivery | Deleted key | Transport failure |
|---|---|---|---|---|---|---|
| [`dynamic-config-etcd`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-etcd) | etcd v3 | a watch stream | immediate — the future is cancelled | not delivered; fetch first | not a change | an expired token re-authenticates and resumes from the last delivered revision; other stream errors end the watch |
| [`dynamic-config-nats`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-nats) | NATS JetStream KV | a KV change stream | immediate — the future is cancelled | not delivered; fetch first | not a change | backs off and retries |
| [`dynamic-config-s3`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-s3) | S3, and anything speaking it | polling the ETag | a quarter second, whatever the poll interval is | not delivered; fetch first | not a change | backs off and retries |

## The blocking family — a watch is a thread

A thread cannot be dropped from outside, so it takes a [`Watching`] token and
checks it between requests; dropping the matching `RemoteWatch` stops it.

| Crate | Store | Watches by | Worst case for noticing a stop | Startup delivery | Deleted key | Transport failure |
|---|---|---|---|---|---|---|
| [`dynamic-config-consul`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-consul) | Consul KV | a blocking query | the blocking query's `wait`, one minute by default | not delivered; fetch first | not a change | backs off and retries |
| [`dynamic-config-redis`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-redis) | Redis | keyspace notifications | a quarter second, whatever the poll interval is | not delivered; fetch first | not a change | fetch failures retry; a dead subscription ends the watch |
| [`dynamic-config-vault`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-vault) | Vault KV v2 | polling the version | a quarter second, whatever the poll interval is | not delivered; fetch first | not a change | backs off and retries |
| [`dynamic-config-firestore`](https://github.com/ctolon/dynamic-config/tree/main/dynamic-config-firestore) | Firestore | polling `updateTime` | a quarter second, whatever the poll interval is | not delivered; fetch first | not a change | backs off and retries |

## The shared contract

The startup-delivery and deleted-key columns are identical on purpose — they
are decisions rather than accidents, and they hold across all seven crates.
Transport failures retry too, except where the table names an error that ends
the watch, deliberately, so a supervisor can restart it:

- **The current value is not delivered at startup.** A watch reports changes;
  announcing the value the caller already has would make every restart look like
  an edit. Fetch first if the starting value matters — it usually does.
- **A deleted key is not a change.** No configuration is not a configuration,
  and neither replaying the last one nor pushing emptiness is better than
  leaving the running snapshot alone.
- **A transport failure retries rather than ending the watch** — with the
  table's named exceptions: an etcd stream error no token refresh can cure,
  and a Redis subscription that died, both of which end the watch with an
  error. An error from *your* callback always ends it, so a caller that wants
  to survive a bad document should log it and return `Ok`.

Credential handling is also uniform: logging in is lazy, expiry is handled both
before and after a request (with exactly one retry), and a credential read from
a file is re-read at every login. See
[Credentials, and keeping them working](../remote-stores.md#credentials-and-keeping-them-working).

[`Watching`]: https://docs.rs/dynamic-config/latest/dynamic_config/struct.Watching.html
