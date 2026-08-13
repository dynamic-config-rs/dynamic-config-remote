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
//! # Several objects as one document
//!
//! A deployment that splits its configuration across a prefix —
//! `prod/db.json`, `prod/server.json` — can have one source read the lot, and
//! [`Keys`] says which:
//!
//! ```no_run
//! # use dynamic_config_s3::{Keys, S3};
//! # async fn example() -> Result<(), dynamic_config::Error> {
//! // Named keys: a list of layers, merged in the order given, later wins.
//! let s3 = S3::new("myapp-config", Keys::several(["prod/base.json", "prod/local.json"])).await?;
//!
//! // A prefix: disjoint sections, and an overlap between two of them is an error.
//! let s3 = S3::new("myapp-config", Keys::prefix("prod/"))
//!     .await?
//!     .with_format(dynamic_config::Format::Json);
//! # Ok(())
//! # }
//! ```
//!
//! **Neither shape is atomic, and S3 offers nothing that would make one so.**
//! There is no batch read: a named list is one `GetObject` per key, and a
//! prefix is one `ListObjectsV2` and then one `GetObject` per key it named. A
//! write landing between two of those requests can produce a document that
//! never existed as a whole. AWS made `ListObjectsV2` strongly consistent in
//! December 2020, so the listing itself is not the hole it once was — but
//! another implementation of this API is free to be eventually consistent, and
//! the gap between the listing and the reads is there in every case.
//!
//! **The 512-key bound is applied to the listing, not to what it fetched.**
//! `ListObjectsV2` is paginated, so the count is checked as each page arrives
//! and a prefix over a bucket of a million objects is refused after one
//! request rather than after a million bodies. Every key the store answers
//! with is checked against the literal prefix, and a key ending in `/` — the
//! zero-byte object the console makes when somebody creates a "folder" — is
//! not a document and is skipped.
//!
//! Three consequences that belong here rather than in an incident:
//!
//! - **Provenance becomes store-grained.** The merged document is one layer,
//!   so `source_of` answers "from s3 … keys a, b" rather than naming which key
//!   supplied a value. [`describe`](AsyncRemoteSource::describe) names the
//!   whole set, which is as close as one layer gets.
//! - **One unreadable key fails the whole fetch.** A configuration quietly
//!   missing a section is worse than a refresh that failed and left the last
//!   document serving.
//! - **A multi-key source cannot be watched.** What a watch delivers is the
//!   object that changed, and a merged document has no one ETag; it refuses at
//!   [`watch`](S3::watch) and points at polling `refresh_remote_async()`.
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
//! **A failing poll says so, if it is asked to.**
//! [`reporting_to`](S3::reporting_to) hands the loop the same sink it delivers
//! through, and the failures inside it — a `HEAD` that did not answer, and a
//! `GET` that did not answer after the ETag moved — are reported to the
//! `RemoteStatus` as they happen. Surviving a failure is what makes that
//! necessary: a loop that retries forever is a loop that reports nothing
//! forever, so `dynamic_config_remote_up` would describe the last *delivery*
//! rather than the last *attempt*.
//!
//! A credential the store will not accept — `AccessDenied`,
//! `InvalidAccessKeyId`, `SignatureDoesNotMatch`, an expired session token —
//! is reported as `ErrorKind::Auth` rather than `Remote`, because no amount of
//! waiting persuades S3 otherwise. A clock too far out of step
//! (`RequestTimeTooSkewed`) shares the same 403 and stays `Remote`: that one
//! does come right.
//!
//! # Timeouts
//!
//! [`S3::with_timeout`] is the deadline for a single fetch attempt, excluding
//! retries the underlying client performs. Here that exclusion has teeth: the
//! SDK retries, so **a five-second timeout with three attempts is a
//! fifteen-second call**. See the README's Timeouts section.
//!
//! # This crate needs a tokio runtime
//!
//! Not this crate's choice: the AWS SDK it is built on is tokio-based
//! (`rt-tokio`), and [`watch`](S3::watch) sleeps on tokio's timer. Driving
//! it from another executor panics inside the SDK. The etcd and NATS
//! companions are executor-agnostic; this one is honest about not being.
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use aws_sdk_s3::config::timeout::TimeoutConfig;
use dynamic_config::{AsyncRemoteSource, Error, Fetched, Format, RemoteSink, Watching};
use dynamic_config_store_core::attempts::Attempts;
use dynamic_config_store_core::documents::{self, Overlap, MOST_KEYS};
use dynamic_config_store_core::guarded;

/// The AWS types a caller needs to configure this, re-exported so using them
/// needs no direct dependency on the SDK.
pub use aws_config::SdkConfig;
pub use aws_sdk_s3::Client;

use dynamic_config_store_core::tls as tls_core;
/// A private certificate authority and a client certificate, as data.
///
/// The shared vocabulary all seven store crates take, so that reaching TLS
/// never means naming an SDK type — see [`S3::with_tls`]. S3 is one of two
/// stores here that cannot express the whole of it: the SDK's TLS context has
/// a trust store and no client-certificate slot, so mTLS is refused rather
/// than ignored.
pub use dynamic_config_store_core::tls::TlsConfig;
use rustls_pki_types::pem::PemObject;

use aws_sdk_s3::error::ProvideErrorMetadata;

/// The error codes S3 uses for a credential it will not accept.
///
/// Matched on the code rather than the 403 that carries them, because a 403
/// is also what `RequestTimeTooSkewed` arrives as — a clock problem that NTP
/// does fix, and so not something to stop a watch loop over.
const AUTH_CODES: [&str; 6] = [
    "AccessDenied",
    "InvalidAccessKeyId",
    "SignatureDoesNotMatch",
    "ExpiredToken",
    "InvalidToken",
    "TokenRefreshRequired",
];

/// The most `ListObjectsV2` pages one prefix read will ask for.
///
/// The key budget already stops a listing that is merely large: one page is
/// asked for `MOST_KEYS + 1` keys, so a prefix over anything bigger than the
/// budget is refused on the first or second page. This is the other failure —
/// a store that keeps answering "truncated" with a continuation token and no
/// keys, which the budget on the keys cannot see because the count never
/// moves.
///
/// Thirty-two rather than the two a well-behaved store needs: `max-keys` is a
/// *maximum*, and an implementation of this API is free to answer with fewer
/// than it was asked for. At sixteen keys a page this still reaches the whole
/// budget, which puts the cap well clear of a small page size and still well
/// short of a loop.
const MOST_LIST_PAGES: usize = 32;

