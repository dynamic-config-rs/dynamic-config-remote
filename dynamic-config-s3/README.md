# dynamic-config-s3

Read [`dynamic-config`] configuration from an S3 object — on AWS, or on anything
that speaks its API.

```toml
[dependencies]
dynamic-config = { version = "0.5.0", features = ["async"] }
dynamic-config-s3 = "0.5.0"
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

## Builders

| Method | Default |
|---|---|
| `with_format(..)` | from the key's extension |
| `with_config(..)` | credentials from the environment |
| `from_client(..)` | builds its own client |

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
