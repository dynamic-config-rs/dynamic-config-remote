//! Read [`dynamic-config`] configuration from a NATS JetStream key/value bucket.
//!
//! NATS is a streaming protocol and its client is async throughout, so this
//! implements the **async** [`AsyncRemoteSource`] trait rather than the
//! blocking one.
//!
//! ```no_run
//! use dynamic_config_nats::Nats;
//!
//! # struct DbConfig;
//! # impl DbConfig {
//! #     fn set_remote_async(_: Nats) {}
//! #     async fn refresh_remote_async() -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! DbConfig::set_remote_async(
//!     Nats::new("nats://nats.internal:4222", "config", "db.json").await?,
//! );
//!
//! // Fetching is explicit; the load that follows touches no network.
//! DbConfig::refresh_remote_async().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # What it reads
//!
//! One key in one bucket, whose value is **a whole configuration document** —
//! the same bytes that would be in a config file. The format comes from the
//! key's extension, or from [`with_format`](Nats::with_format).
//!
//! Like Consul and unlike Vault, that is a deliberate difference: a KV bucket
//! stores opaque bytes, so the natural unit is the document. Vault's KV v2
//! stores a JSON object of fields, so the natural unit there is the field.
//!
//! # Several keys as one document
//!
//! A deployment that splits its configuration across several keys of one
//! bucket can have one source read the lot, and [`Keys`] says which:
//!
//! ```no_run
//! # use dynamic_config_nats::{Keys, Nats};
//! # async fn example() -> Result<(), dynamic_config::Error> {
//! // Named keys: a list of layers, merged in the order given, later wins.
//! let nats = Nats::new(
//!     "nats://nats.internal:4222",
//!     "config",
//!     Keys::several(["base.json", "local.json"]),
//! )
//! .await?;
//! # Ok(())
//! # }
//! ```
//!
//! **A named list is one get per key**, and a bucket read is a request to the
//! stream: there is no batch get in the KV API, so the set is **not** read
//! atomically. A write landing between two of the gets can produce a document
//! that never existed as a whole.
//!
//! **There is deliberately no prefix form**, and the reason is the client's
//! rather than a preference. `Store::keys()` is the only listing there is, and
//! it lists the **whole bucket**: it builds an ordered consumer filtered on
//! `$KV.{bucket}.>` and streams a header for every key in it. `async-nats`
//! keeps the filtered constructor behind a private method, so a prefix here
//! would be a full-bucket scan wearing a prefix's name — the 512-key bound
//! would have to be a bound on the bucket, and a bucket of a hundred thousand
//! keys would stream a hundred thousand headers to find three. Name the keys,
//! or put the set in its own bucket, which is the partition NATS actually
//! offers. [`dynamic-config-consul`] and [`dynamic-config-etcd`] have a real
//! range read and take a prefix for that reason.
//!
//! Two consequences the multi-key form shares with the rest of the family:
//!
//! - **Provenance becomes store-grained.** The merged document is one layer,
//!   so `source_of` names the set rather than which key supplied a value.
//! - **One unreadable key fails the whole fetch.** A configuration quietly
//!   missing a section is worse than a refresh that failed and left the last
//!   document serving.
//!
//! # JetStream must be enabled
//!
//! A key/value bucket is a JetStream feature. A NATS server started without it
//! answers with a "JetStream is not enabled" error, which is reported as it
//! arrives rather than translated into something vaguer.
//!
//! # The connection is made once
//!
//! [`Nats::new`] connects and resolves the bucket; [`fetch`](AsyncRemoteSource::fetch)
//! reuses that handle. Unlike a gRPC client this connects eagerly, so an
//! unreachable server *is* a construction failure.
//!
//! The store handle is `Clone` and its reads take `&self`, so — unlike etcd —
//! nothing here needs a lock.
//!
//! # Reconnecting is the client's job, and it does it
//!
//! `async-nats` reconnects on its own, indefinitely, and re-establishes
//! subscriptions when it does. So there is deliberately no retry logic here:
//! adding one would mean a second, worse implementation of something the client
//! already does properly, layered on top of it.
//!
//! Two consequences worth knowing. A [`fetch`](AsyncRemoteSource::fetch) during
//! a disconnect fails rather than blocking until the connection returns —
//! configuration that hangs is worse than configuration that reports. And a
//! [`watch`](Nats::watch) survives a reconnect without the caller noticing,
//! which is why it ending at all is treated as an error.
//!
//! # Credentials
//!
//! Everything NATS understands — a token, a user and password, an NKey, a JWT,
//! a `.creds` file, TLS — goes through [`ConnectOptions`], which is NATS' own
//! type re-exported. See [`Nats::with_options`].
//!
//! A credential the server refuses fails at *construction*, and reports as
//! `ErrorKind::Auth` rather than `Remote` — the one distinction that separates
//! "the password is wrong" from "the server is down", and the only place
//! `async-nats` draws it. A later read refused for want of permission arrives
//! as an undifferentiated KV error, so it stays `Remote`: guessing there would
//! stop a watch loop that a reconnect would have fixed.
//!
//! A credential in the *URL* — `nats://token@host:4222` is a shape NATS
//! accepts — is redacted before the address is stored, because the address is
//! quoted into every error message and into `Debug`.
//!
//! # Timeouts
//!
//! [`Nats::with_timeout`] is the deadline for a single fetch attempt,
//! excluding retries the underlying client performs — the sentence every store
//! in this family answers to. Ten seconds by default.
//!
//! `ConnectOptions::request_timeout` is its twin on the connection side, set
//! through [`Nats::with_options`] before there is a connection to bound.
//! Neither applies to [`Nats::watch`], which is long-lived on purpose.
//!
//! # Watching
//!
//! A KV bucket is a stream, so [`Nats::watch`] is a future the caller spawns and
//! cancels by dropping — no runtime is imposed and no flag is polled.
//!
//! A **multi-key source cannot be watched**, and refuses rather than pretending
//! to: what a watch delivers here is the document that changed, and for a
//! merged document that means re-reading the whole set on every event. Poll
//! `refresh_remote_async()` on a timer instead.
//!
//! ```no_run
//! # use dynamic_config_nats::Nats;
//! # async fn example(nats: Nats) {
//! # let sink = |_: dynamic_config::Fetched| -> Result<(), dynamic_config::Error> { Ok(()) };
//! let task = tokio::spawn(async move {
//!     nats.watch(move |document| sink(document)).await
//! });
//!
//! // Dropping or aborting the task stops the watch.
//! task.abort();
//! # }
//! ```
//!
//! # A watch that is failing says so
//!
//! A watch is the half of a store `dynamic-config` cannot see: a delivery keeps
//! `RemoteStatus` current, and a stream that broke delivers nothing and would
//! otherwise report nothing — so `dynamic_config_remote_up` would describe the
//! last *delivery* rather than the last *attempt*.
//! [`reporting_to`](Nats::reporting_to) closes that: the sink the loop already
//! holds is told about every attempt that came back with nothing, and a store
//! that stopped answering an hour ago reads as down without anything having to
//! call `refresh_remote_async()`.
//!
//! What that covers here follows from the section above: a *server* that goes
//! away is not a failed watch, because `async-nats` keeps recreating the
//! subscription for as long as it takes and the loop waits through it. What
//! reaches this crate is a stream that stopped — a deleted bucket, a consumer
//! that is gone, a value that is not a document — and that is what is reported.
//!
//!
//! # Every failure branch of the watch loop, and what it reports
//!
//! A watch is the half of a store `dynamic-config` cannot see, and
//! [`reporting_to`](Nats::reporting_to) is what lets it speak: the sink the
//! loop already holds is told about every attempt that came back with
//! nothing. Which attempts those are is a table rather than prose, because
//! the question an operator asks is *which* silence is deliberate.
//!
//! Three rules decide the column, and they are the same three in all seven
//! store crates:
//!
//! 1. **A failure the loop survives by retrying reports.** That is the case
//!    the whole feature exists for: the stream is down, the last delivery is
//!    old, and nothing else would ever say so out loud.
//! 2. **A recovery that worked stays silent.** Only a delivery or a fetch
//!    clears the streak, so reporting a five-minute token turning over on a
//!    healthy cluster would drive `remote_up` to zero and leave it there.
//! 3. **A refusal that never asked the store reports nowhere.** No format, a
//!    key shape that cannot be watched, material that will not build a
//!    client: `RemoteStatus::reachable()` is *whether the store answered the
//!    last time it was asked*, and these never ask. They are returned to the
//!    caller, who is the one holding the mistake — and a status cannot
//!    correct them, since it carries a kind and a path and no message.
//!
//! | Branch | Reports |
//! |---|---|
//! | the format is missing, or the source names several keys | no — rule 3: nothing has been asked of the server |
//! | the bucket refuses the watch | **yes** — the first round trip |
//! | the stream errors | **yes**, and the watch ends — `async-nats` reconnects on its own, so reaching here means it could not |
//! | an operation that is not a `Put` | no — nothing changed |
//! | the value is not UTF-8 | **yes** — the same failure a `fetch` of it would have recorded |
//! | `on_change` refuses the document | no — the store answered; `apply` counted the delivery, and what the document did next is `ConfigStatus`'s half |
//! | the stream ends without an error | **yes** — the connection went away, or the bucket did |
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config
//! [`dynamic-config-consul`]: https://docs.rs/dynamic-config-consul
//! [`dynamic-config-etcd`]: https://docs.rs/dynamic-config-etcd

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use async_nats::jetstream::kv::{Operation, Store};
use dynamic_config::{AsyncRemoteSource, Error, Fetched, Format, RemoteSink};
use dynamic_config_store_core::attempts::Attempts;
use dynamic_config_store_core::documents::{self, Overlap};
use dynamic_config_store_core::{guarded, LoneAuthority};

