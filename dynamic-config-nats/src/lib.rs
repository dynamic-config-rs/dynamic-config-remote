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
//! # Watching
//!
//! A KV bucket is a stream, so [`Nats::watch`] is a future the caller spawns and
//! cancels by dropping — no runtime is imposed and no flag is polled.
//!
//! ```no_run
//! # use dynamic_config_nats::Nats;
//! # struct DbConfig;
//! # impl DbConfig {
//! #     fn apply_remote(_: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! # async fn example(nats: Nats) {
//! let task = tokio::spawn(async move {
//!     nats.watch(DbConfig::apply_remote).await
//! });
//!
//! // Dropping or aborting the task stops the watch.
//! task.abort();
//! # }
//! ```
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::future::Future;
use std::pin::Pin;

use async_nats::jetstream::kv::{Operation, Store};
use dynamic_config::{AsyncRemoteSource, Error, Fetched, Format};

/// NATS' own connection options, re-exported so authenticating needs no direct
/// dependency on `async-nats`.
///
/// Every credential NATS understands lives here: a token, a user and password,
/// an NKey, a JWT, a `.creds` file, TLS. There is no second vocabulary to learn,
/// and options this crate has never heard of keep working.
pub use async_nats::{Client, ConnectOptions};
use futures_util::StreamExt;

/// A key in a JetStream bucket, as a configuration source.
pub struct Nats {
    store: Store,
    key: String,
    format: Option<Format>,
    server: String,
    bucket: String,
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
        key: impl Into<String>,
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
        key: impl Into<String>,
        options: ConnectOptions,
    ) -> Result<Self, Error> {
        let server = server.into();
        let bucket = bucket.into();
        let key = key.into();

        let client = options
            .connect(&server)
            .await
            .map_err(|error| Error::remote(format!("nats {server}: {error}")))?;

        let store = async_nats::jetstream::new(client)
            .get_key_value(&bucket)
            .await
            .map_err(|error| Error::remote(format!("nats {server} bucket {bucket}: {error}")))?;

        let format = Format::from_key(&key);

        Ok(Self {
            store,
            key,
            format,
            server,
            bucket,
        })
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
        key: impl Into<String>,
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
    pub fn from_store(store: Store, key: impl Into<String>) -> Self {
        let key = key.into();
        let bucket = store.name.clone();

        let format = Format::from_key(&key);

        Self {
            store,
            key,
            format,
            // The store does not carry the address it was reached through, and
            // inventing one would put a wrong server in every error message.
            server: "<an existing connection>".to_owned(),
            bucket,
        }
    }

    /// States the format, for a key whose name does not.
    #[must_use]
    pub fn with_format(mut self, format: Format) -> Self {
        self.format = Some(format);
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
    /// # struct DbConfig;
    /// # impl DbConfig {
    /// #     fn apply_remote(_: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
    /// # }
    /// # async fn example(nats: Nats) -> Result<(), dynamic_config::Error> {
    /// DbConfig::apply_remote(nats.fetch().await?)?;
    /// nats.watch(DbConfig::apply_remote).await
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
    pub async fn watch<F>(&self, mut on_change: F) -> Result<(), Error>
    where
        F: FnMut(Fetched) -> Result<(), Error> + Send,
    {
        let format = self.format.ok_or_else(|| {
            Error::remote(format!(
                "{}: the key names no format; call `with_format`",
                self.describe()
            ))
        })?;

        let mut entries = self.store.watch(&self.key).await.map_err(|error| {
            Error::remote(format!("{}: cannot watch: {error}", self.describe()))
        })?;

        while let Some(entry) = entries.next().await {
            let entry = entry.map_err(|error| {
                Error::remote(format!("{}: the watch failed: {error}", self.describe()))
            })?;

            if entry.operation != Operation::Put {
                continue;
            }

            let text = String::from_utf8(entry.value.to_vec()).map_err(|error| {
                Error::remote(format!(
                    "{}: the value is not UTF-8: {error}",
                    self.describe()
                ))
            })?;

            guarded(&mut on_change, Fetched::new(text, format), &self.describe())?;
        }

        // The stream ended without an error: the connection went away, or the
        // bucket did. Also a failure — a watch that stops quietly is a
        // configuration that stops updating quietly.
        Err(Error::remote(format!(
            "{}: the watch ended; the stream was closed",
            self.describe()
        )))
    }
}

impl AsyncRemoteSource for Nats {
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<Fetched, Error>> + Send + '_>> {
        Box::pin(async move {
            let format = self.format.ok_or_else(|| {
                Error::remote(format!(
                    "{}: the key names no format; call `with_format`",
                    self.describe()
                ))
            })?;

            let value = self
                .store
                .get(&self.key)
                .await
                .map_err(|error| Error::remote(format!("{}: {error}", self.describe())))?
                .ok_or_else(|| {
                    Error::remote(format!("{}: the key holds no value", self.describe()))
                })?;

            let text = String::from_utf8(value.to_vec()).map_err(|error| {
                Error::remote(format!(
                    "{}: the value is not UTF-8: {error}",
                    self.describe()
                ))
            })?;

            Ok(Fetched::new(text, format))
        })
    }

    fn describe(&self) -> String {
        format!(
            "nats {} bucket {} key {}",
            self.server, self.bucket, self.key
        )
    }
}

impl std::fmt::Debug for Nats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nats")
            .field("server", &self.server)
            .field("bucket", &self.bucket)
            .field("key", &self.key)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

/// Runs the watch callback with a panic net.
///
/// The callback is the caller's code on the caller's thread; a panic in it
/// used to unwind through the watch loop and kill that thread with the
/// `RemoteWatch` handle still looking alive. Caught, it becomes an orderly
/// error: the watch ends, and the caller is told why.
fn guarded<F>(on_change: &mut F, document: Fetched, described: &str) -> Result<(), Error>
where
    F: FnMut(Fetched) -> Result<(), Error>,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_change(document))).unwrap_or_else(
        |_| {
            Err(Error::remote(format!(
                "{described}: the watch callback panicked; the watch is stopped"
            )))
        },
    )
}
