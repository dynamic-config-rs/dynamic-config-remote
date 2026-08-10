//! Read [`dynamic-config`] configuration from an S3 object.
//!
//! The AWS SDK is async throughout, so this implements the **async**
//! [`AsyncRemoteSource`] trait rather than the blocking one.
//!
//! ```no_run
//! use dynamic_config_s3::S3;
//!
//! # struct DbConfig;
//! # impl DbConfig {
//! #     fn set_remote_async(_: S3) {}
//! #     async fn refresh_remote_async() -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Credentials come from the environment the way every other AWS tool finds
//! // them: variables, the profile, the instance role, IRSA.
//! DbConfig::set_remote_async(S3::new("myapp-config", "prod/db.json").await?);
//!
//! // Fetching is explicit; the load that follows touches no network.
//! DbConfig::refresh_remote_async().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # What it reads
//!
//! One object, whose body is **a whole configuration document** — the same
//! bytes that would be in a config file. The format comes from the key's
//! extension, or from [`with_format`](S3::with_format).
//!
//! # Credentials
//!
//! Through `aws-config`, which is the chain every AWS tool uses:
//! `AWS_ACCESS_KEY_ID`, the shared profile, the EC2 instance role, the ECS task
//! role, and IRSA on EKS. That is deliberately not re-implemented here — a
//! second credential chain in a program that already has one is a bug waiting
//! for a rotation.
//!
//! [`with_config`](S3::with_config) takes an `SdkConfig` the program already
//! built, which is also how a non-AWS endpoint is reached: MinIO, Ceph,
//! Cloudflare R2, Backblaze B2 all speak this API.
//!
//! # Watching
//!
//! S3 cannot tell you when an object changes without a notification pipeline —
//! SNS, SQS, EventBridge — that is a deployment's decision, not a library's. So
//! [`watch`](S3::watch) polls, and says so.
//!
//! What it does not do is download the object every tick. `HEAD` returns the
//! ETag, which changes when the body does, so an unchanged configuration costs
//! one small request and no transfer.
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use dynamic_config::{AsyncRemoteSource, Error, Fetched, Format, Watching};

/// The AWS types a caller needs to configure this, re-exported so using them
/// needs no direct dependency on the SDK.
pub use aws_config::SdkConfig;
pub use aws_sdk_s3::Client;

/// An object in S3, as a configuration source.
pub struct S3 {
    client: Client,
    bucket: String,
    key: String,
    format: Option<Format>,
    /// The endpoint override, when the construction path knew one. Only for
    /// `describe()`: the endpoint tells MinIO apart from AWS in an error.
    endpoint: Option<String>,
}

impl S3 {
    /// The object `key` in `bucket`, with credentials from the environment.
    ///
    /// The format is taken from the key's extension — `prod/db.json` is JSON. A
    /// key without one needs [`with_format`](Self::with_format).
    ///
    /// This resolves credentials, which may read a file or call the instance
    /// metadata service — the one constructor in this family that does I/O,
    /// because the credential chain is what it is.
    pub async fn new(bucket: impl Into<String>, key: impl Into<String>) -> Result<Self, Error> {
        let config = aws_config::load_from_env().await;

        Ok(Self::with_config(&config, bucket, key))
    }

    /// Uses an `SdkConfig` the program already built.
    ///
    /// For a caller that already talks to AWS, and for anything that is not
    /// AWS: MinIO, Ceph, R2 and B2 all speak this API, and all of them need an
    /// endpoint override the environment cannot express.
    ///
    /// ```no_run
    /// # use dynamic_config_s3::S3;
    /// # async fn example() {
    /// let config = aws_config::from_env()
    ///     .endpoint_url("http://minio.internal:9000")
    ///     .load()
    ///     .await;
    ///
    /// let s3 = S3::with_config(&config, "myapp-config", "prod/db.json");
    /// # }
    /// ```
    #[must_use]
    pub fn with_config(
        config: &SdkConfig,
        bucket: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        // `force_path_style` is what makes every S3-compatible server work:
        // `http://host/bucket/key` rather than `http://bucket.host/key`, which
        // needs DNS entries only AWS has.
        let s3 = aws_sdk_s3::config::Builder::from(config)
            .force_path_style(true)
            .build();

        let mut source = Self::from_client(Client::from_conf(s3), bucket, key);
        source.endpoint = config.endpoint_url().map(str::to_owned);

        source
    }

    /// Uses a client the program already has.
    #[must_use]
    pub fn from_client(client: Client, bucket: impl Into<String>, key: impl Into<String>) -> Self {
        let key = key.into();

        let format = Format::from_key(&key);

        Self {
            client,
            bucket: bucket.into(),
            key,
            format,
            endpoint: None,
        }
    }

    /// States the format, for a key whose name does not.
    #[must_use]
    pub fn with_format(mut self, format: Format) -> Self {
        self.format = Some(format);
        self
    }