/// NATS' own connection options, re-exported so authenticating needs no direct
/// dependency on `async-nats`.
///
/// Every credential NATS understands lives here: a token, a user and password,
/// an NKey, a JWT, a `.creds` file, TLS. There is no second vocabulary to learn,
/// and options this crate has never heard of keep working.
pub use async_nats::{Client, ConnectOptions};
use futures_util::StreamExt;

use dynamic_config_store_core::tls as tls_core;
/// A private certificate authority and a client certificate, as data.
///
/// The shared vocabulary all seven store crates take, so that reaching TLS
/// never means naming an `async-nats` type — see [`Nats::with_tls`]. NATS is
/// the one store here that cannot express the whole of it: its client takes
/// **file paths**, so the PEM-bytes spellings are refused rather than written
/// to a temporary file.
pub use dynamic_config_store_core::tls::TlsConfig;

/// How long one fetch may take before it is given up on.
///
/// Ten seconds, matching the rest of the family. A configuration fetch that
/// hangs is worse than one that fails: the caller can retry a failure.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a source reads: one key, or several named ones.
///
/// Every constructor takes one, and a bare `&str` or `String` is
/// [`Keys::one`] — so the single-key spelling every caller already wrote keeps
/// working unchanged.
///
/// There is no prefix variant, and that is the client's doing rather than a
/// preference: the only listing `async-nats` exposes walks the whole bucket.
/// The crate documentation says the whole of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Keys {
    /// One key, whose value is the whole document.
    One(String),
    /// Several named keys, merged **in the order given — later wins**.
    ///
    /// The rule a list of `.file(..)` calls already teaches: the caller wrote
    /// the list, so the list is the precedence. One get per key, because the
    /// KV API has no batch read — so the set is **not** read atomically.
    Several(Vec<String>),
}