/// What a source reads: one object, several named ones, or a prefix.
///
/// Every constructor takes one, and a bare `&str` or `String` is
/// [`Keys::one`] — so the single-key spelling every caller already wrote keeps
/// working unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Keys {
    /// One object, whose body is the whole document.
    One(String),
    /// Several named objects, merged **in the order given — later wins**.
    ///
    /// The rule a list of `.file(..)` calls already teaches: the caller wrote
    /// the list, so the list is the precedence. One `GetObject` per key,
    /// because S3 has no batch read — so the set is **not** read atomically.
    Several(Vec<String>),
    /// Every object under a literal prefix, merged as **disjoint sections**.
    ///
    /// A caller naming a prefix is not expressing an order — S3 lists keys in
    /// UTF-8 order, which is nobody's precedence — so two objects under it
    /// supplying the same path is a deployment bug, and reported as one rather
    /// than resolved. One `ListObjectsV2` (paginated) and then one
    /// `GetObject` per key.
    Prefix(String),
}

impl Keys {
    /// One object, whose body is the whole document.
    #[must_use]
    pub fn one(key: impl Into<String>) -> Self {
        Self::One(key.into())
    }

    /// Several named objects, merged in the order given — later wins.
    #[must_use]
    pub fn several<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Several(keys.into_iter().map(Into::into).collect())
    }

    /// Every object under `prefix`, merged as disjoint sections.
    #[must_use]
    pub fn prefix(prefix: impl Into<String>) -> Self {
        Self::Prefix(prefix.into())
    }

    /// The keys as a slice, for the diagnostics and the format inference.
    ///
    /// A prefix has none to list — the set is not known until the store
    /// answers.
    fn named(&self) -> &[String] {
        match self {
            Self::One(key) => std::slice::from_ref(key),
            Self::Several(keys) => keys,
            Self::Prefix(_) => &[],
        }
    }

    /// How a diagnostic names what this source reads.
    ///
    /// One key renders as the key itself, so every message a single-key source
    /// has ever produced is unchanged.
    fn describe(&self) -> String {
        match self {
            Self::One(key) => key.clone(),
            Self::Several(keys) => format!("keys {}", keys.join(", ")),
            Self::Prefix(prefix) => format!("prefix {prefix}"),
        }
    }
}

impl From<&str> for Keys {
    fn from(key: &str) -> Self {
        Self::one(key)
    }
}

impl From<String> for Keys {
    fn from(key: String) -> Self {
        Self::One(key)
    }
}

impl From<&String> for Keys {
    fn from(key: &String) -> Self {
        Self::one(key)
    }
}

/// An object in S3, as a configuration source.
pub struct S3 {
    client: Client,
    bucket: String,
    keys: Keys,
    format: Option<Format>,
    /// Why the keys' own extensions could not settle the format between them.
    ///
    /// Kept rather than reported at construction because `from_client` cannot
    /// fail and because `with_format` is allowed to settle it afterwards.
    disagreement: Option<String>,
    /// The endpoint override, when the construction path knew one. Only for
    /// `describe()`: the endpoint tells MinIO apart from AWS in an error.
    endpoint: Option<String>,
    /// Where the watch loop reports a poll that came back with nothing.
    ///
    /// Nobody, unless [`reporting_to`](S3::reporting_to) said otherwise: a
    /// fetch records itself through `refresh_remote_async`, and a poll is
    /// the half of this store `dynamic-config` cannot see on its own.
    attempts: Attempts,
}