    /// Calls `on_change` when the object's ETag moves, checking every
    /// `interval`.
    ///
    /// Polling, because S3 offers nothing better without a notification
    /// pipeline — and *ETag* polling, because downloading an object every
    /// thirty seconds to discover it has not changed is a poor thing to do to a
    /// bucket that charges per gigabyte. Each tick is a `HEAD`; only a new ETag
    /// costs a `GET`.
    ///
    /// The current value is **not** delivered at startup, for the same reason a
    /// file watcher does not report an edit when it starts. Fetch first if the
    /// starting value matters.
    ///
    /// A failed check does not end the watch — an expired credential, a network
    /// blip, a bucket briefly unreachable — it waits out the interval and tries
    /// again. `stop` is noticed within a quarter second whatever `interval` is.
    ///
    /// # Errors
    ///
    /// If the key names no format and none was stated — a watch that cannot
    /// parse what it fetches would poll forever and deliver nothing, so it
    /// refuses at the start instead. Or if `on_change` returns an error, which
    /// ends the watch. Transport failures do not surface here; they are
    /// retried.
    pub async fn watch<F>(
        &self,
        watching: &Watching,
        interval: Duration,
        mut on_change: F,
    ) -> Result<(), Error>
    where
        F: FnMut(Fetched) -> Result<(), Error> + Send,
    {
        // Checked before the first tick: with no format every `read` inside
        // the loop fails, and the `if let Ok` there — right for a transient
        // network failure — would swallow a permanent configuration mistake.
        self.format.ok_or_else(|| {
            Error::remote(format!(
                "{}: the key names no format; call `with_format`",
                self.describe()
            ))
        })?;

        let mut seen: Option<String> = None;

        while watching.keep_going() {
            match self.etag().await {
                // The first tick records the tag without firing: the object it
                // names is the one the caller already has.
                Ok(tag) if seen.is_none() => seen = Some(tag),

                Ok(tag) if seen.as_ref() != Some(&tag) => {
                    // The tag is taken from the read itself rather than from
                    // the check, so a write landing between the two is not
                    // delivered twice.
                    if let Ok((document, current)) = self.read().await {
                        seen = current.or(Some(tag));

                        on_change(document)?;
                    }
                }

                // Unchanged, or the check failed. Either way, wait.
                _ => {}
            }

            sleep_while(interval, watching).await;
        }

        Ok(())
    }

    /// The object's ETag, which changes when its body does.
    async fn etag(&self) -> Result<String, Error> {
        let head = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&self.key)
            .send()
            .await
            .map_err(|error| Error::remote(format!("{}: {error}", self.describe())))?;

        head.e_tag()
            .map(str::to_owned)
            .ok_or_else(|| Error::remote(format!("{}: the object has no ETag", self.describe())))
    }

    /// The object, and the ETag it was read at.
    async fn read(&self) -> Result<(Fetched, Option<String>), Error> {
        let format = self.format.ok_or_else(|| {
            Error::remote(format!(
                "{}: the key names no format; call `with_format`",
                self.describe()
            ))
        })?;

        let object = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&self.key)
            .send()
            .await
            .map_err(|error| Error::remote(format!("{}: {error}", self.describe())))?;

        let tag = object.e_tag().map(str::to_owned);

        let bytes = object
            .body
            .collect()
            .await
            .map_err(|error| Error::remote(format!("{}: {error}", self.describe())))?
            .into_bytes();

        let text = String::from_utf8(bytes.to_vec()).map_err(|error| {
            Error::remote(format!(
                "{}: the object is not UTF-8: {error}",
                self.describe()
            ))
        })?;

        Ok((Fetched::new(text, format), tag))
    }
}

impl AsyncRemoteSource for S3 {
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<Fetched, Error>> + Send + '_>> {
        Box::pin(async move { self.read().await.map(|(document, _tag)| document) })
    }

    fn describe(&self) -> String {
        // The endpoint tells MinIO apart from AWS, and one MinIO from
        // another — the detail that matters when an error says "no such
        // bucket" and there are three object stores it could mean.
        match &self.endpoint {
            Some(endpoint) => format!("s3 {endpoint} {}/{}", self.bucket, self.key),
            None => format!("s3 {}/{}", self.bucket, self.key),
        }
    }
}

impl std::fmt::Debug for S3 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3")
            .field("bucket", &self.bucket)
            .field("key", &self.key)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

/// Sleeps in slices, so a stop is noticed inside the interval rather than after
/// it — a thirty-second poll should not mean a thirty-second exit.
async fn sleep_while(total: Duration, watching: &Watching) {
    const SLICE: Duration = Duration::from_millis(250);

    let mut slept = Duration::ZERO;

    while slept < total && watching.keep_going() {
        tokio::time::sleep(SLICE).await;
        slept += SLICE;
    }
}
