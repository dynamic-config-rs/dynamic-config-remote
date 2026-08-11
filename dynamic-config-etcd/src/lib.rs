//! Read [`dynamic-config`] configuration from an etcd v3 key/value store.
//!
//! etcd speaks gRPC, so its client is async — which is why this implements the
//! **async** [`AsyncRemoteSource`] trait rather than the blocking one.
//!
//! ```no_run
//! use dynamic_config_etcd::Etcd;
//!
//! # struct DbConfig;
//! # impl DbConfig {
//! #     fn set_remote_async(_: Etcd) {}
//! #     async fn refresh_remote_async() -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! DbConfig::set_remote_async(
//!     Etcd::new(["http://etcd.internal:2379"], "myapp/db.json").await?,
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
//! One key, whose value is **a whole configuration document** — the same bytes
//! that would be in a config file. The format comes from the key's extension,
//! or from [`with_format`](Etcd::with_format).
//!
//! # The connection is made once, and lazily
//!
//! [`Etcd::new`] builds the client and [`fetch`](AsyncRemoteSource::fetch)
//! reuses it — a source that reconnected on every read would turn a refresh
//! loop into a connection storm.
//!
//! The underlying client connects *lazily*, so `new` succeeding does not mean
//! the endpoints are reachable: an unreachable etcd surfaces on the first
//! `fetch`, not at construction. That is the client's behaviour rather than a
//! choice made here, and papering over it with an eager round trip would make
//! every construction cost one.
//!
//! # Watching
//!
//! etcd's watch is a real push stream, so [`Etcd::watch`] is a future the caller
//! spawns and cancels by dropping — no runtime is imposed and no flag is polled.
//!
//! ```no_run
//! # use dynamic_config_etcd::Etcd;
//! # async fn example(etcd: Etcd) {
//! # let sink = |_: dynamic_config::Fetched| -> Result<(), dynamic_config::Error> { Ok(()) };
//! let task = tokio::spawn(async move {
//!     etcd.watch(move |document| sink(document)).await
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
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::future::Future;
use std::pin::Pin;

use dynamic_config::{AsyncRemoteSource, Error, Fetched, Format};
use etcd_client::EventType;

/// etcd's own connection options, re-exported so authenticating needs no direct
/// dependency on `etcd-client`.
pub use etcd_client::{Client, ConnectOptions};

/// etcd's TLS types, behind this crate's `tls` feature.
///
/// A separate feature because TLS pulls a whole stack in, and a program talking
/// to etcd over a private network inside a cluster has no use for it.
#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
pub use etcd_client::{Certificate, Identity, TlsOptions};

/// What an expired auth token looks like in etcd's error text.
///
/// etcd issues simple tokens with a TTL — five minutes by default — and refuses
/// requests carrying an expired one. The gRPC channel reconnects on its own;
/// the token does not, so this is the one failure worth recognising by hand.
const INVALID_TOKEN: &str = "invalid auth token";
use tokio::sync::Mutex;

/// A key in etcd, as a configuration source.
pub struct Etcd {
    // etcd's client needs `&mut` to issue a request, so it is behind a lock —
    // a tokio one, because it is held across an await.
    client: Mutex<Client>,
    key: String,
    format: Option<Format>,
    endpoints: String,
}