impl S3 {
    /// The object `key` in `bucket`, with credentials from the environment.
    ///
    /// `key` is a key — `"prod/db.json"` — or a [`Keys`], for the
    /// several-objects and prefix forms.
    ///
    /// The format is taken from the key's extension — `prod/db.json` is JSON. A
    /// key without one, and every prefix, needs
    /// [`with_format`](Self::with_format).
    ///
    /// This resolves credentials, which may read a file or call the instance
    /// metadata service — the one constructor in this family that does I/O,
    /// because the credential chain is what it is.
    pub async fn new(bucket: impl Into<String>, key: impl Into<Keys>) -> Result<Self, Error> {
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
        key: impl Into<Keys>,
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

    /// As [`with_config`](Self::with_config), with a private certificate
    /// authority.
    ///
    /// The same vocabulary as the other six store crates, spelled as *data* —
    /// nothing here names an SDK or a `rustls` type:
    ///
    /// ```no_run
    /// # use dynamic_config_s3::{S3, TlsConfig};
    /// # async fn example() -> Result<(), dynamic_config::Error> {
    /// let config = aws_config::from_env()
    ///     .endpoint_url("https://minio.internal:9000")
    ///     .load()
    ///     .await;
    ///
    /// let s3 = S3::with_tls(
    ///     &config,
    ///     "myapp-config",
    ///     "prod/db.json",
    ///     &TlsConfig::new().with_ca_certificate_file("/etc/ssl/private-ca.pem"),
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// This is for the S3-compatible servers, which is where a private
    /// authority actually turns up: MinIO, Ceph and a company's own gateway all
    /// present certificates AWS' public chain has never heard of.
    ///
    /// # What S3 cannot express
    ///
    /// **A client certificate.** The SDK reaches TLS through
    /// `aws-smithy-http-client`, whose `TlsContext` has a trust store and
    /// nothing else — there is no client-certificate slot to fill, at any
    /// version this crate can depend on. So
    /// [`with_client_certificate_files`] and [`with_client_certificate_pem`]
    /// are **refused here**, naming the call and pointing at
    /// [`from_client`](Self::from_client) — not ignored, because a caller who
    /// asked to present a certificate and did not would discover it as an
    /// authentication failure a long way from the cause.
    ///
    /// A caller who needs mTLS to an S3-compatible server builds the connector
    /// themselves and hands over the finished `Client`. That is what the escape
    /// hatch is for, and it is untouched.
    ///
    /// **The CA replaces the platform trust store** rather than adding to it,
    /// which is what naming a private authority means. A deployment that needs
    /// both puts both in the file.
    ///
    /// There is no way to turn verification off; [`TlsConfig`]'s own
    /// documentation argues that one, and the SDK's TLS context offers no such
    /// switch to forward even if this crate wanted to.
    ///
    /// # Errors
    ///
    /// If the configuration names a client certificate, if a PEM file cannot be
    /// read, or if the TLS context will not build.
    ///
    /// [`with_client_certificate_files`]: TlsConfig::with_client_certificate_files
    /// [`with_client_certificate_pem`]: TlsConfig::with_client_certificate_pem
    pub fn with_tls(
        config: &SdkConfig,
        bucket: impl Into<String>,
        key: impl Into<Keys>,
        tls: &TlsConfig,
    ) -> Result<Self, Error> {
        let bucket = bucket.into();
        let described = format!("s3 {bucket}");

        if tls.client_certificate().is_some() {
            return Err(tls_core::unsupported(
                &described,
                "a client certificate",
                "the AWS SDK's TLS context has a trust store and no \
                 client-certificate slot; build the connector yourself and use \
                 `from_client`",
            ));
        }

        let mut trust_store = aws_smithy_http_client::tls::TrustStore::empty();

        if let Some(pem) = tls.ca_certificate_pem(&described)? {
            // Parsed here and thrown away, purely to refuse. The SDK's rustls
            // connector calls `.expect("cert parsable")` on this material, so
            // a certificate it cannot read is a *panic* at the first
            // connection — a long way from the call that supplied it, and in a
            // library whose whole job is to not take a process down. The
            // parser's own message is dropped for the reason it is dropped
            // everywhere in this family: it renders the line it choked on.
            let readable = rustls_pki_types::CertificateDer::pem_slice_iter(&pem)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| {
                    Error::remote(format!(
                        "{described}: the CA certificate is not PEM-encoded \
                         certificate material"
                    ))
                })?;

            if readable.is_empty() {
                return Err(Error::remote(format!(
                    "{described}: the CA certificate holds no certificate; it \
                     is refused rather than ignored"
                )));
            }

            trust_store = trust_store.with_pem_certificate(pem);
        }

        let context = aws_smithy_http_client::tls::TlsContext::builder()
            .with_trust_store(trust_store)
            .build()
            // The SDK renders the PEM parse failure underneath, and a parse
            // failure renders the line it choked on — so the upstream text is
            // dropped rather than wrapped, the same rule the whole family
            // holds to.
            .map_err(|_| {
                Error::remote(format!(
                    "{described}: the CA certificate was refused; check that it \
                     is PEM-encoded certificate material"
                ))
            })?;

        let http = aws_smithy_http_client::Builder::new()
            .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
                aws_smithy_http_client::tls::rustls_provider::CryptoMode::AwsLc,
            ))
            .tls_context(context)
            .build_https();

        // `force_path_style` for the same reason `with_config` sets it: it is
        // what makes every S3-compatible server work, and those are exactly the
        // servers a private authority belongs to.
        let s3 = aws_sdk_s3::config::Builder::from(config)
            .force_path_style(true)
            .http_client(http)
            .build();

        let mut source = Self::from_client(Client::from_conf(s3), bucket, key);
        source.endpoint = config.endpoint_url().map(str::to_owned);

        Ok(source)
    }

    /// Uses a client the program already has.
    ///
    /// The escape hatch, and it stays one: a connector this crate has no
    /// spelling for — mTLS, a proxy, a DNS resolver — is built here and handed
    /// over finished.
    #[must_use]
    pub fn from_client(client: Client, bucket: impl Into<String>, key: impl Into<Keys>) -> Self {
        let keys = key.into();

        let (format, disagreement) = match documents::agreed_format(keys.named()) {
            Ok(format) => (format, None),
            Err(complaint) => (None, Some(complaint)),
        };

        Self {
            client,
            bucket: bucket.into(),
            keys,
            format,
            disagreement,
            endpoint: None,
            attempts: Attempts::default(),
        }
    }

    /// States the format, for a key whose name does not.
    ///
    /// Required for [`Keys::Prefix`] — a prefix has no extension — and it also
    /// settles a list whose keys name two different formats.
    #[must_use]
    pub fn with_format(mut self, format: Format) -> Self {
        self.format = Some(format);
        // The caller has now said which format wins, so the keys no longer
        // have to agree between themselves.
        self.disagreement = None;
        self
    }

    /// Reports this source's **watch** failures to `sink`.
    ///
    /// A poll loop is the half of a store `dynamic-config` cannot see. A
    /// delivery keeps `RemoteStatus` current because
    /// [`RemoteSink::apply`] records one — but a poll that keeps failing
    /// delivers nothing and would otherwise say nothing: an expired
    /// credential, a bucket policy that changed under the process, a
    /// gateway that went away. `dynamic_config_remote_up` would report the
    /// last *delivery* rather than the last *attempt*, and a bucket that
    /// stopped answering an hour ago would look healthy until something
    /// called `refresh_remote_async()`.
    ///
    /// ```no_run
    /// # use dynamic_config_s3::S3;
    /// # struct DbConfig;
    /// # impl DbConfig {
    /// #     fn remote_sink() -> dynamic_config::RemoteSink { unimplemented!() }
    /// # }
    /// # async fn example() -> Result<(), dynamic_config::Error> {
    /// // Taken once, where the loop is wired: a sink captures the generation
    /// // of the source installed at that moment, which is what stops a loop
    /// // winding down from charging its failures to its replacement.
    /// let sink = DbConfig::remote_sink();
    ///
    /// let watcher = S3::new("myapp-config", "prod/db.json").await?.reporting_to(sink);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// **A failure moves the failure streak and nothing else.** The fetch
    /// count and the clock are left alone, so
    /// `dynamic_config_remote_last_fetch_seconds` keeps ageing while
    /// `dynamic_config_remote_up` goes to zero — the pair an alert wants.
    /// Only the failure's kind and key path are recorded; a bucket, an
    /// endpoint and a key never reach a `RemoteStatus`.
    ///
    /// It changes nothing about what [`watch`](Self::watch) *returns*, and
    /// nothing about [`fetch`](AsyncRemoteSource::fetch), which already
    /// records itself through `refresh_remote_async()`.
    ///
    /// [`RemoteSink::apply`]: dynamic_config::RemoteSink::apply
    #[must_use]
    pub fn reporting_to(mut self, sink: RemoteSink) -> Self {
        self.attempts = Attempts::to(sink);
        self
    }

    /// The format, or an error naming the call that supplies one.
    fn format(&self) -> Result<Format, Error> {
        if let Some(complaint) = &self.disagreement {
            return Err(Error::remote(format!("{}: {complaint}", self.describe())));
        }

        self.format.ok_or_else(|| {
            Error::remote(format!(
                "{}: the key names no format; call `with_format`",
                self.describe()
            ))
        })
    }

    /// The one key this source reads, or an error saying it reads several.
    ///
    /// A watch here compares ETags, and an ETag belongs to an object: a set of
    /// objects has none, and following one member's would fire on that member
    /// and miss every other.
    fn single_key(&self) -> Result<&str, Error> {
        match &self.keys {
            Keys::One(key) => Ok(key),
            _ => Err(Error::remote(format!(
                "{}: a source that reads several keys cannot be watched; \
                 poll `refresh_remote_async()` on a timer instead",
                self.describe()
            ))),
        }
    }

    /// What two of this source's keys supplying one path means.
    ///
    /// The distinction the feature turns on: a caller who wrote the list wrote
    /// the precedence with it, and a caller who wrote a prefix wrote no order
    /// at all — so the first merges and the second refuses.
    fn overlap(&self) -> Overlap {
        match self.keys {
            Keys::One(_) | Keys::Several(_) => Overlap::LaterWins,
            Keys::Prefix(_) => Overlap::Refused,
        }
    }

    /// How long a single fetch **attempt** may take before it is given up on.
    ///
    /// The deadline for one attempt, excluding retries the underlying client
    /// performs — the sentence every store in this family answers to, and the
    /// one place in the family where the exclusion is not a technicality.
    ///
    /// The AWS SDK retries on its own. So this maps onto
    /// `operation_attempt_timeout`, which is per attempt, and a fetch can take
    /// this multiplied by the attempt count — three, by default. That is
    /// documented rather than tuned away: the SDK's retry policy is a
    /// deployment's decision, and silently disabling it here would be this
    /// crate overruling it. Set `operation_timeout` on the `SdkConfig` for a
    /// ceiling on the whole call, or a retry policy for a different multiplier.
    ///
    /// The SDK has no timeout set at all by default, so this is additive:
    /// nothing that worked before starts failing, and a fetch that used to
    /// hang now stops.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        // Built from what the config already carries, so a caller's own
        // connect or read timeouts survive setting this one.
        let timeouts = self
            .client
            .config()
            .timeout_config()
            .map_or_else(TimeoutConfig::builder, TimeoutConfig::to_builder)
            .operation_attempt_timeout(timeout)
            .build();

        let config = self
            .client
            .config()
            .to_builder()
            .timeout_config(timeouts)
            .build();

        self.client = Client::from_conf(config);
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
    /// # What a failing loop reports
    ///
    /// Nothing, unless [`reporting_to`](Self::reporting_to) was given a sink.
    /// With one, both failures **inside** the loop are reported to the
    /// `RemoteStatus` as they happen: a `HEAD` that did not answer, and a
    /// `GET` that did not answer after the ETag moved. Surviving a failure is
    /// exactly what makes this necessary — a loop that retries forever is a
    /// loop that reports nothing forever, and a poll silently failing since
    /// Tuesday is indistinguishable from a configuration nobody has changed.
    ///
    /// The refusals **at the door** — no format, several keys — are not
    /// reported: they are returned to the caller by this very call, before
    /// there is a loop to be silent in, and they are deployment mistakes
    /// rather than a store that stopped answering.
    ///
    /// # Errors
    ///
    /// If the key names no format and none was stated — a watch that cannot
    /// parse what it fetches would poll forever and deliver nothing, so it
    /// refuses at the start instead. If the source reads several keys: an ETag
    /// belongs to an object, and a set of objects has none. Or if `on_change`
    /// returns an error, which ends the watch. Transport failures do not
    /// surface here; they are retried.
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
        self.format()?;
        // Refused up front for the same reason, so a multi-key source fails at
        // `watch` rather than on the first change, hours later.
        self.single_key()?;

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
                    match self.read().await {
                        Ok((document, current)) => {
                            seen = current.or(Some(tag));

                            guarded(&mut on_change, document, &self.describe())?;
                        }
                        // The `HEAD` answered and the `GET` did not, so the
                        // object this loop exists to deliver has changed and
                        // has not been delivered. `seen` is deliberately left
                        // where it was, so the next tick tries the same tag
                        // again rather than treating a failed read as read.
                        Err(error) => self.attempts.failed(&error),
                    }
                }

                // Unchanged: the object is what it was, which is a store
                // answering. Nothing to report and nothing to deliver.
                Ok(_) => {}

                // The check itself failed — an expired credential, a bucket
                // briefly unreachable. It does not end the watch, which is
                // what makes reporting it the only way anyone learns that
                // this loop has been polling into the void.
                Err(error) => self.attempts.failed(&error),
            }

            sleep_while(interval, watching).await;
        }

        Ok(())
    }

    /// Sorts one of the SDK's failures into a kind.
    ///
    /// The code, not the status: S3 answers `AccessDenied` and
    /// `RequestTimeTooSkewed` with the same 403, and only one of them is
    /// something waiting cannot cure.
    fn classified<E: ProvideErrorMetadata + std::fmt::Display>(&self, error: &E) -> Error {
        let described = format!("{}: {error}", self.describe());

        match error.code() {
            Some(code) if AUTH_CODES.contains(&code) => Error::auth(described),
            _ => Error::remote(described),
        }
    }

    /// The object's ETag, which changes when its body does.
    async fn etag(&self) -> Result<String, Error> {
        let key = self.single_key()?;

        let head = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| self.classified(&error))?;

        head.e_tag()
            .map(str::to_owned)
            .ok_or_else(|| Error::remote(format!("{}: the object has no ETag", self.describe())))
    }

    /// The one object a watch follows, and the ETag it was read at.
    async fn read(&self) -> Result<(Fetched, Option<String>), Error> {
        let format = self.format()?;
        let key = self.single_key()?;

        let (text, tag) = self.object(key).await?;

        Ok((Fetched::new(text, format), tag))
    }

    /// One object's body, and its ETag.
    async fn object(&self, key: &str) -> Result<(String, Option<String>), Error> {
        let object = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| self.classified(&error))?;

        let tag = object.e_tag().map(str::to_owned);

        let bytes = object
            .body
            .collect()
            .await
            .map_err(|error| Error::remote(format!("{}: `{key}`: {error}", self.describe())))?
            .into_bytes();

        let text = String::from_utf8(bytes.to_vec()).map_err(|error| {
            Error::remote(format!(
                "{}: `{key}` is not UTF-8: {error}",
                self.describe()
            ))
        })?;

        Ok((text, tag))
    }

    /// The `(key, document)` pairs this source reads, in merge order.
    ///
    /// Every key must answer: merging the four that did would leave a process
    /// running a configuration with a section quietly missing from it.
    async fn documents(&self) -> Result<Vec<(String, String)>, Error> {
        let keys = match &self.keys {
            Keys::One(key) => vec![key.clone()],
            Keys::Several(keys) => keys.clone(),
            Keys::Prefix(prefix) => self.listed(prefix).await?,
        };

        // A prefix that matched nothing is a missing configuration rather than
        // an empty one, and saying so here beats an empty merge's vaguer word.
        if keys.is_empty() {
            return Err(Error::remote(format!(
                "{}: nothing matched, so there is nothing to load",
                self.describe()
            )));
        }

        let mut documents = Vec::with_capacity(keys.len());

        for key in keys {
            let (text, _tag) = self.object(&key).await?;

            documents.push((key, text));
        }

        Ok(documents)
    }

    /// Every key under `prefix`, from `ListObjectsV2`.
    ///
    /// The budget is applied **to the listing**: a page is asked for one key
    /// more than the budget allows, so a prefix pointed at a whole bucket is
    /// refused after one request rather than after a million bodies. S3 lists
    /// in UTF-8 order and pages continue where the last left off, so the
    /// result is already sorted — which the prefix rule needs, because the
    /// same set of keys has to produce the same document and the same
    /// diagnostic every time.
    async fn listed(&self, prefix: &str) -> Result<Vec<String>, Error> {
        // One more than the budget, so the refusal happens on the count rather
        // than on a page boundary: a bucket holding exactly the budget is
        // allowed, and the first key past it is not.
        let per_page = i32::try_from(MOST_KEYS + 1).unwrap_or(i32::MAX);

        let mut found: Vec<String> = Vec::new();
        let mut token: Option<String> = None;

        for _ in 0..MOST_LIST_PAGES {
            let page = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .max_keys(per_page)
                .set_continuation_token(token)
                .send()
                .await
                .map_err(|error| self.classified(&error))?;

            for object in page.contents() {
                let Some(key) = object.key() else {
                    continue;
                };

                // The store is not trusted to have honoured the prefix it was
                // given: a proxy in front of it could rewrite the request, and
                // the check is one comparison.
                documents::under_prefix(key, prefix, &self.describe())?;

                // The zero-byte object the console creates when somebody makes
                // a "folder". It is not a missing document; it is not a
                // document.
                if key.ends_with('/') {
                    continue;
                }

                found.push(key.to_owned());
            }

            // Checked per page, so the refusal costs one listing rather than
            // every listing plus every body.
            documents::within_key_budget(found.len(), &self.describe())?;

            token = page.next_continuation_token().map(str::to_owned);

            if token.is_none() {
                return Ok(found);
            }
        }

        Err(Error::remote(format!(
            "{}: the listing did not finish in {MOST_LIST_PAGES} pages; \
             the store is not advancing the continuation token",
            self.describe()
        )))
    }
}