impl Keys {
    /// One key, whose value is the whole document.
    #[must_use]
    pub fn one(key: impl Into<String>) -> Self {
        Self::One(key.into())
    }

    /// Several named keys, merged in the order given — later wins.
    #[must_use]
    pub fn several<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Several(keys.into_iter().map(Into::into).collect())
    }

    /// The keys as a slice, in the order they are read.
    fn named(&self) -> &[String] {
        match self {
            Self::One(key) => std::slice::from_ref(key),
            Self::Several(keys) => keys,
        }
    }

    /// How a diagnostic names what this source reads.
    ///
    /// One key renders as `key {name}`, so every message a single-key source
    /// has ever produced is unchanged.
    fn describe(&self) -> String {
        match self {
            Self::One(key) => format!("key {key}"),
            Self::Several(keys) => format!("keys {}", keys.join(", ")),
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

/// A key in a JetStream bucket, as a configuration source.
pub struct Nats {
    store: Store,
    keys: Keys,
    format: Option<Format>,
    /// Why the keys' own extensions could not settle the format between them.
    ///
    /// Kept rather than reported at construction because the constructors
    /// report only what they reached, and because `with_format` is allowed to
    /// settle it afterwards.
    disagreement: Option<String>,
    server: String,
    bucket: String,
    timeout: Duration,
    /// Where the watch loop reports an attempt that came back with nothing.
    ///
    /// Nobody, unless [`reporting_to`](Nats::reporting_to) said otherwise —
    /// which is what makes reporting free for a caller who never asked for it.
    attempts: Attempts,
}

impl Nats {
    /// Connects to `server` and resolves `key` in `bucket`.
    ///
    /// The format is taken from the key's extension — `db.json` is JSON. A key
    /// without one needs [`with_format`](Self::with_format).
    ///
    /// # Errors
    ///
    /// If the server cannot be reached, if JetStream is not enabled, or if the
    /// bucket does not exist. This crate deliberately does not create the
    /// bucket: a configuration reader that provisions storage would hide a
    /// misconfigured deployment behind an empty one.
    pub async fn new(
        server: impl Into<String>,
        bucket: impl Into<String>,
        key: impl Into<Keys>,
    ) -> Result<Self, Error> {
        Self::with_options(server, bucket, key, ConnectOptions::new()).await
    }

    /// As [`new`](Self::new), with NATS' own connection options.
    ///
    /// This is where credentials live, because that is where `async-nats` puts
    /// them:
    ///
    /// ```no_run
    /// # use dynamic_config_nats::{ConnectOptions, Nats};
    /// # async fn example() -> Result<(), dynamic_config::Error> {
    /// // A `.creds` file, which is how a NATS account usually authenticates.
    /// let nats = Nats::with_options(
    ///     "nats://nats.internal:4222",
    ///     "config",
    ///     "db.json",
    ///     ConnectOptions::with_credentials_file("/etc/myapp/nats.creds")
    ///         .await
    ///         .map_err(|error| dynamic_config::Error::remote(error.to_string()))?,
    /// )
    /// .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Token, user and password, NKey, JWT and TLS all live on the same type.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub async fn with_options(
        server: impl Into<String>,
        bucket: impl Into<String>,
        key: impl Into<Keys>,
        options: ConnectOptions,
    ) -> Result<Self, Error> {
        let server = server.into();
        let bucket = bucket.into();
        let keys = key.into();

        // Everything a person or a log ever sees is the redacted form. A NATS
        // URL may carry a token or a password in its authority, and `server`
        // is quoted by `describe()` — which means by every error message and
        // by `Debug`.
        let described = redacted(&server);

        let client = options.connect(&server).await.map_err(|error| {
            let described = format!("nats {described}: {error}");

            // The one place `async-nats` names an auth failure as such: a
            // signed nonce the server would not take, or an outright
            // authorization violation. Both survive any amount of retrying,
            // which is exactly what `Auth` tells a caller.
            match error.kind() {
                async_nats::ConnectErrorKind::Authentication
                | async_nats::ConnectErrorKind::AuthorizationViolation => Error::auth(described),
                _ => Error::remote(described),
            }
        })?;

        let store = async_nats::jetstream::new(client)
            .get_key_value(&bucket)
            .await
            .map_err(|error| Error::remote(format!("nats {described} bucket {bucket}: {error}")))?;

        let (format, disagreement) = agreed(&keys);

        Ok(Self {
            store,
            keys,
            format,
            disagreement,
            server: described,
            bucket,
            timeout: DEFAULT_TIMEOUT,
            attempts: Attempts::default(),
        })
    }

    /// As [`with_options`](Self::with_options), with a private certificate
    /// authority or a client certificate from the shared vocabulary.
    ///
    /// The same three settings, spelled the same way, in all seven store
    /// crates — and spelled as *data*, so nothing here names an `async-nats`
    /// type:
    ///
    /// ```no_run
    /// # use dynamic_config_nats::{ConnectOptions, Nats, TlsConfig};
    /// # async fn example() -> Result<(), dynamic_config::Error> {
    /// let nats = Nats::with_tls(
    ///     "tls://nats.internal:4222",
    ///     "config",
    ///     "db.json",
    ///     ConnectOptions::new(),
    ///     &TlsConfig::new().with_ca_certificate_file("/etc/nats/ca.pem"),
    /// )
    /// .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # What NATS cannot express
    ///
    /// **PEM bytes.** `async-nats` takes paths and opens the files itself;
    /// there is no byte-taking door short of handing it a whole
    /// `rustls::ClientConfig`, which would put a direct `rustls` dependency
    /// and a crypto-provider decision in this crate for one spelling. So
    /// [`with_ca_certificate_pem`] and [`with_client_certificate_pem`] are
    /// **refused here**, naming the call and pointing at the file spelling —
    /// not ignored, because a caller who supplied a CA and got the public
    /// trust store has a program that believes it is pinned and is not. The
    /// obvious workaround, writing the bytes to a temporary file, is
    /// deliberately not taken: it would put a private key on a disk that never
    /// asked for one.
    ///
    /// Everything else is there: a CA file and a client certificate and key,
    /// which is what a NATS deployment with `tls` in its configuration file
    /// hands out.
    ///
    /// **Naming a CA turns TLS on.** `require_tls(true)` is set, so a
    /// `nats://` URL that would have negotiated plaintext fails instead of
    /// quietly connecting without the authority the caller just named.
    ///
    /// `options` carries everything that is not TLS: the token, the NKey, the
    /// `.creds` file. **The `tls` argument owns the TLS slot**; if `options`
    /// also names root certificates, both sets are added, because that is what
    /// `async-nats` does with them.
    ///
    /// There is no way to turn verification off; [`TlsConfig`]'s own
    /// documentation argues that one.
    ///
    /// # Errors
    ///
    /// If the configuration names PEM bytes, or as [`new`](Self::new).
    ///
    /// [`with_ca_certificate_pem`]: TlsConfig::with_ca_certificate_pem
    /// [`with_client_certificate_pem`]: TlsConfig::with_client_certificate_pem
    pub async fn with_tls(
        server: impl Into<String>,
        bucket: impl Into<String>,
        key: impl Into<Keys>,
        options: ConnectOptions,
        tls: &TlsConfig,
    ) -> Result<Self, Error> {
        let server = server.into();
        let described = format!("nats {}", redacted(&server));

        let options = with_tls_options(options, tls, &described)?;

        Self::with_options(server, bucket, key, options).await
    }

    /// Uses a client the program already has.
    ///
    /// For a caller already connected to NATS: reusing the connection beats
    /// opening a second one to the same server, and the client is `Clone` —
    /// cheaply, it is a handle — so sharing costs nothing.
    ///
    /// ```no_run
    /// # use dynamic_config_nats::{Client, Nats};
    /// # async fn example(client: Client) -> Result<(), dynamic_config::Error> {
    /// let nats = Nats::from_client(client, "config", "db.json").await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// If JetStream is not enabled, or the bucket does not exist.
    pub async fn from_client(
        client: Client,
        bucket: impl Into<String>,
        key: impl Into<Keys>,
    ) -> Result<Self, Error> {
        let bucket = bucket.into();

        let store = async_nats::jetstream::new(client)
            .get_key_value(&bucket)
            .await
            .map_err(|error| Error::remote(format!("nats bucket {bucket}: {error}")))?;

        Ok(Self::from_store(store, key))
    }

    /// Uses an already-resolved bucket.
    ///
    /// One step further than [`from_client`](Self::from_client), for a program
    /// that already holds the `Store` itself.
    #[must_use]
    pub fn from_store(store: Store, key: impl Into<Keys>) -> Self {
        let keys = key.into();
        let bucket = store.name.clone();

        let (format, disagreement) = agreed(&keys);

        Self {
            store,
            keys,
            format,
            disagreement,
            // The store does not carry the address it was reached through, and
            // inventing one would put a wrong server in every error message.
            server: "<an existing connection>".to_owned(),
            bucket,
            timeout: DEFAULT_TIMEOUT,
            attempts: Attempts::default(),
        }
    }

    /// Reports the watch loop's failed attempts to `sink`.
    ///
    /// Without this a watch is the half of a store `dynamic-config` cannot
    /// see. [`RemoteSink::apply`] records a delivery, so a *working* watch
    /// keeps the status current — but a loop whose stream broke or whose
    /// bucket went away delivers nothing, and so says nothing:
    /// `dynamic_config_remote_up` reports the last delivery rather than the
    /// last attempt, and a store that stopped answering an hour ago looks
    /// healthy until something calls `refresh_remote_async()`.
    ///
    /// ```no_run
    /// # use dynamic_config_nats::Nats;
    /// # struct DbConfig;
    /// # impl DbConfig {
    /// #     fn remote_sink() -> dynamic_config::RemoteSink { unimplemented!() }
    /// # }
    /// # async fn example(nats: Nats) -> Result<(), dynamic_config::Error> {
    /// let sink = DbConfig::remote_sink();
    ///
    /// // The same sink delivers and reports: one generation, one fence.
    /// nats.reporting_to(sink)
    ///     .watch(move |document| sink.apply(document))
    ///     .await
    /// # }
    /// ```
    ///
    /// A sink is `Copy` and captures its source's generation when it is taken,
    /// which is what keeps a loop winding down after its source was replaced
    /// from charging its failures to the replacement — so take it once, where
    /// the watch is wired, exactly as the delivering half already does.
    ///
    /// **Only the watch.** A [`fetch`](AsyncRemoteSource::fetch) records itself
    /// through `refresh_remote_async()` already, and what is reported here is
    /// the failure streak and the last failure and nothing else: the staleness
    /// clock keeps ageing while `remote_up` goes to zero, which is the pair an
    /// alert wants.
    ///
    /// The error's kind is what travels. Nothing that names this store — no
    /// server, no bucket, no key, and certainly not a token in a `nats://`
    /// URL — enters a `RemoteStatus`.
    #[must_use]
    pub fn reporting_to(mut self, sink: RemoteSink) -> Self {
        self.attempts = Attempts::to(sink);
        self
    }

    /// Reports `error` to whatever asked to hear about failed attempts, and
    /// hands it straight back.
    ///
    /// Every failure the watch loop ends on goes through here, so reporting is
    /// one word at each site rather than a branch that can be left out of the
    /// next one. It cannot fail and it does not touch the error: a loop must
    /// never have to handle a failure to report a failure, and the caller sees
    /// exactly what it always saw.
    fn failing(&self, error: Error) -> Error {
        self.attempts.failed(&error);

        error
    }

    /// States the format, for a key whose name does not.
    ///
    /// It also settles a list whose keys name two different formats.
    #[must_use]
    pub fn with_format(mut self, format: Format) -> Self {
        self.format = Some(format);
        // The caller has now said which format wins, so the keys no longer
        // have to agree between themselves.
        self.disagreement = None;
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
    /// A watch delivers *the document that changed*; for a merged document
    /// that means re-reading the whole set on every event, which is a
    /// different loop with different failure modes and belongs behind its own
    /// decision rather than behind this one.
    fn single_key(&self) -> Result<&str, Error> {
        match &self.keys {
            Keys::One(key) => Ok(key),
            Keys::Several(_) => Err(Error::remote(format!(
                "{}: a source that reads several keys cannot be watched; \
                 poll `refresh_remote_async()` on a timer instead",
                self.describe()
            ))),
        }
    }

    /// What two of this source's keys supplying one path means.
    ///
    /// Only [`Overlap::LaterWins`] here: a caller who wrote the list wrote the
    /// precedence with it, and there is no prefix form whose order nobody
    /// chose.
    fn overlap(&self) -> Overlap {
        Overlap::LaterWins
    }

    /// The `(key, document)` pairs this source reads, in merge order.
    ///
    /// One get per key and **every one of them must answer**: merging the four
    /// that did would leave a process running a configuration with a section
    /// quietly missing from it.
    async fn documents(&self) -> Result<Vec<(String, String)>, Error> {
        let keys = self.keys.named();
        let mut documents = Vec::with_capacity(keys.len());

        for key in keys {
            documents.push((key.clone(), self.read(key).await?));
        }

        Ok(documents)
    }

    /// One key's value, as text.
    async fn read(&self, key: &str) -> Result<String, Error> {
        let read = tokio::time::timeout(self.timeout, self.store.get(key))
            .await
            .map_err(|_| {
                Error::remote(format!(
                    "{}: `{key}` timed out after {:?}",
                    self.describe(),
                    self.timeout
                ))
            })?;

        let value = read
            .map_err(|error| Error::remote(format!("{}: {error}", self.describe())))?
            .ok_or_else(|| Error::remote(format!("{}: `{key}` holds no value", self.describe())))?;

        String::from_utf8(value.to_vec()).map_err(|error| {
            Error::remote(format!(
                "{}: `{key}` is not UTF-8: {error}",
                self.describe()
            ))
        })
    }

    /// How long a single fetch may take before it is given up on. Ten seconds
    /// by default.
    ///
    /// The deadline for **one fetch attempt**, excluding retries the
    /// underlying client performs — the same sentence every store in this
    /// family answers to. `async-nats` reconnects on its own, so a fetch
    /// crossing a reconnect is exactly the case this bounds.
    ///
    /// It bounds **each get**, so a source reading several keys reads each of
    /// them under this deadline rather than sharing one between them: the KV
    /// API has no batch read, and a deadline divided by however many keys a
    /// caller listed would be a different promise per source.
    ///
    /// It is the *second* of two timeouts, and they cover different halves.
    /// `ConnectOptions::request_timeout`, passed to
    /// [`with_options`](Self::with_options), bounds the client's own requests
    /// and is set before there is a connection to bound. This one wraps the
    /// KV read, so a server that accepted the request and then went quiet
    /// still ends the fetch rather than parking it.
    ///
    /// It does not cover [`watch`](Self::watch), which is long-lived by
    /// definition.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Calls `on_change` every time the key's value moves, forever.
    ///
    /// The first call happens when the *first change* arrives, not at startup:
    /// a watch reports changes, and reporting the current value as one would
    /// make every restart look like an edit. Fetch first if the starting value
    /// matters, which it usually does:
    ///
    /// ```no_run
    /// # use dynamic_config::AsyncRemoteSource;
    /// # use dynamic_config_nats::Nats;
    /// # struct Sink;
    /// # impl Sink {
    /// #     fn apply(&self, _: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
    /// # }
    /// # async fn example(nats: Nats) -> Result<(), dynamic_config::Error> {
    /// # let sink = Sink;
    /// sink.apply(nats.fetch().await?)?;
    /// nats.watch(move |document| sink.apply(document)).await
    /// # }
    /// ```
    ///
    /// **Cancellation is dropping the future.** There is no stop flag, because
    /// there is nothing to poll one between: this suspends on the stream, so
    /// any executor's cancellation already ends it immediately.
    ///
    /// Deletes and purges are not changes this reports. The key holding no
    /// value is not a configuration, and calling back with the last one — or
    /// with nothing — would both be worse than leaving the running snapshot
    /// alone.
    ///
    /// # Errors
    ///
    /// If the watch cannot be established, if the connection fails or the
    /// stream ends, or if `on_change` returns an error, which ends the watch —
    /// so a caller that wants to survive a bad document should log it and
    /// return `Ok`.
    ///
    /// This never returns `Ok`: a watch either runs or has failed, and a silent
    /// success would leave a spawned task finished and a configuration frozen
    /// with nothing said about either. Callers that want to reconnect should
    /// loop around it.
    ///
    /// Every one of those failures is also reported to the sink
    /// [`reporting_to`](Self::reporting_to) was given, if one was — because a
    /// watch is normally spawned and its `JoinHandle` dropped, so the error
    /// returned here has nowhere else to go. The one failure not charged to the
    /// store is `on_change`'s own refusal: the store answered, `apply` recorded
    /// the delivery, and whether the document then installs is
    /// `ConfigStatus`'s business.
    pub async fn watch<F>(&self, mut on_change: F) -> Result<(), Error>
    where
        F: FnMut(Fetched) -> Result<(), Error> + Send,
    {
        // Neither of these is recorded: no request has left the process, and
        // `RemoteStatus::reachable()` is *whether the store answered the last
        // time it was asked*. Everything below the first round trip reports;
        // see the table in this crate's documentation.
        let format = self.format()?;
        // Refused up front, so a multi-key source fails at `watch` rather than
        // on the first change, hours later.
        let key = self.single_key()?;

        let mut entries = self.store.watch(key).await.map_err(|error| {
            self.failing(Error::remote(format!(
                "{}: cannot watch: {error}",
                self.describe()
            )))
        })?;

        while let Some(entry) = entries.next().await {
            // The stream itself failed. `async-nats` reconnects on its own, so
            // reaching here means it could not — which is exactly the state an
            // operator is asking about when they ask whether the store is up.
            let entry = entry.map_err(|error| {
                self.failing(Error::remote(format!(
                    "{}: the watch failed: {error}",
                    self.describe()
                )))
            })?;

            if entry.operation != Operation::Put {
                continue;
            }

            // What the store put in the key is not a document, which is the
            // same failure a `fetch` of it would have recorded.
            let text = String::from_utf8(entry.value.to_vec()).map_err(|error| {
                self.failing(Error::remote(format!(
                    "{}: the value is not UTF-8: {error}",
                    self.describe()
                )))
            })?;

            // `on_change`'s own refusal is deliberately *not* reported: the
            // store answered, `apply` already counted the delivery, and
            // whether the document installs is `ConfigStatus`'s half of the
            // picture.
            guarded(&mut on_change, Fetched::new(text, format), &self.describe())?;
        }

        // The stream ended without an error: the connection went away, or the
        // bucket did. Also a failure — a watch that stops quietly is a
        // configuration that stops updating quietly.
        Err(self.failing(Error::remote(format!(
            "{}: the watch ended; the stream was closed",
            self.describe()
        ))))
    }
}

impl AsyncRemoteSource for Nats {
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<Fetched, Error>> + Send + '_>> {
        Box::pin(async move {
            let format = self.format()?;

            let documents = self.documents().await?;

            // Read in call order, which is the order the rule wants — so
            // nothing is sorted here.
            documents::merged(&documents, format, self.overlap(), &self.describe())
        })
    }

    fn describe(&self) -> String {
        format!(
            "nats {} bucket {} {}",
            self.server,
            self.bucket,
            self.keys.describe()
        )
    }
}

/// The shared vocabulary, applied to NATS' own connection options.
///
/// `async-nats` opens the files itself, so this passes paths through and
/// refuses bytes. The refusal is the point: a store that quietly dropped a CA
/// would leave a program believing it had pinned a private authority.
fn with_tls_options(
    mut options: ConnectOptions,
    tls: &TlsConfig,
    described: &str,
) -> Result<ConnectOptions, Error> {
    if let Some(ca) = tls.ca_certificate() {
        let path = ca.path().ok_or_else(|| {
            tls_core::unsupported(
                described,
                "a certificate authority from PEM bytes",
                "`async-nats` opens the file itself; name it with \
                 `with_ca_certificate_file`",
            )
        })?;

        options = options.add_root_certificates(path.to_path_buf());
    }

    if let Some(client) = tls.client_certificate() {
        let (certificate, key) = match (client.certificate().path(), client.key().path()) {
            (Some(certificate), Some(key)) => (certificate, key),
            _ => {
                return Err(tls_core::unsupported(
                    described,
                    "a client certificate from PEM bytes",
                    "`async-nats` opens the files itself; name them with \
                     `with_client_certificate_files`",
                ))
            }
        };

        options = options.add_client_certificate(certificate.to_path_buf(), key.to_path_buf());
    }

    // A caller who named an authority expects the connection to be
    // authenticated against it. Without this a `nats://` URL negotiates
    // plaintext and the authority is never consulted — a program believing it
    // is pinned and is not, which is the failure this surface exists to
    // prevent.
    if !tls.is_empty() {
        options = options.require_tls(true);
    }

    Ok(options)
}

impl std::fmt::Debug for Nats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nats")
            .field("server", &self.server)
            .field("bucket", &self.bucket)
            .field("keys", &self.keys)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

/// The format the keys' own extensions agree on, and the complaint if they do
/// not.
///
/// Kept rather than reported at construction: `new` already fails for the
/// things it reached — a server, a bucket — and a key list is not one of them.
/// `with_format` settles it afterwards, which is exactly what the complaint
/// tells the caller to do.
fn agreed(keys: &Keys) -> (Option<Format>, Option<String>) {
    match documents::agreed_format(keys.named()) {
        Ok(format) => (format, None),
        Err(complaint) => (None, Some(complaint)),
    }
}

/// A server URL with its credentials removed, for error messages.
///
/// `nats://s3cr3t@host:4222` and `nats://user:s3cr3t@host:4222` are both
/// ordinary ways to point this at a server, and both put a credential in a
/// string that `describe()` quotes into every error. A comma-separated list
/// is redacted server by server, because that is also a shape NATS accepts.
///
/// [`LoneAuthority::Secret`] is the NATS-specific half: an authority with no
/// colon in it is a *token* here, and keeping it would keep the credential.
/// The Redis crate reads the same shape as a user name, which is why the two
/// pass different arguments to one implementation rather than keeping two.
fn redacted(servers: &str) -> String {
    dynamic_config_store_core::redacted_list(servers, LoneAuthority::Secret)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// Speaks just enough of the NATS protocol to greet a client and then
    /// answer it with `reply`. No Docker, no JetStream — the handshake is all
    /// this needs, because the handshake is where a credential is refused.
    fn scripted(reply: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("nats://{}", listener.local_addr().unwrap());

        let server = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };

            let info = r#"{"server_id":"scripted","server_name":"scripted","version":"2.10.0","proto":1,"go":"","host":"127.0.0.1","port":4222,"headers":true,"max_payload":1048576}"#;
            let _ = stream.write_all(format!("INFO {info}\r\n").as_bytes());

            // The client answers with CONNECT and PING; anything up to the
            // end of the PING is enough to know it is our turn again.
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];

            while !seen.ends_with(b"PING\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                seen.push(byte[0]);
            }

            let _ = stream.write_all(reply.as_bytes());
        });

        (address, server)
    }

    /// The credential half of the promise: a server that says no is `Auth`,
    /// and no amount of waiting changes its mind.
    #[tokio::test]
    async fn a_refused_credential_is_an_auth_failure() {
        let (address, server) = scripted("-ERR 'Authorization Violation'\r\n");

        let error = Nats::with_options(
            &address,
            "config",
            "db.json",
            ConnectOptions::new().token("hunter2-nats-token".to_owned()),
        )
        .await
        .expect_err("the server refused the token");

        let _ = server.join();

        assert_eq!(error.kind(), dynamic_config::ErrorKind::Auth);
        assert!(
            !error.to_string().contains("hunter2"),
            "a refused credential must not be echoed back: {error}"
        );
    }

    /// `nats://token@host` is an ordinary way to point this at a server, and
    /// the address it produces is quoted into every error message. The token
    /// must not survive that trip.
    #[test]
    fn a_credential_in_the_url_never_reaches_an_error_message() {
        assert_eq!(
            redacted("nats://hunter2-token@nats.internal:4222"),
            "nats://***@nats.internal:4222"
        );
        assert_eq!(
            redacted("nats://app:hunter2@nats.internal:4222"),
            "nats://app:***@nats.internal:4222"
        );
        // A password may contain `@`; splitting on the first one would leave
        // its tail in the "redacted" output.
        assert_eq!(
            redacted("nats://app:p@ss@w@rd@nats.internal:4222"),
            "nats://app:***@nats.internal:4222"
        );
        // A list is a shape NATS accepts, so each server is redacted.
        assert_eq!(
            redacted("nats://hunter2@a:4222,nats://hunter2@b:4222"),
            "nats://***@a:4222,nats://***@b:4222"
        );
        // Nothing to redact is left exactly alone.
        assert_eq!(
            redacted("nats://nats.internal:4222"),
            "nats://nats.internal:4222"
        );
        assert_eq!(redacted("not a url"), "not a url");
    }

    /// And end to end: the address the source keeps, and every error it
    /// renders, carry the redacted form.
    #[tokio::test]
    async fn a_credential_in_the_url_never_reaches_a_failed_connection() {
        // Port 9 is discard; nothing listens there.
        let error = Nats::new("nats://hunter2-token@127.0.0.1:9", "config", "db.json")
            .await
            .expect_err("nothing is listening");

        let printed = format!("{error} {error:?}");

        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("127.0.0.1:9"), "{printed}");
    }

    /// Two keys naming two formats is the confusing failure worth catching by
    /// name: `server.toml` parsed as JSON is a syntax error about a file that
    /// has no syntax error in it. It is kept rather than raised at
    /// construction because `with_format` is allowed to settle it.
    #[test]
    fn keys_naming_two_formats_are_reported_rather_than_guessed() {
        let (format, disagreement) = agreed(&Keys::several(["base.json", "local.toml"]));

        assert_eq!(format, None);

        let complaint = disagreement.expect("json and toml cannot both be it");

        assert!(complaint.contains("base.json"), "{complaint}");
        assert!(complaint.contains("local.toml"), "{complaint}");
        assert!(complaint.contains("with_format"), "{complaint}");

        // One format between them is no disagreement at all.
        assert_eq!(
            agreed(&Keys::several(["base.json", "local.json"])),
            (Some(Format::Json), None)
        );
    }

    /// The diagnostic names the whole set, because the merged document is one
    /// layer and one layer cannot say more. A single key must keep the
    /// wording it always had.
    #[test]
    fn a_diagnostic_names_the_whole_set_and_one_key_reads_as_it_always_did() {
        assert_eq!(Keys::one("db.json").describe(), "key db.json");
        assert_eq!(
            Keys::several(["base.json", "local.json"]).describe(),
            "keys base.json, local.json"
        );
    }

    /// The other half, and the one that costs more to get wrong: a server
    /// that is simply not there must stay `Remote`, so a watch loop backs off
    /// instead of stopping.
    #[tokio::test]
    async fn an_unreachable_server_is_remote_rather_than_auth() {
        // Port 9 is discard; nothing listens there.
        let error = Nats::with_options(
            "nats://127.0.0.1:9",
            "config",
            "db.json",
            ConnectOptions::new()
                .token("hunter2-nats-token".to_owned())
                .retry_on_initial_connect()
                .max_reconnects(Some(0)),
        )
        .await
        .expect_err("nothing is listening");

        assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
        assert!(!error.to_string().contains("hunter2"), "{error}");
    }
    // -----------------------------------------------------------------------
    // TLS: the shared vocabulary, and the half NATS cannot express.
    //
    // `with_tls_options` is the whole translation and is a pure function, so
    // it is tested directly: the refusals are the interesting part, and a
    // refusal that only happened after a connection attempt would be a
    // refusal nobody sees in a unit test.
    // -----------------------------------------------------------------------

    /// `async-nats` opens the CA file itself, so there is no byte-taking door
    /// to forward to. Refused, and told where to go — never ignored: a caller
    /// who supplied an authority and got the platform trust store has a
    /// program that believes it is pinned and is not.
    #[test]
    fn a_certificate_authority_from_bytes_is_refused_and_says_what_to_use() {
        let error = with_tls_options(
            ConnectOptions::new(),
            &TlsConfig::new().with_ca_certificate_pem("-----BEGIN CERTIFICATE-----\n"),
            "nats nats://nats.internal:4222 key db.json",
        )
        .expect_err("async-nats takes paths");

        assert!(error.to_string().contains("PEM bytes"), "{error}");
        assert!(
            error.to_string().contains("with_ca_certificate_file"),
            "{error}"
        );
        assert!(
            error.to_string().contains("refused rather than ignored"),
            "{error}"
        );
    }

    /// The same for the client certificate, and the private key is why the
    /// obvious workaround is not taken: writing the bytes to a temporary file
    /// would put a private key on a disk that never asked for one.
    #[test]
    fn a_client_certificate_from_bytes_is_refused_and_never_quotes_the_key() {
        const PLANTED: &str = "PLANTED-PRIVATE-KEY-MATERIAL";

        let error = with_tls_options(
            ConnectOptions::new(),
            &TlsConfig::new().with_client_certificate_pem("cert", PLANTED),
            "nats nats://nats.internal:4222 key db.json",
        )
        .expect_err("async-nats takes paths");

        assert!(!error.to_string().contains(PLANTED), "{error}");
        assert!(
            error.to_string().contains("with_client_certificate_files"),
            "{error}"
        );
    }

    /// The file spellings are the ones NATS has, and they pass through — the
    /// files themselves are opened by `async-nats` at connect time, so
    /// nothing here has to exist yet.
    #[test]
    fn the_file_spellings_are_accepted_and_turn_tls_on() {
        with_tls_options(
            ConnectOptions::new(),
            &TlsConfig::new()
                .with_ca_certificate_file("/etc/nats/ca.pem")
                .with_client_certificate_files("/etc/nats/client.crt", "/etc/nats/client.key"),
            "nats nats://nats.internal:4222 key db.json",
        )
        .expect("paths are what this client takes");
    }

    /// An empty configuration must not turn `require_tls` on behind a
    /// caller's back: it means "the platform defaults", which for a
    /// `nats://` URL is the connection they already had.
    #[test]
    fn an_empty_configuration_changes_nothing() {
        with_tls_options(
            ConnectOptions::new(),
            &TlsConfig::new(),
            "nats nats://nats.internal:4222 key db.json",
        )
        .expect("nothing was asked for");
    }

    /// A NATS URL carries its credential in the authority, so a refusal that
    /// quoted the description raw would put a token in a log. The description
    /// reaching this module is already redacted, and this pins that the
    /// refusal adds nothing back.
    #[test]
    fn a_refusal_carries_the_redacted_server_and_not_the_token() {
        let described = format!(
            "nats {}",
            redacted("nats://hunter2-token@nats.internal:4222")
        );

        let error = with_tls_options(
            ConnectOptions::new(),
            &TlsConfig::new().with_ca_certificate_pem("x"),
            &described,
        )
        .expect_err("async-nats takes paths");

        assert!(!error.to_string().contains("hunter2"), "{error}");
        assert!(error.to_string().contains("nats.internal:4222"), "{error}");
    }
}