impl Etcd {
    /// Connects to `endpoints` and reads `key`.
    ///
    /// The format is taken from the key's extension — `myapp/db.json` is JSON.
    /// A key without one needs [`with_format`](Self::with_format).
    ///
    /// # Errors
    ///
    /// If the endpoints cannot be parsed. **Not** if they are unreachable: the
    /// client connects lazily, so that surfaces on the first
    /// [`fetch`](AsyncRemoteSource::fetch).
    pub async fn new<E, S>(endpoints: E, key: impl Into<String>) -> Result<Self, Error>
    where
        E: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::with_options(endpoints, key, ConnectOptions::new()).await
    }

    /// As [`new`](Self::new), with etcd's own connection options.
    ///
    /// This is where authentication and TLS live, because that is where
    /// `etcd-client` puts them — there is no second vocabulary to learn, and
    /// options this crate has never heard of keep working.
    ///
    /// ```no_run
    /// # use dynamic_config_etcd::{ConnectOptions, Etcd};
    /// # async fn example() -> Result<(), dynamic_config::Error> {
    /// let etcd = Etcd::with_options(
    ///     ["https://etcd.internal:2379"],
    ///     "myapp/db.json",
    ///     ConnectOptions::new()
    ///         .with_user("myapp", std::env::var("ETCD_PASSWORD").unwrap())
    ///         .with_keep_alive(
    ///             std::time::Duration::from_secs(30),
    ///             std::time::Duration::from_secs(5),
    ///         ),
    /// )
    /// .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The credentials live in the client afterwards, which is what lets an
    /// expired auth token be replaced without rebuilding anything.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub async fn with_options<E, S>(
        endpoints: E,
        key: impl Into<String>,
        options: ConnectOptions,
    ) -> Result<Self, Error>
    where
        E: IntoIterator<Item = S>,
        S: Into<String>,
    {
        // Collected once: the client wants a slice, and the description wants
        // the same strings.
        let endpoints: Vec<String> = endpoints.into_iter().map(Into::into).collect();
        let described = endpoints.join(", ");

        let client = connect(&endpoints, &options, &described).await?;

        Ok(Self {
            client: Mutex::new(client),
            key: key.into(),
            format: None,
            endpoints: described,
        }
        .with_format_from_key())
    }

    /// Uses a client the program already has.
    ///
    /// For a caller that already talks to etcd and would rather not open a
    /// second connection to it. The client is `Clone` — cheaply, it is a
    /// handle — so sharing one costs nothing.
    ///
    /// ```no_run
    /// # use dynamic_config_etcd::{Client, Etcd};
    /// # fn example(client: Client) {
    /// let etcd = Etcd::from_client(client, "myapp/db.json");
    /// # }
    /// ```
    ///
    /// A shared client recovers from an expired auth token like any other: the
    /// credentials live in the client, so refreshing the token needs nothing
    /// this source would have to own.
    #[must_use]
    pub fn from_client(client: Client, key: impl Into<String>) -> Self {
        Self {
            client: Mutex::new(client),
            key: key.into(),
            format: None,
            endpoints: "<an existing client>".to_owned(),
        }
        .with_format_from_key()
    }

    /// Fills in the format from the key's extension, if it has a known one.
    fn with_format_from_key(mut self) -> Self {
        self.format = Format::from_key(&self.key);

        self
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
    /// # use dynamic_config_etcd::Etcd;
    /// # struct Sink;
    /// # impl Sink {
    /// #     fn apply(&self, _: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
    /// # }
    /// # async fn example(etcd: Etcd) -> Result<(), dynamic_config::Error> {
    /// # let sink = Sink;
    /// sink.apply(etcd.fetch().await?)?;
    /// etcd.watch(move |document| sink.apply(document)).await
    /// # }
    /// ```
    ///
    /// **Cancellation is dropping the future.** There is no stop flag, because
    /// there is nothing to poll one between: this suspends on the stream, so
    /// any executor's cancellation already ends it immediately.
    ///
    /// A deletion is not a change this reports. The key holding no value is not
    /// a configuration, and calling back with the last one — or with nothing —
    /// would both be worse than leaving the running snapshot alone.
    ///
    /// # Errors
    ///
    /// If the watch cannot be established, if the connection fails or ends, if
    /// etcd cancels the watch — compaction is the usual reason — or if
    /// `on_change` returns an error, which ends the watch, so a caller that
    /// wants to survive a bad document should log it and return `Ok`.
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

        let mut stream = match self.watch_once(None).await {
            Err(error) if is_expired_token(&error) => {
                self.refresh_token().await?;

                self.watch_once(None).await?
            }
            outcome => outcome?,
        };

        // Consecutive is what matters: any successfully received message
        // proves the refreshed token worked and resets the count.
        const MOST_TOKEN_RECOVERIES: u32 = 3;
        let mut token_recoveries = 0_u32;
        // Where a re-established stream picks up: just past the last batch
        // this loop was handed.
        let mut resume_from: Option<i64> = None;

        loop {
            let response = match stream.message().await {
                Ok(Some(response)) => {
                    token_recoveries = 0;

                    if let Some(header) = response.header() {
                        resume_from = Some(header.revision() + 1);
                    }

                    response
                }
                Ok(None) => break,
                Err(error) => {
                    let wrapped =
                        Error::remote(format!("{}: the watch failed: {error}", self.describe()));

                    // The single most predictable failure of a long-lived
                    // watch: etcd's simple tokens default to a five-minute
                    // TTL, and a watch is long-lived by definition. Refresh
                    // and re-establish instead of handing the caller a
                    // terminal error for something the credentials can cure.
                    // The new stream resumes just past the last delivered
                    // revision, so a write that lands while the stream is
                    // down is replayed rather than lost; if that revision
                    // has been compacted away meanwhile, etcd cancels the
                    // resumed watch and the cancel branch below makes that a
                    // clean error.
                    //
                    // Bounded twice over: a refresh that fails propagates,
                    // and a server that keeps *accepting* the login while
                    // failing the stream — an auth-enabled proxy in front of
                    // a member without auth, say — hits the recovery cap
                    // instead of hammering the login endpoint forever.
                    if is_expired_token(&wrapped) {
                        token_recoveries += 1;

                        if token_recoveries > MOST_TOKEN_RECOVERIES {
                            return Err(wrapped);
                        }

                        self.refresh_token().await?;
                        stream = self.watch_once(resume_from).await?;

                        continue;
                    }

                    return Err(wrapped);
                }
            };

            // etcd cancels a watch it can no longer serve — most often because
            // the revision it started from has been compacted away. Returning
            // `Ok` here would leave the caller's task finished, the
            // configuration frozen, and nothing said about either.
            if response.canceled() {
                return Err(Error::remote(format!(
                    "{}: the store cancelled the watch: {}",
                    self.describe(),
                    response.cancel_reason()
                )));
            }

            for event in response.events() {
                if event.event_type() != EventType::Put {
                    continue;
                }

                let Some(value) = event.kv() else { continue };

                let text = value.value_str().map_err(|error| {
                    Error::remote(format!(
                        "{}: the value is not UTF-8: {error}",
                        self.describe()
                    ))
                })?;

                guarded(&mut on_change, Fetched::new(text, format), &self.describe())?;
            }
        }

        // The stream ended without an error and without being cancelled: the
        // connection went away. Also a failure, for the same reason — a watch
        // that stops quietly is a configuration that stops updating quietly.
        Err(Error::remote(format!(
            "{}: the watch ended; the connection was closed",
            self.describe()
        )))
    }

    /// Asks etcd for a new auth token, using the credentials the client holds.
    ///
    /// Not a reconnect: the gRPC channel looks after itself, and the client
    /// kept the credentials, so the thing that actually expired is the only
    /// thing replaced. This works for a shared client too, which a reconnect
    /// would not — replacing a client the caller owns is not this crate's to
    /// do.
    ///
    /// # Errors
    ///
    /// If etcd refuses the credentials.
    async fn refresh_token(&self) -> Result<(), Error> {
        self.client
            .lock()
            .await
            .refresh_token()
            .await
            .map_err(|error| {
                Error::remote(format!(
                    "{}: the auth token expired and could not be replaced: {error}",
                    self.describe()
                ))
            })
    }
}

