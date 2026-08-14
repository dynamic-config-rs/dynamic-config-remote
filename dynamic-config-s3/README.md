# dynamic-config-s3

Read [`dynamic-config`] configuration from an S3 object — on AWS, or on anything
that speaks its API.

```toml
[dependencies]
dynamic-config = { version = "0.6.1", features = ["async"] }
dynamic-config-s3 = "0.6.1"
```

```rust
use dynamic_config_s3::S3;

DbConfig::set_remote_async(S3::new("myapp-config", "prod/db.json").await?);

DbConfig::refresh_remote_async().await?;
```

The AWS SDK is async throughout, so this implements the **async**
`AsyncRemoteSource` trait rather than the blocking one.

## What it reads

One object, whose body is **a whole configuration document** — the same bytes
that would be in a config file. The format comes from the key's extension, or
from `with_format`.

## Several objects as one document

```rust
use dynamic_config_s3::{Keys, S3};

// Named keys: a list of layers, merged in call order — later wins.
S3::new("myapp-config", Keys::several(["prod/base.json", "prod/local.json"])).await?;

// A prefix: disjoint sections, and an overlap between two of them is an error
// naming both keys and the paths.
S3::new("myapp-config", Keys::prefix("prod/")).await?.with_format(Format::Json);
```

| | Requests | Consistency | Ceiling |
|---|---|---|---|
| `Keys::several` | one `GetObject` **per key** — S3 has no batch read | **not atomic** | the caller's list |
| `Keys::prefix` | one `ListObjectsV2` (paginated), then one `GetObject` per key | **not atomic** — the listing and the reads are separate requests | 512 keys, checked **on the listing** |

AWS made `ListObjectsV2` strongly consistent in December 2020, so the listing
itself is no longer the hole it once was — but another implementation of this API
is free to be eventually consistent, and the gap between the listing and the
reads is there in every case.

The budget bites on the **listing**: each page asks for one key more than the
budget allows, so a prefix pointed at a whole bucket is refused after one request
rather than after a million bodies. A key the store answers with that is not
under the prefix is refused, a key ending in `/` is the zero-byte "folder" a
console leaves behind and is skipped, and a listing whose continuation token
never clears is given up on after thirty-two pages.

One unreadable key fails the whole fetch, naming it. Provenance becomes
store-grained: the merged document is one layer, so `source_of` names the store
and the set rather than which key supplied a value. A multi-key source refuses to
be watched — an ETag belongs to an object, and a set of them has none — so poll
`refresh_remote_async()` on a timer.

## Credentials

Through `aws-config`, which is the chain every AWS tool uses:
`AWS_ACCESS_KEY_ID`, the shared profile, the EC2 instance role, the ECS task
role, and IRSA on EKS. That is deliberately not re-implemented here — a second
credential chain in a program that already has one is a bug waiting for a
rotation.

## Not only AWS

`with_config` takes an `SdkConfig` the program already built, which is how a
non-AWS endpoint is reached. Path-style addressing is forced, because
`http://bucket.host/key` needs DNS entries only AWS has:

```rust
let config = aws_config::from_env()
    .endpoint_url("http://minio.internal:9000")
    .load()
    .await;

let s3 = S3::with_config(&config, "myapp-config", "prod/db.json");
```

MinIO, Ceph, Cloudflare R2 and Backblaze B2 all work this way — and the test
suite runs against MinIO, so that is checked rather than claimed.

## TLS

`with_tls` takes the same data-only `TlsConfig` every store in this family
takes, with no SDK type in the calling code:

```rust
use dynamic_config_s3::{S3, TlsConfig};

let config = aws_config::from_env()
    .endpoint_url("https://minio.internal:9000")
    .load()
    .await;

let s3 = S3::with_tls(
    &config,
    "myapp-config",
    "prod/db.json",
    &TlsConfig::new().with_ca_certificate_file("/etc/ssl/private-ca.pem"),
)?;
```

This is for the S3-compatible servers, which is where a private authority
actually turns up: MinIO, Ceph and a company's own gateway all present
certificates AWS' public chain has never heard of.

**S3 cannot express a client certificate.** The SDK reaches TLS through
`aws-smithy-http-client`, whose `TlsContext` is a trust store and nothing else
— there is no slot to fill, at any version this crate can depend on. So mTLS is
**refused**, naming the call and pointing at `from_client`, which is where a
caller builds the connector themselves. It is not ignored, because a caller who
asked to present a certificate and did not would discover it as an
authentication failure a long way from the cause.