impl AsyncRemoteSource for S3 {
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<Fetched, Error>> + Send + '_>> {
        Box::pin(async move {
            let format = self.format()?;

            let documents = self.documents().await?;

            // A named list is read in call order and a listing arrives in
            // UTF-8 order, which is what each rule wants — so nothing is
            // reordered here.
            documents::merged(&documents, format, self.overlap(), &self.describe())
        })
    }

    fn describe(&self) -> String {
        // The endpoint tells MinIO apart from AWS, and one MinIO from
        // another — the detail that matters when an error says "no such
        // bucket" and there are three object stores it could mean.
        match &self.endpoint {
            Some(endpoint) => format!("s3 {endpoint} {}/{}", self.bucket, self.keys.describe()),
            None => format!("s3 {}/{}", self.bucket, self.keys.describe()),
        }
    }
}

impl std::fmt::Debug for S3 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3")
            .field("bucket", &self.bucket)
            .field("keys", &self.keys)
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
        // `min`, so an interval below the slice sleeps what was asked, not a
        // silently rounded-up quarter second.
        tokio::time::sleep(SLICE.min(total - slept)).await;
        slept += SLICE;
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use aws_sdk_s3::config::retry::RetryConfig;
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};

    use super::*;

    /// An S3 pointed at `endpoint`, with credentials that exist only here and
    /// the retry policy `retries` asks for.
    ///
    /// Static credentials rather than the environment's: a test that reads the
    /// ambient credential chain passes or fails according to whose laptop it
    /// is on. The retry policy is explicit for the same reason — the real
    /// construction paths inherit `aws-config`'s standard three attempts, and
    /// a test should say which number it is asserting about.
    fn against(endpoint: &str, retries: RetryConfig) -> S3 {
        let config = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .retry_config(retries)
            .credentials_provider(Credentials::for_tests())
            .build();

        S3::from_client(Client::from_conf(config), "myapp-config", "prod/db.json")
    }

    /// Answers every request with `status` and `body`, and counts them.
    fn scripted(
        status: &'static str,
        body: impl Into<String>,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::thread::JoinHandle<()>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let body = body.into();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = std::sync::Arc::new(AtomicUsize::new(0));

        let counter = std::sync::Arc::clone(&requests);
        let server = std::thread::spawn(move || {
            // Bounded: a refusal the SDK does not retry costs one request, and
            // the loop must not outlive the test either way. Above
            // `MOST_LIST_PAGES`, so the page cap is what ends a listing test
            // rather than the server running out of turns.
            for _ in 0..MOST_LIST_PAGES + 4 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };

                let mut seen = Vec::new();
                let mut byte = [0u8; 1];

                while !seen.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                    seen.push(byte[0]);
                }

                counter.fetch_add(1, Ordering::SeqCst);

                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/xml\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        (endpoint, requests, server)
    }

    /// A `ListObjectsV2` answer naming `keys`, and truncated if `token` says
    /// where to carry on from.
    fn listing(keys: &[String], token: Option<&str>) -> String {
        let contents: String = keys
            .iter()
            .map(|key| format!("<Contents><Key>{key}</Key><Size>1</Size></Contents>"))
            .collect();

        let truncation = match token {
            Some(token) => format!(
                "<IsTruncated>true</IsTruncated><NextContinuationToken>{token}</NextContinuationToken>"
            ),
            None => "<IsTruncated>false</IsTruncated>".to_owned(),
        };

        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>myapp-config</Name>{truncation}{contents}</ListBucketResult>"#
        )
    }

    /// An S3 reading `keys`, against `endpoint`, with no retries.
    fn reading(endpoint: &str, keys: Keys) -> S3 {
        let config = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .retry_config(RetryConfig::disabled())
            .credentials_provider(Credentials::for_tests())
            .build();

        S3::from_client(Client::from_conf(config), "myapp-config", keys).with_format(Format::Json)
    }

    /// The budget has to bite on the *listing*. A prefix pointed at a whole
    /// bucket must cost one request, not one request and half a million
    /// bodies — which is the difference between a refusal and an outage.
    #[tokio::test]
    async fn a_prefix_over_the_budget_is_refused_after_one_listing() {
        let keys: Vec<String> = (0..=MOST_KEYS)
            .map(|n| format!("prod/section-{n:04}.json"))
            .collect();

        let (endpoint, requests, server) = scripted("200 OK", listing(&keys, None));

        let error = reading(&endpoint, Keys::prefix("prod/"))
            .fetch()
            .await
            .expect_err("the prefix matches more keys than the budget allows");

        drop(server);

        assert!(error.to_string().contains("narrow the prefix"), "{error}");
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the budget is checked on the listing, so not one body is fetched"
        );
    }

    /// A store is not trusted to have honoured the prefix it was given: a
    /// proxy in front of it can rewrite a request, and a key from outside the
    /// prefix would put somebody else's document into this configuration.
    #[tokio::test]
    async fn a_key_the_store_answers_with_from_outside_the_prefix_is_refused() {
        let (endpoint, _requests, server) = scripted(
            "200 OK",
            listing(&["other-tenant/db.json".to_owned()], None),
        );

        let error = reading(&endpoint, Keys::prefix("prod/"))
            .fetch()
            .await
            .expect_err("that key is not under the prefix that was asked for");

        drop(server);

        assert!(
            error.to_string().contains("other-tenant/db.json"),
            "{error}"
        );
        assert!(
            error.to_string().contains("not under the prefix"),
            "{error}"
        );
    }

    /// A continuation token that never clears is a loop inside a fetch, and
    /// the budget on the keys cannot see it: the count never moves.
    #[tokio::test]
    async fn a_listing_that_never_finishes_is_given_up_on() {
        // Every page: no keys, and always another page to come.
        let (endpoint, requests, server) = scripted("200 OK", listing(&[], Some("always-more")));

        let error = reading(&endpoint, Keys::prefix("prod/"))
            .fetch()
            .await
            .expect_err("the token never clears");

        drop(server);

        assert!(
            error.to_string().contains("not advancing the continuation"),
            "{error}"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            MOST_LIST_PAGES,
            "the page budget is what ends it"
        );
    }

    /// A watch compares ETags, and a set of objects has none. Refused at
    /// `watch`, so it fails now rather than in six hours by never firing.
    #[tokio::test]
    async fn a_multi_key_source_refuses_to_be_watched_and_says_what_to_do_instead() {
        let source = reading("http://127.0.0.1:9", Keys::several(["a.json", "b.json"]));

        let watch = dynamic_config::RemoteWatch::new();
        let watching = watch.watching();

        let error = source
            .watch(&watching, Duration::from_millis(50), |_| Ok(()))
            .await
            .expect_err("a merged document has no one ETag");

        assert!(error.to_string().contains("several keys"), "{error}");
        assert!(
            error.to_string().contains("refresh_remote_async"),
            "{error}"
        );
    }

    /// Two keys naming two formats is the confusing failure worth catching by
    /// name: `server.toml` parsed as JSON is a syntax error about a file that
    /// has no syntax error in it.
    #[tokio::test]
    async fn keys_naming_two_formats_are_reported_rather_than_guessed() {
        let config = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url("http://127.0.0.1:9")
            .force_path_style(true)
            .retry_config(RetryConfig::disabled())
            .credentials_provider(Credentials::for_tests())
            .build();

        let source = S3::from_client(
            Client::from_conf(config),
            "myapp-config",
            Keys::several(["prod/db.json", "prod/server.toml"]),
        );

        let error = source.fetch().await.expect_err("two formats, one source");

        assert!(error.to_string().contains("prod/db.json"), "{error}");
        assert!(error.to_string().contains("prod/server.toml"), "{error}");
        assert!(error.to_string().contains("with_format"), "{error}");
    }

    /// The store saying no to the credentials, which no amount of waiting
    /// changes — so a watch loop should stop rather than back off.
    #[tokio::test]
    async fn access_denied_is_an_auth_failure() {
        let (endpoint, _requests, server) = scripted(
            "403 Forbidden",
            r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>"#,
        );

        let error = against(&endpoint, RetryConfig::disabled())
            .fetch()
            .await
            .expect_err("the store refused the credentials");

        drop(server);

        assert_eq!(error.kind(), dynamic_config::ErrorKind::Auth, "{error}");
        assert!(error.to_string().contains("prod/db.json"), "{error}");
    }

    /// The same 403, a different code, and the opposite verdict: a clock too
    /// far out of step does come right, so classifying it `Auth` would stop a
    /// watch loop that NTP was about to fix.
    #[tokio::test]
    async fn a_skewed_clock_shares_the_403_and_stays_remote() {
        let (endpoint, _requests, server) = scripted(
            "403 Forbidden",
            r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>RequestTimeTooSkewed</Code><Message>The difference between the request time and the current time is too large.</Message></Error>"#,
        );

        let error = against(&endpoint, RetryConfig::disabled())
            .fetch()
            .await
            .expect_err("the store refused the request");

        drop(server);

        assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote, "{error}");
    }

    /// A store that is simply not there is `Remote`, never `Auth`.
    #[tokio::test]
    async fn an_unreachable_store_is_remote_rather_than_auth() {
        // Port 9 is discard; nothing listens there.
        let error = against("http://127.0.0.1:9", RetryConfig::disabled())
            .with_timeout(Duration::from_millis(200))
            .fetch()
            .await
            .expect_err("nothing is listening");

        assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote, "{error}");
    }

    /// `with_timeout` is per *attempt*, so the README's arithmetic — a
    /// timeout times the attempt count — is a tested claim rather than a
    /// hopeful one.
    #[tokio::test]
    async fn the_deadline_is_per_attempt_and_the_sdk_retries_underneath() {
        // Accepts and never answers: the failure only a per-attempt deadline
        // catches, and one the SDK treats as worth retrying.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());

        let silent = std::thread::spawn(move || {
            let mut held = Vec::new();

            while held.len() < 3 {
                let Ok(accepted) = listener.accept() else {
                    return;
                };
                held.push(accepted);
            }

            std::thread::sleep(Duration::from_secs(1));
        });

        const ATTEMPT: Duration = Duration::from_millis(300);

        // Three attempts, the number `aws-config` gives the real
        // construction paths, with the backoff shortened so the test spends
        // its time on the attempts rather than between them.
        let source = against(
            &endpoint,
            RetryConfig::standard()
                .with_max_attempts(3)
                .with_initial_backoff(Duration::from_millis(1)),
        )
        .with_timeout(ATTEMPT);

        assert_eq!(
            source
                .client
                .config()
                .timeout_config()
                .and_then(aws_sdk_s3::config::timeout::TimeoutConfig::operation_attempt_timeout),
            Some(ATTEMPT),
            "the value has to reach the SDK, not merely be remembered here"
        );

        let started = std::time::Instant::now();
        let error = source.fetch().await.expect_err("nothing ever answers");
        let elapsed = started.elapsed();

        assert!(
            elapsed > ATTEMPT * 2,
            "the SDK retries beneath the per-attempt deadline, so the call \
             outlasts one attempt — that is the README's multiplier: {elapsed:?}"
        );
        assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote, "{error}");

        let _ = silent.join();
    }

    // -----------------------------------------------------------------------
    // Reporting a failing watch
    //
    // One `#[dynamic_config]` type per test: the snapshot, the remote slot and
    // the sink's generation all live in statics keyed by the type, so two
    // tests sharing one would race and — worse — pass alone.
    // -----------------------------------------------------------------------

    /// The failure nobody notices: a poll loop *survives* its failures by
    /// design, so a bucket that stopped answering on Tuesday looks exactly
    /// like a configuration nobody has changed since Tuesday.
    ///
    /// What the status must say afterwards is a *pair*: `reachable()` goes to
    /// `Some(false)` while `last_fetch` keeps the instant the last document
    /// really arrived — so an alert can ask "down, and stale for how long".
    /// A failure that reset the clock would hide the second half.
    #[tokio::test]
    async fn a_failing_poll_reports_the_store_as_down_and_leaves_the_clock_running() {
        use dynamic_config::dynamic_config;

        #[dynamic_config]
        #[derive(Debug, serde::Deserialize)]
        struct Polled {
            // Never read: this test is about the status the store records,
            // not about the document, which never gets as far as a snapshot.
            #[allow(dead_code)]
            host: String,
        }

        let (answering, _requests, answered) = scripted("200 OK", r#"{"db": {"host": "base"}}"#);

        Polled::set_remote_async(against(&answering, RetryConfig::disabled()));
        Polled::refresh_remote_async()
            .await
            .expect("the store answers the first read");

        // Taken after the source is installed, which is what fences it.
        let sink = Polled::remote_sink();
        let before = sink.status();

        assert_eq!(before.reachable(), Some(true), "one fetch, and it answered");
        assert!(before.last_fetch.is_some());

        // A second endpoint rather than a second mood: the store the watch
        // polls has started refusing, which is what an expired credential or
        // a gateway that went away looks like from inside the loop.
        let (refusing, _polls, refused) = scripted(
            "500 Internal Server Error",
            "<Error><Code>Internal</Code></Error>",
        );

        let watcher = against(&refusing, RetryConfig::disabled())
            .with_timeout(Duration::from_millis(500))
            .reporting_to(sink);

        let watch = dynamic_config::RemoteWatch::new();
        let watching = watch.watching();

        let polling = tokio::spawn(async move {
            watcher
                .watch(&watching, Duration::from_millis(50), |_| Ok(()))
                .await
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(20);

        while sink.status().consecutive_failures == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let after = sink.status();

        assert!(
            !polling.is_finished(),
            "a failed check does not end the watch — which is exactly why \
             reporting it is the only way anyone hears about it"
        );
        assert_eq!(
            after.reachable(),
            Some(false),
            "a loop polling into the void is a store that is down"
        );
        assert_eq!(
            after.last_fetch, before.last_fetch,
            "the staleness clock keeps running: `last_fetch` is when a document \
             last arrived, and a failed attempt is not one"
        );
        assert_eq!(
            after.fetches, before.fetches,
            "a failure is not a fetch, however it is counted elsewhere"
        );
        assert_eq!(
            after
                .last_failure
                .as_ref()
                .expect("a failure was recorded")
                .kind,
            dynamic_config::ErrorKind::Remote,
            "a store answering 500 may yet come back"
        );

        // The recorded failure is a kind and a path, and the bucket, the key
        // and the endpoint are in none of them.
        let recorded = format!("{:?}", after.last_failure);

        assert!(!recorded.contains("myapp-config"), "{recorded}");
        assert!(!recorded.contains("prod/db.json"), "{recorded}");

        watch.stop();
        let _ = polling.await;
        drop((answered, refused));
    }

    /// Answers every `HEAD` with an ETag — a new one after the first, so the
    /// object has changed — and every `GET` with a refusal.
    ///
    /// The shape of a bucket whose read policy went away, or of an object
    /// large enough that the transfer is the thing failing: the check the loop
    /// makes every tick keeps working, and the read it exists to make does
    /// not. Nothing about that reaches the caller, so nothing about it reaches
    /// anyone.
    fn heads_but_refuses_the_body() -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());

        let server = std::thread::spawn(move || {
            let mut heads = 0;

            // Bounded, so the thread cannot outlive the test whatever the
            // loop does.
            for _ in 0..64 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };

                let mut seen = Vec::new();
                let mut byte = [0u8; 1];

                while !seen.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                    seen.push(byte[0]);
                }

                let response = if String::from_utf8_lossy(&seen).starts_with("HEAD") {
                    heads += 1;

                    // The first tick records the tag without reading. The
                    // second is the change this loop exists for.
                    let tag = if heads > 1 { "second" } else { "first" };

                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nETag: \"{tag}\"\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    let body = "<Error><Code>Internal</Code></Error>";

                    format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };

                if stream.write_all(response.as_bytes()).is_err() {
                    return;
                }
            }
        });

        (endpoint, server)
    }

    /// The second failure site, and the easier one to miss: the ETag moved, so
    /// the object *did* change, and the read that would have delivered it
    /// failed. The loop swallows that by design — it is the same `if let Ok`
    /// that makes a network blip survivable — so without a report the store
    /// looks healthy while the configuration it serves is frozen.
    #[tokio::test]
    async fn a_read_that_fails_after_the_tag_moved_is_reported_too() {
        use dynamic_config::dynamic_config;

        #[dynamic_config]
        #[derive(Debug, serde::Deserialize)]
        struct Torn {
            // Never read, for the reason the test above gives.
            #[allow(dead_code)]
            host: String,
        }

        let sink = Torn::remote_sink();
        let (endpoint, server) = heads_but_refuses_the_body();

        let watcher = against(&endpoint, RetryConfig::disabled())
            .with_timeout(Duration::from_millis(500))
            .reporting_to(sink);

        let watch = dynamic_config::RemoteWatch::new();
        let watching = watch.watching();

        let polling = tokio::spawn(async move {
            watcher
                .watch(&watching, Duration::from_millis(50), |_| Ok(()))
                .await
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(20);

        while sink.status().consecutive_failures == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
            !polling.is_finished(),
            "a failed read does not end the watch either"
        );
        assert_eq!(
            sink.status().reachable(),
            Some(false),
            "the store answered the check and not the read, which is a store \
             this loop cannot get a document out of"
        );
        assert_eq!(
            sink.status().fetches,
            0,
            "nothing was delivered, so nothing is counted as a fetch"
        );

        watch.stop();
        let _ = polling.await;
        // Dropped rather than joined: the server is parked in `accept` and no
        // connection is coming, so joining it would hang the suite.
        drop(server);
    }

    /// The line the other way: a watch refused at the door is returned to the
    /// caller standing there, and those refusals are deployment mistakes
    /// rather than a store that stopped answering. Charging them to
    /// `dynamic_config_remote_up` would page somebody about S3 for a source
    /// that names several keys.
    #[tokio::test]
    async fn a_watch_refused_at_the_door_is_not_a_store_that_stopped_answering() {
        use dynamic_config::dynamic_config;

        #[dynamic_config]
        #[derive(Debug, serde::Deserialize)]
        struct Doorstep {
            // Never read, for the reason the test above gives.
            #[allow(dead_code)]
            host: String,
        }

        // No source is installed: a sink does not need one, and this is the
        // state a program that only ever watches is in.
        let sink = Doorstep::remote_sink();

        let source =
            reading("http://127.0.0.1:9", Keys::several(["a.json", "b.json"])).reporting_to(sink);

        let watch = dynamic_config::RemoteWatch::new();
        let error = source
            .watch(&watch.watching(), Duration::from_millis(50), |_| Ok(()))
            .await
            .expect_err("an ETag belongs to an object, and a set has none");

        assert!(error.to_string().contains("cannot be watched"), "{error}");
        assert_eq!(
            sink.status().reachable(),
            None,
            "nothing has been asked of this store, so it is neither up nor down"
        );
    }

    // -----------------------------------------------------------------------
    // TLS: the shared vocabulary, and the half the AWS SDK cannot express.
    // -----------------------------------------------------------------------

    /// A certificate authority, generated here. A committed fixture expires,
    /// and a suite that fails on a date nobody chose is worse than one that
    /// costs a millisecond.
    fn authority() -> String {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);

        params.self_signed(&key).unwrap().pem()
    }

    /// A trust store is what the SDK's TLS context has, so a certificate
    /// authority goes through — and construction still reaches no network.
    #[tokio::test]
    async fn a_private_authority_builds_a_client_and_touches_nothing() {
        let config = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .endpoint_url("https://minio.internal:9000")
            .build();

        let source = S3::with_tls(
            &config,
            "myapp-config",
            "prod/db.json",
            &TlsConfig::new().with_ca_certificate_pem(authority()),
        )
        .expect("a trust store is what the SDK's TLS context holds");

        assert!(
            source.describe().contains("minio.internal"),
            "the endpoint tells MinIO apart from AWS in an error: {}",
            source.describe()
        );
    }

    /// The SDK's connector calls `.expect("cert parsable")` on this material,
    /// so a certificate it cannot read would be a panic at the first
    /// connection — a long way from the call that supplied it. Refused at
    /// construction instead, and without quoting what it choked on.
    #[tokio::test]
    async fn a_ca_certificate_the_sdk_would_panic_on_is_refused_at_construction() {
        let config = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .build();

        let error = S3::with_tls(
            &config,
            "myapp-config",
            "prod/db.json",
            &TlsConfig::new().with_ca_certificate_pem(
                "-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n",
            ),
        )
        .expect_err("the SDK would have panicked on this");

        assert!(error.to_string().contains("not PEM-encoded"), "{error}");
    }

    /// The one thing S3 cannot express. Refused rather than ignored: a caller
    /// who asked to present a certificate and did not would discover it as an
    /// authentication failure a long way from the cause.
    #[tokio::test]
    async fn a_client_certificate_is_refused_and_points_at_the_escape_hatch() {
        let config = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .build();

        let error = S3::with_tls(
            &config,
            "myapp-config",
            "prod/db.json",
            &TlsConfig::new().with_client_certificate_files("/etc/ssl/app.crt", "/etc/ssl/app.key"),
        )
        .expect_err("the SDK's TLS context has no client-certificate slot");

        assert!(error.to_string().contains("client certificate"), "{error}");
        assert!(error.to_string().contains("from_client"), "{error}");
        assert!(
            error.to_string().contains("refused rather than ignored"),
            "{error}"
        );
    }

    /// The private key is the sharpest secret here even where it is refused:
    /// the refusal must name the call and not the material.
    #[tokio::test]
    async fn the_client_certificate_refusal_never_quotes_the_key() {
        const PLANTED: &str = "PLANTED-PRIVATE-KEY-MATERIAL";

        let config = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .build();

        let error = S3::with_tls(
            &config,
            "myapp-config",
            "prod/db.json",
            &TlsConfig::new().with_client_certificate_pem("cert", PLANTED),
        )
        .expect_err("the SDK's TLS context has no client-certificate slot");

        assert!(!error.to_string().contains(PLANTED), "{error}");
        assert!(!format!("{error:?}").contains(PLANTED), "{error:?}");
    }

    /// A CA file that is not there names the path, from the constructor
    /// rather than from a panic in a builder chain.
    #[tokio::test]
    async fn a_missing_ca_file_names_the_path_and_the_material() {
        let config = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .build();

        let error = S3::with_tls(
            &config,
            "myapp-config",
            "prod/db.json",
            &TlsConfig::new().with_ca_certificate_file("/nonexistent/private-ca.pem"),
        )
        .expect_err("the CA file is not there");

        assert!(
            error.to_string().contains("/nonexistent/private-ca.pem"),
            "{error}"
        );
        assert!(error.to_string().contains("the CA certificate"), "{error}");
    }
}