impl Etcd {
    /// One attempt at establishing the watch, with no recovery.
    ///
    /// The client guard is taken to establish the stream and released
    /// immediately. Holding it for the watch's lifetime would block every
    /// `fetch` on this source until the watch ended — which, for a watch, is
    /// never.
    async fn watch_once(
        &self,
        from_revision: Option<i64>,
    ) -> Result<etcd_client::WatchStream, Error> {
        // Resuming replays every event after the one last delivered, so a
        // write that lands while the stream is down is caught up rather than
        // lost. A fresh watch starts at the current revision instead — the
        // startup contract is "changes only".
        let options = from_revision
            .map(|revision| etcd_client::WatchOptions::new().with_start_revision(revision));

        self.client
            .lock()
            .await
            .watch(self.key.as_str(), options)
            .await
            .map_err(|error| Error::remote(format!("{}: cannot watch: {error}", self.describe())))
    }

    /// One read, with no recovery.
    async fn get_once(&self) -> Result<etcd_client::GetResponse, Error> {
        self.client
            .lock()
            .await
            .get(self.key.as_str(), None)
            .await
            .map_err(|error| Error::remote(format!("{}: {error}", self.describe())))
    }
}

/// Whether a failure is etcd saying the auth token has expired.
///
/// Matched on the message because `etcd-client` reports it as a generic gRPC
/// status, and the alternative — treating *every* failure as a reason to
/// refresh — would hide a wrong password behind a refresh loop.
fn is_expired_token(error: &Error) -> bool {
    error.to_string().contains(INVALID_TOKEN)
}

/// One connection attempt, with the endpoints named in any failure.
async fn connect(
    endpoints: &[String],
    options: &ConnectOptions,
    described: &str,
) -> Result<Client, Error> {
    Client::connect(endpoints, Some(options.clone()))
        .await
        .map_err(|error| Error::remote(format!("etcd {described}: {error}")))
}

impl AsyncRemoteSource for Etcd {
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<Fetched, Error>> + Send + '_>> {
        Box::pin(async move {
            let format = self.format.ok_or_else(|| {
                Error::remote(format!(
                    "{}: the key names no format; call `with_format`",
                    self.describe()
                ))
            })?;

            let response = match self.get_once().await {
                Err(error) if is_expired_token(&error) => {
                    // etcd's simple tokens have a TTL — five minutes by
                    // default — and a long-lived reader outlives one. The gRPC
                    // channel looks after itself; the token does not, so this
                    // is the one failure worth recovering from by hand.
                    //
                    // Once, not in a loop: if a fresh token is refused too, the
                    // credentials are wrong and retrying would turn a clear
                    // failure into a hang.
                    self.refresh_token().await?;

                    self.get_once().await?
                }
                outcome => outcome?,
            };

            let value = response.kvs().first().ok_or_else(|| {
                Error::remote(format!("{}: the key holds no value", self.describe()))
            })?;

            let text = value.value_str().map_err(|error| {
                Error::remote(format!(
                    "{}: the value is not UTF-8: {error}",
                    self.describe()
                ))
            })?;

            Ok(Fetched::new(text, format))
        })
    }

    fn describe(&self) -> String {
        format!("etcd {} key {}", self.endpoints, self.key)
    }
}

impl std::fmt::Debug for Etcd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Etcd")
            .field("endpoints", &self.endpoints)
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