The certificate is parsed here purely in order to refuse: the SDK's rustls
connector calls `.expect("cert parsable")` on the material, so one it cannot
read would otherwise be a panic at the first connection. There is no way to
turn verification off, and the book's [remote stores chapter](https://github.com/ctolon/dynamic-config/blob/main/book/src/remote-stores.md#tls-and-the-one-vocabulary-all-seven-speak) argues that one.

## Timeouts

`with_timeout(..)` is **the deadline for a single fetch attempt, excluding
retries the underlying client performs** — the same sentence every store in this
family answers to. Here the exclusion is not a technicality.

**The AWS SDK retries underneath it.** `with_timeout` maps onto the SDK's
`operation_attempt_timeout`, which is per attempt, so with the default three
attempts:

```text
with_timeout(5s)  ×  3 attempts  =  a fetch that can take 15 seconds
```

That is documented rather than tuned away. A retry policy is a deployment's
decision, and a configuration library quietly disabling one it did not set would
be overruling that decision. Two ways to change the arithmetic, both on the
`SdkConfig` you hand to `with_config`:

| Want | Set |
|---|---|
| a ceiling on the whole call, retries included | `TimeoutConfig::operation_timeout` |
| fewer attempts, or none | `RetryConfig::with_max_attempts`, `RetryConfig::disabled()` |

The SDK sets no timeout at all by default, so calling `with_timeout` is
additive: nothing that worked before starts failing, and a fetch that used to
hang now stops.

## Watching

S3 cannot say when an object changes without a notification pipeline — SNS, SQS,
EventBridge — and that is a deployment's decision rather than a library's. So
`watch` polls, and says so.

What it does *not* do is download the object every tick. `HEAD` returns the
ETag, which changes when the body does, so an unchanged configuration costs one
small request and no transfer — which matters on a bucket that charges per
gigabyte.

```rust
s3.watch(&watching, Duration::from_secs(30), move |document| sink.apply(document)).await
```

- The current object is **not** delivered at startup.
- The ETag is taken from the *read*, not from the check that preceded it, so a
  write landing between the two is not delivered twice.
- A failed check does not end the watch. Stopping is noticed within a quarter
  second whatever the interval is.

### A failing poll says so

`reporting_to(sink)` hands the loop the same `RemoteSink` it already delivers
through, and the failures **inside** the loop are reported to it — the `HEAD`
that did not answer, and the `GET` that did not answer after the ETag moved:

```rust
let sink = DbConfig::remote_sink();

s3.reporting_to(sink)
    .watch(&watching, Duration::from_secs(30), move |document| sink.apply(document))
    .await
```

Surviving a failure is what makes this necessary: a poll loop that retries
forever is a loop that reports nothing forever. Without it only deliveries are
recorded, so `dynamic_config_remote_up` reports the last *delivery* rather than
the last *attempt* — and an expired credential, a bucket policy that changed
under the process or a gateway that went away is indistinguishable from a
configuration nobody has changed. A failure moves the failure streak and the
last failure and nothing else, so `remote_up` goes to zero while
`remote_last_fetch_seconds` keeps ageing — the pair an alert wants: down, and
stale for how long. Only a kind and a key path are recorded; the bucket, the
key and the endpoint stay out of it.

**Refusals at the door do not report** — no format, or a source naming several
keys. `watch()` returns those to the caller standing there, before there is a
loop to be silent in, and they are deployment mistakes rather than a store that
stopped answering. `on_change`'s own refusal does not report either: the store
answered, and a document that will not install is `ConfigStatus`'s half of the
picture.

## Builders

| Method | Default |
|---|---|
| `with_format(..)` | from the key's extension |
| `with_config(..)` | credentials from the environment |
| `with_timeout(..)` | none — the SDK sets no timeout of its own |
| `reporting_to(..)` | nothing reported; a failing poll is invisible |
| `from_client(..)` | builds its own client |
| `with_tls(..)` *(constructor)* | the platform trust store; **no client certificate** |

## Testing

The test suite drives **MinIO in a container** — the same API, offline and free.
That the crate works against MinIO at all is itself the assertion that matters
for everyone on Ceph, R2 or B2.

```sh
cargo test -p dynamic-config-s3    # needs a working Docker daemon
```

## MSRV

1.88 — higher than [`dynamic-config`]'s own 1.71, because the AWS SDK moves
faster than that crate wants to.

## License

MIT

[`dynamic-config`]: https://docs.rs/dynamic-config
