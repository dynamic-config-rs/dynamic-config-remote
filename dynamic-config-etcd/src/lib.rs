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
//! # Several keys as one document
//!
//! A deployment that splits its configuration across a range —  `myapp/db.json`,
//! `myapp/server.json` — can have one source read the lot, and [`Keys`] says
//! which:
//!
//! ```no_run
//! # use dynamic_config_etcd::{Etcd, Keys};
//! # async fn example() -> Result<(), dynamic_config::Error> {
//! # let endpoints = ["http://etcd.internal:2379"];
//! // Named keys: a list of layers, merged in the order given, later wins.
//! let etcd = Etcd::new(endpoints, Keys::several(["myapp/base.json", "myapp/local.json"])).await?;
//!
//! // A prefix: disjoint sections, and an overlap between two of them is an error.
//! let etcd = Etcd::new(endpoints, Keys::prefix("myapp/"))
//!     .await?
//!     .with_format(dynamic_config::Format::Json);
//! # Ok(())
//! # }
//! ```
//!
//! Both are **one round trip**: a list is a transaction of range reads and a
//! prefix is one range read, so either way the keys are read at a single etcd
//! revision and a write landing mid-read cannot tear the document in half.
//!
//! Three consequences, each of which belongs here rather than in an incident:
//!
//! - **A prefix that matches more than 512 keys is refused.** A prefix is
//!   caller input and the answer to it is server input; an empty prefix
//!   matches a whole cluster.
//! - **Provenance becomes store-grained.** The merged document is one layer,
//!   so `source_of` answers "from etcd … keys a, b" and not which of them
//!   supplied a given value. [`describe`](AsyncRemoteSource::describe) names
//!   every key in the set, which is as close as one layer gets.
//! - **One unreadable key fails the whole fetch.** A configuration quietly
//!   missing a section is worse than a refresh that failed and left the last
//!   document serving.
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
//! # Timeouts
//!
//! [`Etcd::with_timeout`] is the deadline for a single fetch attempt,
//! excluding retries the underlying client performs — the sentence every
//! store in this family answers to. Ten seconds by default.
//!
//! etcd's own `ConnectOptions::with_timeout` bounds *connecting*, which is a
//! different thing and does not help a connection established minutes ago, so
//! the deadline here wraps the request. Both can be set; they cover different
//! halves. Neither applies to [`Etcd::watch`], which is long-lived on purpose.
//!
//! # Watching
//!
//! etcd's watch is a real push stream, so [`Etcd::watch`] is a future the caller
//! spawns and cancels by dropping — no runtime is imposed and no flag is polled.
//!
//! **A prefix can be watched; a named list cannot.** A watch on a set is only
//! honest if the store says *the set* changed and the set can then be re-read
//! as of one instant. A prefix answers both: one stream over the range says the
//! set moved and carries the revision it moved at, and one range read at that
//! revision is the whole subtree as one instant had it — so a delivered
//! document is a state the cluster really was in, never one key's new value
//! merged with another's old one. A named list answers neither: etcd
//! establishes a watch on a key or a range, so a list is N independent streams,
//! and none of them is about the set. That shape refuses at
//! [`watch`](Etcd::watch), before the first event; poll
//! `refresh_remote_async()` on a timer instead — it is the same one round trip
//! the fetch always was.
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
//! # A watch that is failing says so
//!
//! A watch is the half of a store `dynamic-config` cannot see: a delivery keeps
//! `RemoteStatus` current, and a stream that broke delivers nothing and would
//! otherwise report nothing — so `dynamic_config_remote_up` would describe the
//! last *delivery* rather than the last *attempt*.
//! [`reporting_to`](Etcd::reporting_to) closes that: the sink the loop already
//! holds is told about every attempt that came back with nothing, and a store
//! that stopped answering an hour ago reads as down without anything having to
//! call `refresh_remote_async()`.
//!
//!
//! # Every failure branch of the watch loop, and what it reports
//!
//! A watch is the half of a store `dynamic-config` cannot see, and
//! [`reporting_to`](Etcd::reporting_to) is what lets it speak: the sink the
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
//! | the format is missing, or the source names a list of keys | no — rule 3: nothing has been asked of the cluster |
//! | the stream cannot be established | **yes** — the first round trip |
//! | …because the token had expired, and the refresh worked | no — the store answered, the credential was replaced, and the resumed stream lost no event |
//! | …and the refresh, the re-establish, or the recovery cap fails | **yes** |
//! | the stream errors for any other reason | **yes**, and the watch ends |
//! | etcd cancels the watch — a compacted revision, usually | **yes**, and the watch ends |
//! | a prefix batch's range read fails (one token refresh and retry first) | **yes** |
//! | two keys under a prefix supply one path | **yes** — the read failed, not the callback |
//! | the value is not UTF-8 | **yes** — the same failure a `fetch` of it would have recorded |
//! | a progress notification, or an event that is not a `Put` | no — nothing changed |
//! | a single key was deleted, or the last key under a prefix went away | no — see the note below |
//! | `on_change` refuses the document | no — the store answered; `apply` counted the delivery, and what the document did next is `ConfigStatus`'s half |
//! | the stream ends without an error | **yes** — a watch that stops quietly is a configuration that stops updating quietly |
//!
//! **The deletion row is a difference between stores, deliberately left
//! standing.** Here and in `dynamic-config-redis` a key holding nothing leaves
//! the running snapshot alone and says nothing, because the store is answering
//! and only a delivery clears a streak — reporting it would park `remote_up`
//! at zero for as long as nobody recreated the key. `dynamic-config-consul`
//! records it as a failed attempt instead, on the argument that a `fetch` of
//! the same key fails. Both are defensible, both are written down at the
//! branch, and neither moves in a patch release.
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use dynamic_config::{AsyncRemoteSource, Error, Fetched, Format, RemoteSink};
use dynamic_config_store_core::attempts::Attempts;
use dynamic_config_store_core::documents::{self, Overlap};
use dynamic_config_store_core::guarded;
use etcd_client::EventType;
use tokio::sync::Mutex;

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

/// A private certificate authority and a client certificate, as data.
///
/// The shared vocabulary all seven store crates take, so that reaching TLS
/// never means naming a `tonic` type — see [`Etcd::with_tls`]. Visible without
/// the `tls` feature so that the *type* is nameable everywhere; the
/// constructor that consumes it is not, because a TLS stack is what the
/// feature buys.
pub use dynamic_config_store_core::tls::TlsConfig;

/// What an expired auth token looks like in etcd's error text.
///
/// etcd issues simple tokens with a TTL — five minutes by default — and refuses
/// requests carrying an expired one. The gRPC channel reconnects on its own;
/// the token does not, so this is the one failure worth recognising by hand.
const INVALID_TOKEN: &str = "invalid auth token";

/// How etcd words the refusals that no amount of waiting will cure.
///
/// Matched on the message rather than the gRPC code because etcd does not use
/// one code for them: `authentication failed` arrives as `InvalidArgument`,
/// `permission denied` as `PermissionDenied`, `invalid auth token` as
/// `Unauthenticated`. The message is the part that is stable across all three.
const AUTH_REFUSALS: [&str; 5] = [
    INVALID_TOKEN,
    "authentication failed",
    "permission denied",
    "user name is empty",
    "user name not found",
];

/// The longest named key list one transaction can carry.
///
/// etcd's own `--max-txn-ops`, whose default is 128 and which is a server-side
/// limit rather than a client one. A longer list would have to go as several
/// transactions at several revisions — which is precisely the torn document
/// reading them in one transaction exists to prevent — so it is refused
/// instead, and says so.
const MOST_TRANSACTION_KEYS: usize = 128;

/// How long one request may take before it is given up on.
///
/// Ten seconds, matching the three HTTP stores. A configuration fetch that
/// hangs is worse than one that fails: the caller can retry a failure.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a source reads: one key, several named keys, or a range.
///
/// Every constructor takes one, and a bare `&str` or `String` is
/// [`Keys::one`] — so the single-key spelling every caller already wrote keeps
/// working unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Keys {
    /// One key, whose value is the whole document.
    One(String),
    /// Several named keys, merged **in the order given — later wins**.
    ///
    /// The rule a list of `.file(..)` calls already teaches: the caller wrote
    /// the list, so the list is the precedence. Read as one etcd transaction,
    /// so every key is read at the same revision.
    ///
    /// **Cannot be watched.** etcd establishes a watch on a key or on a range,
    /// so an arbitrary list is one stream per key, and N independent streams
    /// never say *the set* moved — they say one key did, N times. Watch a
    /// prefix, or poll `refresh_remote_async()`.
    Several(Vec<String>),
    /// Every key under a prefix, merged as **disjoint sections**.
    ///
    /// A caller naming a prefix is not expressing an order — the order is
    /// whatever etcd lists, which is nobody's decision — so two keys under the
    /// prefix supplying the same path is a deployment bug, and reported as one
    /// rather than resolved. Read as a single range request.
    ///
    /// **Can be watched**, and is the only multi-key shape here that can: one
    /// stream over the range says the set moved and carries the revision it
    /// moved at, and one range read at that revision is the set as of one
    /// instant. See [`Etcd::watch`].
    Prefix(String),
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

    /// Every key under `prefix`, merged as disjoint sections.
    #[must_use]
    pub fn prefix(prefix: impl Into<String>) -> Self {
        Self::Prefix(prefix.into())
    }

    /// The keys as a slice, for the diagnostics and the format inference.
    ///
    /// A prefix has none to list — the set is not known until etcd answers.
    fn named(&self) -> &[String] {
        match self {
            Self::One(key) => std::slice::from_ref(key),
            Self::Several(keys) => keys,
            Self::Prefix(_) => &[],
        }
    }

    /// How a diagnostic names what this source reads.
    fn describe(&self) -> String {
        match self {
            Self::One(key) => format!("key {key}"),
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

/// A key in etcd, as a configuration source.
pub struct Etcd {
    // etcd's client needs `&mut` to issue a request, so it is behind a lock —
    // a tokio one, because it is held across an await.
    client: Mutex<Client>,
    keys: Keys,
    format: Option<Format>,
    /// Why the keys' own extensions could not settle the format between them.
    ///
    /// Kept rather than reported at construction because `with_format` is
    /// allowed to settle it afterwards — and because `new` is not the only
    /// door in, so a store whose constructor cannot fail would have nowhere
    /// to report it.
    disagreement: Option<String>,
    endpoints: String,
    timeout: Duration,
    /// Where the watch loop reports an attempt that came back with nothing.
    ///
    /// Nobody, unless [`reporting_to`](Etcd::reporting_to) said otherwise —
    /// which is what makes reporting free for a caller who never asked for it.
    attempts: Attempts,
}

impl Etcd {
    /// Connects to `endpoints` and reads `keys`.
    ///
    /// `keys` is a key — `"myapp/db.json"` — or a [`Keys`], for the several-keys
    /// and prefix forms.
    ///
    /// The format is taken from the key's extension — `myapp/db.json` is JSON.
    /// A key without one, and every prefix, needs
    /// [`with_format`](Self::with_format).
    ///
    /// # Errors
    ///
    /// If the endpoints cannot be parsed. **Not** if they are unreachable: the
    /// client connects lazily, so that surfaces on the first
    /// [`fetch`](AsyncRemoteSource::fetch).
    pub async fn new<E, S>(endpoints: E, keys: impl Into<Keys>) -> Result<Self, Error>
    where
        E: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::with_options(endpoints, keys, ConnectOptions::new()).await
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
        keys: impl Into<Keys>,
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

        Ok(Self::build(client, keys, described))
    }

    /// As [`with_options`](Self::with_options), with a private certificate
    /// authority or a client certificate from the shared vocabulary.
    ///
    /// The same three settings, spelled the same way, in all seven store
    /// crates — and spelled as *data*, so nothing here names a `tonic` type:
    ///
    /// ```no_run
    /// # use dynamic_config_etcd::{ConnectOptions, Etcd, TlsConfig};
    /// # async fn example() -> Result<(), dynamic_config::Error> {
    /// let etcd = Etcd::with_tls(
    ///     ["https://etcd.internal:2379"],
    ///     "myapp/db.json",
    ///     ConnectOptions::new().with_user("myapp", std::env::var("ETCD_PASSWORD").unwrap()),
    ///     &TlsConfig::new()
    ///         .with_ca_certificate_file("/etc/etcd/ca.pem")
    ///         .with_client_certificate_files("/etc/etcd/client.crt", "/etc/etcd/client.key"),
    /// )
    /// .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// etcd expresses all of it: a CA from a file or from bytes, and a client
    /// certificate from either. mTLS is not an afterthought here the way it is
    /// for the HTTP stores — an etcd cluster with `--client-cert-auth` is the
    /// ordinary hardened deployment.
    ///
    /// `options` carries everything that is *not* TLS: the user and password,
    /// keep-alive, whatever `etcd-client` grows next. **The `tls` argument owns
    /// the TLS slot.** If `options` also carries a
    /// [`TlsOptions`] of its own, this one replaces it — `etcd-client` exposes
    /// no way to ask whether that slot is already filled, so the interaction is
    /// documented rather than refused. Use one door or the other, never both.
    ///
    /// There is no way to turn verification off; [`TlsConfig`]'s own
    /// documentation argues that one, and `tonic` offers no such switch to
    /// forward even if this crate wanted to.
    ///
    /// # Errors
    ///
    /// If a PEM file cannot be read, if what was read is not PEM, or as
    /// [`new`](Self::new).
    #[cfg(feature = "tls")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
    pub async fn with_tls<E, S>(
        endpoints: E,
        keys: impl Into<Keys>,
        options: ConnectOptions,
        tls: &TlsConfig,
    ) -> Result<Self, Error>
    where
        E: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let endpoints: Vec<String> = endpoints.into_iter().map(Into::into).collect();
        let described = endpoints.join(", ");

        let options = options.with_tls(tls_options(tls, &format!("etcd {described}"))?);

        let client = connect(&endpoints, &options, &described).await?;

        Ok(Self::build(client, keys, described))
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
    pub fn from_client(client: Client, keys: impl Into<Keys>) -> Self {
        Self::build(client, keys, "<an existing client>".to_owned())
    }

    /// The one place a source is assembled, so the format inference and its
    /// disagreement cannot drift between the three doors in.
    fn build(client: Client, keys: impl Into<Keys>, endpoints: String) -> Self {
        let keys = keys.into();

        let (format, disagreement) = match documents::agreed_format(keys.named()) {
            Ok(format) => (format, None),
            Err(complaint) => (None, Some(complaint)),
        };

        Self {
            client: Mutex::new(client),
            keys,
            format,
            disagreement,
            endpoints,
            timeout: DEFAULT_TIMEOUT,
            attempts: Attempts::default(),
        }
    }

    /// How long a single fetch may take before it is given up on. Ten seconds
    /// by default.
    ///
    /// The deadline for **one fetch attempt**, excluding retries the
    /// underlying client performs — the same sentence every store in this
    /// family answers to.
    ///
    /// It is applied here as a `tokio::time::timeout` around the request
    /// rather than through `ConnectOptions::with_timeout`, and the difference
    /// is the whole point: etcd's own option bounds *connecting*, and a
    /// connection that was established minutes ago cannot be bounded by it. A
    /// member that accepts the request and then never answers is the failure
    /// worth having a deadline for, and only the wrap catches it.
    ///
    /// It does not cover [`watch`](Self::watch), which is long-lived by
    /// definition; a watch that stops after ten seconds would be a watch that
    /// does not work. It *does* bound each range read a prefix watch performs
    /// in answer to an event — that is a request like any other, and one that
    /// hangs would wedge the loop for good.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Reports the watch loop's failed attempts to `sink`.
    ///
    /// Without this a watch is the half of a store `dynamic-config` cannot
    /// see. [`RemoteSink::apply`] records a delivery, so a *working* watch
    /// keeps the status current — but a loop whose stream broke, whose watch
    /// was cancelled or whose credential was refused delivers nothing, and so
    /// says nothing: `dynamic_config_remote_up` reports the last delivery
    /// rather than the last attempt, and a store that stopped answering an hour
    /// ago looks healthy until something calls `refresh_remote_async()`.
    ///
    /// ```no_run
    /// # use dynamic_config_etcd::Etcd;
    /// # struct DbConfig;
    /// # impl DbConfig {
    /// #     fn remote_sink() -> dynamic_config::RemoteSink { unimplemented!() }
    /// # }
    /// # async fn example(etcd: Etcd) -> Result<(), dynamic_config::Error> {
    /// let sink = DbConfig::remote_sink();
    ///
    /// // The same sink delivers and reports: one generation, one fence.
    /// etcd.reporting_to(sink)
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
    /// endpoint, no key, no credential — enters a `RemoteStatus`.
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

    /// Calls `on_change` every time what this source reads moves, forever.
    ///
    /// **One key or a prefix.** A prefix watch is the multi-key case that can
    /// be answered honestly: etcd's watch says the *range* moved and carries
    /// the revision it moved at, and one range read at that revision is the
    /// whole set as of one instant. So the document delivered is a state the
    /// cluster really was in, never a merge of one key's new value with
    /// another's old one. A **named list** is still refused; the reason is on
    /// [`Keys::Several`].
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
    /// A deletion is not a change this reports **for a single key**. The key
    /// holding no value is not a configuration, and calling back with the last
    /// one — or with nothing — would both be worse than leaving the running
    /// snapshot alone. Under a **prefix** a deletion is a change like any
    /// other: the set is what it is after the delete, and the re-read reports
    /// it — unless nothing is left under the prefix, which is the same
    /// no-configuration case and is skipped for the same reason.
    ///
    /// # Errors
    ///
    /// If the watch cannot be established, if the connection fails or ends, if
    /// etcd cancels the watch — compaction is the usual reason — or if
    /// `on_change` returns an error, which ends the watch, so a caller that
    /// wants to survive a bad document should log it and return `Ok`.
    ///
    /// Under a prefix, also if the range read at an event's revision fails, or
    /// if two keys under the prefix supply the same path — that is a
    /// deployment bug rather than a blip, and retrying it forever with nothing
    /// said would leave the configuration frozen and silent.
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
        // Returned and recorded nowhere: nothing has been asked of the
        // cluster yet, and `RemoteStatus::reachable()` is *whether the store
        // answered the last time it was asked*. Everything below the first
        // round trip reports; see the table in this crate's documentation.
        let format = self.format()?;

        if let Keys::Several(_) = &self.keys {
            // Refused rather than approximated. etcd's watch is established on
            // a key or a range, so a caller's arbitrary list would need one
            // stream per key — and nothing about N independent streams says
            // *the set* moved, which is the first of the two things a watch on
            // a set has to answer. A prefix is one stream over one range and
            // answers it, which is why that shape is the one that landed.
            //
            // Not reported: this refusal is about the *source*, and no
            // request has left the process. A status saying the cluster is
            // unreachable would be untrue, and it carries no message to
            // correct the operator reading it with.
            return Err(Error::remote(format!(
                "{}: a source that reads a named list of keys cannot be \
                 watched; etcd establishes a watch on a key or a range, so a \
                 list would be one stream per key and none of them would say \
                 the set moved together — watch a prefix, or poll \
                 `refresh_remote_async()` on a timer, which is one round trip",
                self.describe()
            )));
        }

        let mut stream = match self.watch_once(None).await {
            // A token that expired before the stream existed is not a failure
            // an operator needs woken for while it can still be cured — only
            // the cure failing is. See `reporting_to`.
            Err(error) if is_expired_token(&error) => {
                self.refresh_token()
                    .await
                    .map_err(|error| self.failing(error))?;

                self.watch_once(None)
                    .await
                    .map_err(|error| self.failing(error))?
            }
            outcome => outcome.map_err(|error| self.failing(error))?,
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
                    let wrapped = classified(
                        &format!("{}: the watch failed: {error}", self.describe()),
                        &error,
                    );

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
                    //
                    // Nothing is reported for a recovery that *works*: the
                    // store answered, the credential was replaced, and the
                    // resumed stream lost no event. Reporting it would drive
                    // `remote_up` to zero every time a five-minute token
                    // turned over on a cluster that is perfectly healthy —
                    // and, because only a delivery or a fetch clears the
                    // streak, it would stay there until the next change. The
                    // three failures around it do report: a refusal to
                    // re-authenticate, a stream that will not re-establish,
                    // and a recovery cap that ran out.
                    if is_expired_token(&wrapped) {
                        token_recoveries += 1;

                        if token_recoveries > MOST_TOKEN_RECOVERIES {
                            return Err(self.failing(wrapped));
                        }

                        self.refresh_token()
                            .await
                            .map_err(|error| self.failing(error))?;
                        stream = self
                            .watch_once(resume_from)
                            .await
                            .map_err(|error| self.failing(error))?;

                        continue;
                    }

                    return Err(self.failing(wrapped));
                }
            };

            // etcd cancels a watch it can no longer serve — most often because
            // the revision it started from has been compacted away. Returning
            // `Ok` here would leave the caller's task finished, the
            // configuration frozen, and nothing said about either.
            if response.canceled() {
                return Err(self.failing(Error::remote(format!(
                    "{}: the store cancelled the watch: {}",
                    self.describe(),
                    response.cancel_reason()
                ))));
            }

            // A prefix watch answers with *the set moved*, not with a
            // document: the events name the keys that changed, and the
            // configuration is every key under the prefix. So the document is
            // read back — once per batch rather than once per event, because a
            // batch is one revision — as a range read **at the revision the
            // event carries**. One range read is evaluated at one revision, so
            // that read is the atomic half a watch on a set needs; pinning it
            // to the event's own revision rather than to `now` is what makes
            // the delivered document the state the event announced instead of
            // whatever has landed since.
            if let Keys::Prefix(prefix) = &self.keys {
                // A progress notification carries no events and no change.
                if response.events().is_empty() {
                    continue;
                }

                let Some(revision) = response.header().map(etcd_client::ResponseHeader::revision)
                else {
                    continue;
                };

                let documents = match self.range_at(prefix, revision).await {
                    // The stream was established with a token that has since
                    // expired: the stream itself survives, and only the read
                    // this batch needs is refused. One refresh, one retry —
                    // the same bound `watch_once` uses.
                    Err(error) if is_expired_token(&error) => {
                        self.refresh_token()
                            .await
                            .map_err(|error| self.failing(error))?;

                        self.range_at(prefix, revision)
                            .await
                            .map_err(|error| self.failing(error))?
                    }
                    outcome => outcome.map_err(|error| self.failing(error))?,
                };

                // The last key under the prefix went away. No configuration is
                // not a configuration, so the running snapshot stays — the same
                // rule a deleted single key gets.
                if documents.is_empty() {
                    continue;
                }

                // The merge is the last of the reading: two keys under the
                // prefix supplying one path is a deployment bug, and it is
                // reported here for the same reason a fetch that hit it would
                // be — the read is what failed, not the callback.
                let document =
                    documents::merged(&documents, format, Overlap::Refused, &self.describe())
                        .map_err(|error| self.failing(error))?;

                // `on_change`'s own refusal is deliberately *not* reported:
                // the store answered, `apply` already counted the delivery,
                // and whether the document installs is `ConfigStatus`'s half
                // of the picture.
                guarded(&mut on_change, document, &self.describe())?;

                continue;
            }

            for event in response.events() {
                if event.event_type() != EventType::Put {
                    continue;
                }

                let Some(value) = event.kv() else { continue };

                // What the store put in the key is not a document, which is
                // the same failure a `fetch` of it would have recorded.
                let text = value.value_str().map_err(|error| {
                    self.failing(Error::remote(format!(
                        "{}: the value is not UTF-8: {error}",
                        self.describe()
                    )))
                })?;

                guarded(&mut on_change, Fetched::new(text, format), &self.describe())?;
            }
        }

        // The stream ended without an error and without being cancelled: the
        // connection went away. Also a failure, for the same reason — a watch
        // that stops quietly is a configuration that stops updating quietly.
        Err(self.failing(Error::remote(format!(
            "{}: the watch ended; the connection was closed",
            self.describe()
        ))))
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
                classified(
                    &format!(
                        "{}: the auth token expired and could not be replaced: {error}",
                        self.describe()
                    ),
                    &error,
                )
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
        let mut options = etcd_client::WatchOptions::new();

        if let Some(revision) = from_revision {
            options = options.with_start_revision(revision);
        }

        // `watch` refuses a named list before reaching here, so only the two
        // shapes etcd can establish one stream for arrive.
        let key = match &self.keys {
            Keys::One(key) => key.as_str(),
            Keys::Prefix(prefix) => {
                options = options.with_prefix();

                prefix.as_str()
            }
            Keys::Several(_) => {
                return Err(Error::remote(format!(
                    "{}: only a single key or a prefix can be watched",
                    self.describe()
                )))
            }
        };

        self.client
            .lock()
            .await
            .watch(key, Some(options))
            .await
            .map_err(|error| {
                classified(
                    &format!("{}: cannot watch: {error}", self.describe()),
                    &error,
                )
            })
    }

    /// One range read of `prefix`, evaluated at `revision`.
    ///
    /// The re-read half of a prefix watch, and the reason that watch can be
    /// honest: etcd evaluates a range read at a single revision, so the pairs
    /// this returns are the subtree as one instant had it. Bounded by
    /// [`with_timeout`](Self::with_timeout), like every other request — a
    /// member that accepts the read and never answers would otherwise wedge
    /// the loop for good.
    async fn range_at(&self, prefix: &str, revision: i64) -> Result<Vec<(String, String)>, Error> {
        let read = async {
            let response = self
                .client
                .lock()
                .await
                .get(prefix, Some(prefix_options(Some(revision))))
                .await
                .map_err(|error| self.wrapped(&error))?;

            documents::within_key_budget(response.kvs().len(), &self.describe())?;

            self.pairs_of(&response, None)
        };

        tokio::time::timeout(self.timeout, read)
            .await
            .unwrap_or_else(|_| {
                Err(Error::remote(format!(
                    "{}: timed out after {:?} re-reading the range the watch \
                     reported a change to",
                    self.describe(),
                    self.timeout
                )))
            })
    }

    /// One read of whatever this source reads, with no recovery, bounded by
    /// [`with_timeout`](Self::with_timeout).
    ///
    /// One round trip in all three shapes. A named list goes as a transaction
    /// of range reads rather than as N gets, which is not merely fewer packets:
    /// a transaction is evaluated at one revision, so a write landing between
    /// two of the keys cannot produce a document that never existed.
    async fn get_once(&self) -> Result<Vec<(String, String)>, Error> {
        if let Keys::Several(keys) = &self.keys {
            if keys.len() > MOST_TRANSACTION_KEYS {
                return Err(Error::remote(format!(
                    "{}: {} keys is more than the {MOST_TRANSACTION_KEYS} one etcd \
                     transaction carries (`--max-txn-ops`); reading them would take \
                     several round trips at several revisions, which is the torn \
                     document this avoids — read a prefix, or install a source per \
                     group",
                    self.describe(),
                    keys.len()
                )));
            }
        }

        let read = async {
            let mut client = self.client.lock().await;

            match &self.keys {
                Keys::One(key) => {
                    let response = client
                        .get(key.as_str(), None)
                        .await
                        .map_err(|error| self.wrapped(&error))?;

                    self.pairs_of(&response, Some(key))
                }
                Keys::Several(keys) => {
                    let transaction = etcd_client::Txn::new().and_then(
                        keys.iter()
                            .map(|key| etcd_client::TxnOp::get(key.as_str(), None))
                            .collect::<Vec<_>>(),
                    );

                    let answered = client
                        .txn(transaction)
                        .await
                        .map_err(|error| self.wrapped(&error))?;

                    let mut documents = Vec::with_capacity(keys.len());

                    // Zipped with the request, not read out of the response:
                    // a range read for a key that is not there answers with an
                    // empty range, so the response alone cannot say which key
                    // was missing.
                    for (key, answer) in keys.iter().zip(answered.op_responses()) {
                        let etcd_client::TxnOpResponse::Get(response) = answer else {
                            return Err(Error::remote(format!(
                                "{}: the store answered a read with something else",
                                self.describe()
                            )));
                        };

                        documents.extend(self.pairs_of(&response, Some(key))?);
                    }

                    Ok(documents)
                }
                Keys::Prefix(prefix) => {
                    let response = client
                        .get(prefix.as_str(), Some(prefix_options(None)))
                        .await
                        .map_err(|error| self.wrapped(&error))?;

                    documents::within_key_budget(response.kvs().len(), &self.describe())?;

                    self.pairs_of(&response, None)
                }
            }
        };

        // The lock is inside the deadline on purpose: waiting behind another
        // request is time the caller waited for this fetch, and a deadline
        // that excluded it would be a deadline the caller cannot rely on.
        tokio::time::timeout(self.timeout, read)
            .await
            .unwrap_or_else(|_| {
                Err(Error::remote(format!(
                    "{}: timed out after {:?}",
                    self.describe(),
                    self.timeout
                )))
            })
    }

    /// The `(key, document)` pairs in one range response.
    ///
    /// `expected` is the key that was asked for, when one was: a range read
    /// answering with nothing means that key holds no value, and **that fails
    /// the whole fetch**. Merging the four keys that did answer would leave a
    /// process running a configuration with a section quietly missing from it,
    /// which is worse than a refresh that failed and left the last document in
    /// place.
    fn pairs_of(
        &self,
        response: &etcd_client::GetResponse,
        expected: Option<&str>,
    ) -> Result<Vec<(String, String)>, Error> {
        if let Some(key) = expected {
            if response.kvs().is_empty() {
                return Err(Error::remote(format!(
                    "{}: `{key}` holds no value",
                    self.describe()
                )));
            }
        }

        response
            .kvs()
            .iter()
            .map(|value| {
                let key = value.key_str().map_err(|error| {
                    Error::remote(format!("{}: a key is not UTF-8: {error}", self.describe()))
                })?;

                // Only ever reachable through a proxy that rewrote the range,
                // and one comparison is cheaper than finding out the hard way
                // that a prefix meant something else.
                if let Keys::Prefix(prefix) = &self.keys {
                    documents::under_prefix(key, prefix, &self.describe())?;
                }

                let text = value.value_str().map_err(|error| {
                    Error::remote(format!(
                        "{}: `{key}` is not UTF-8: {error}",
                        self.describe()
                    ))
                })?;

                Ok((key.to_owned(), text.to_owned()))
            })
            .collect()
    }

    /// One of etcd's failures, named and classified.
    fn wrapped(&self, error: &etcd_client::Error) -> Error {
        classified(&format!("{}: {error}", self.describe()), error)
    }
}

/// How a prefix is read, whether by a fetch or by a watch's re-read.
///
/// One over the budget, so a range that is too big is *known* to be too big
/// rather than silently truncated — a truncated prefix read is a configuration
/// missing a section, which is the failure this whole feature is about not
/// having.
///
/// `revision` pins the read to one the caller already knows: a watch reads at
/// the revision its event carried, and a fetch reads at whatever is current.
/// Both are one revision, which is the property that matters; only the watch
/// needs to name which.
fn prefix_options(revision: Option<i64>) -> etcd_client::GetOptions {
    let options = etcd_client::GetOptions::new()
        .with_prefix()
        .with_limit(i64::try_from(documents::MOST_KEYS.saturating_add(1)).unwrap_or(i64::MAX));

    match revision {
        Some(revision) => options.with_revision(revision),
        None => options,
    }
}

/// Sorts one of etcd's failures into a kind, without stringifying first.
///
/// The `GRpcStatus` guard is what keeps this honest: `permission denied` is
/// also what an OS says about a file it will not open, and `etcd-client`
/// reports that as `IoError`. Only a refusal that came back over the wire can
/// be an auth refusal.
fn classified(message: &str, error: &etcd_client::Error) -> Error {
    match error {
        etcd_client::Error::GRpcStatus(status) if is_auth_refusal(status.message()) => {
            Error::auth(message)
        }
        _ => Error::remote(message),
    }
}

/// Whether a gRPC status message is etcd refusing the credentials.
fn is_auth_refusal(message: &str) -> bool {
    AUTH_REFUSALS
        .iter()
        .any(|refusal| message.contains(refusal))
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
        .map_err(|error| classified(&format!("etcd {described}: {error}"), &error))
}

impl AsyncRemoteSource for Etcd {
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<Fetched, Error>> + Send + '_>> {
        Box::pin(async move {
            let format = self.format()?;

            let documents = match self.get_once().await {
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

            // etcd lists a range in key order, and a transaction answers in
            // request order — which is exactly the order each rule wants, so
            // nothing is sorted here.
            documents::merged(&documents, format, self.overlap(), &self.describe())
        })
    }

    fn describe(&self) -> String {
        format!("etcd {} {}", self.endpoints, self.keys.describe())
    }
}

impl Etcd {
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
}

/// The shared vocabulary, translated into `tonic`'s own TLS types.
///
/// `described` is what an error names the source by; the material itself never
/// appears in one. In particular the PEM parse failures underneath are *not*
/// wrapped: `rustls-pki-types` renders the line it choked on, and the line it
/// choked on in a private key file is private key material — so the failure is
/// reported here, at construction, only as far as which file was wrong.
///
/// tonic validates the PEM lazily, when the channel is built, which is why
/// there is nothing to fail on for a malformed certificate until `connect`.
#[cfg(feature = "tls")]
fn tls_options(tls: &TlsConfig, described: &str) -> Result<TlsOptions, Error> {
    let mut options = TlsOptions::new();

    if let Some(pem) = tls.ca_certificate_pem(described)? {
        // Replaces the trust store rather than adding to it, which is what
        // pinning a private authority means. `tls-roots` is the feature that
        // says "the platform's roots as well", and it applies when no CA is
        // named here.
        options = options.ca_certificate(Certificate::from_pem(pem));
    }

    if let Some((certificate, key)) = tls.client_certificate_pem(described)? {
        options = options.identity(Identity::from_pem(certificate, key));
    }

    Ok(options)
}

impl std::fmt::Debug for Etcd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Etcd")
            .field("endpoints", &self.endpoints)
            .field("keys", &self.keys)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusals a running etcd actually sends, verbatim. If etcd ever
    /// rewords one, this is the test that says so rather than a watch loop
    /// quietly retrying a wrong password forever.
    #[test]
    fn etcds_own_refusals_are_recognised() {
        for message in [
            "etcdserver: invalid auth token",
            "etcdserver: authentication failed, invalid user ID or password",
            "etcdserver: permission denied",
            "etcdserver: user name is empty",
            "etcdserver: user name not found",
        ] {
            assert!(is_auth_refusal(message), "{message}");
        }
    }

    /// The over-classification this guard exists to prevent: a store that is
    /// merely down must stay `Remote`, because a watch loop backs off on that
    /// and stops on `Auth`.
    #[test]
    fn an_ordinary_failure_is_not_an_auth_refusal() {
        for message in [
            "etcdserver: request timed out",
            "etcdserver: too many requests",
            "transport error: connection refused",
            // The trap the `GRpcStatus` guard covers from the other side: a
            // file etcd's TLS setup could not open says this too, with a
            // capital P and no gRPC status behind it.
            "Permission denied (os error 13)",
        ] {
            assert!(!is_auth_refusal(message), "{message}");
        }
    }

    #[tokio::test]
    async fn a_fetch_from_a_server_that_never_answers_ends_at_the_deadline() {
        // Accepts the connection and then says nothing at all — the failure
        // a connect timeout cannot see, and the reason the deadline wraps the
        // request rather than the connection.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());

        let silent = std::thread::spawn(move || {
            let held = listener.accept();
            // Held until the test is done with it: dropping the socket here
            // would answer the client with a close, which is an answer.
            std::thread::sleep(Duration::from_secs(2));
            drop(held);
        });

        let source = Etcd::new([address], "myapp/db.json")
            .await
            .expect("the endpoint parses; connecting is lazy")
            .with_timeout(Duration::from_millis(200));

        let started = std::time::Instant::now();
        let error = source.fetch().await.expect_err("nothing ever answers");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "the deadline must bound the fetch, not merely the connect: {elapsed:?}"
        );
        assert!(error.to_string().contains("timed out"), "{error}");
        assert_eq!(
            error.kind(),
            dynamic_config::ErrorKind::Remote,
            "a store that went quiet may yet come back; that is not an auth failure"
        );

        let _ = silent.join();
    }

    /// The password goes in through `ConnectOptions`, and supplying one makes
    /// `connect` log in there and then — so the construction error is where a
    /// leak would surface, and `{:?}` where it would surface next.
    #[tokio::test]
    async fn neither_an_error_nor_debug_prints_a_credential() {
        // Port 9 is discard; nothing listens there.
        let error = Etcd::with_options(
            ["http://127.0.0.1:9"],
            "myapp/db.json",
            ConnectOptions::new().with_user("myapp", "hunter2-etcd-password"),
        )
        .await
        .expect_err("nothing is listening");

        let printed = format!("{error} {error:?}");

        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("127.0.0.1:9"), "{printed}");
        assert_eq!(
            error.kind(),
            dynamic_config::ErrorKind::Remote,
            "a refused connection is the store being unreachable, not the \
             credentials being wrong — a watch loop backs off on one and \
             stops on the other"
        );
    }

    /// And the same for the ordinary path, where the endpoint is all this
    /// type holds: a `Debug` of it names the store, never a secret.
    #[tokio::test]
    async fn debug_names_the_store_and_nothing_else() {
        let source = Etcd::new(["http://127.0.0.1:9"], "myapp/db.json")
            .await
            .expect("the endpoint parses; connecting is lazy");

        let printed = format!("{source:?}");

        assert!(printed.contains("myapp/db.json"), "{printed}");
        assert!(printed.contains("127.0.0.1:9"), "{printed}");
    }

    /// A bare string still means one key, which is what keeps every caller
    /// who wrote the single-key spelling compiling.
    #[test]
    fn a_bare_key_is_still_one_key() {
        assert_eq!(Keys::from("myapp/db.json"), Keys::one("myapp/db.json"));
        assert_eq!(
            Keys::from("myapp/db.json".to_owned()),
            Keys::one("myapp/db.json")
        );
    }

    /// Provenance is store-grained once several keys become one document, so
    /// the one thing `describe()` can still do is name the whole set — it is
    /// what `source_of` reports and what every error quotes.
    #[tokio::test]
    async fn describe_names_every_key_in_the_set() {
        let several = Etcd::new(
            ["http://127.0.0.1:9"],
            Keys::several(["myapp/base.json", "myapp/local.json"]),
        )
        .await
        .expect("the endpoint parses");

        assert!(
            several.describe().contains("myapp/base.json")
                && several.describe().contains("myapp/local.json"),
            "{}",
            several.describe()
        );

        let prefix = Etcd::new(["http://127.0.0.1:9"], Keys::prefix("myapp/"))
            .await
            .expect("the endpoint parses");

        assert!(
            prefix.describe().contains("prefix myapp/"),
            "{}",
            prefix.describe()
        );
    }

    /// A prefix has no extension to infer from, so the refusal has to name the
    /// call that settles it — before any round trip, so a misconfiguration
    /// fails at once rather than against a server.
    #[tokio::test]
    async fn a_prefix_with_no_format_says_which_call_supplies_one() {
        let source = Etcd::new(["http://127.0.0.1:9"], Keys::prefix("myapp/"))
            .await
            .expect("the endpoint parses");

        let error = source.fetch().await.expect_err("no format was ever named");

        assert!(error.to_string().contains("with_format"), "{error}");
    }

    /// Two keys naming two formats is caught by name rather than parsed as
    /// whichever came first — which would be a syntax error about a document
    /// that has no syntax error in it.
    #[tokio::test]
    async fn keys_naming_two_formats_are_refused_until_one_is_chosen() {
        let source = Etcd::new(
            ["http://127.0.0.1:9"],
            Keys::several(["myapp/db.json", "myapp/server.toml"]),
        )
        .await
        .expect("the endpoint parses");

        let error = source
            .fetch()
            .await
            .expect_err("json and toml cannot both be the format");

        assert!(error.to_string().contains("myapp/db.json"), "{error}");
        assert!(error.to_string().contains("myapp/server.toml"), "{error}");

        // ...and `with_format` settles it, which is what makes the refusal a
        // signpost rather than a dead end. The fetch then fails on the network
        // instead, which is the next honest thing to fail on.
        let settled = Etcd::new(
            ["http://127.0.0.1:9"],
            Keys::several(["myapp/db.json", "myapp/server.toml"]),
        )
        .await
        .expect("the endpoint parses")
        .with_format(Format::Json);

        let error = settled.fetch().await.expect_err("nothing is listening");

        assert!(!error.to_string().contains("with_format"), "{error}");
    }

    /// etcd caps a transaction at `--max-txn-ops`, so a longer list would have
    /// to be read at several revisions — the torn document the transaction was
    /// chosen to prevent. Refused with the number in it rather than silently
    /// split.
    #[tokio::test]
    async fn a_named_list_longer_than_one_transaction_is_refused() {
        let keys: Vec<String> = (0..=MOST_TRANSACTION_KEYS)
            .map(|n| format!("myapp/{n:04}.json"))
            .collect();

        let source = Etcd::new(["http://127.0.0.1:9"], Keys::several(keys))
            .await
            .expect("the endpoint parses");

        let error = source.fetch().await.expect_err("one key too many");

        assert!(error.to_string().contains("max-txn-ops"), "{error}");
        assert!(
            error.to_string().contains("129"),
            "the count belongs in the message: {error}"
        );
    }

    /// Refused, not approximated: etcd establishes a watch on a key or a
    /// range, so a caller's list is N streams and none of them is about the
    /// set. The refusal has to arrive at `watch`, before the first event —
    /// hours later is not a refusal.
    #[tokio::test]
    async fn a_named_list_refuses_to_be_watched_and_says_what_to_do() {
        let source = Etcd::new(
            ["http://127.0.0.1:9"],
            Keys::several(["myapp/db.json", "myapp/server.json"]),
        )
        .await
        .expect("the endpoint parses");

        let error = source
            .watch(|_| Ok(()))
            .await
            .expect_err("a named list cannot be watched");

        assert!(error.to_string().contains("cannot be watched"), "{error}");
        assert!(
            error.to_string().contains("refresh_remote_async"),
            "{error}"
        );
        assert!(
            error.to_string().contains("watch a prefix"),
            "the refusal must name the shape that does work: {error}"
        );
    }

    /// The other half of the same decision: a prefix is *not* refused, so the
    /// only way it can fail here is by failing to reach the endpoint. This is
    /// what keeps the refusal above from quietly widening back over the shape
    /// that was built.
    #[tokio::test]
    async fn a_prefix_is_not_refused_at_the_door() {
        let source = Etcd::new(["http://127.0.0.1:9"], Keys::prefix("myapp/"))
            .await
            .expect("the endpoint parses")
            .with_format(Format::Json);

        let error = source
            .watch(|_| Ok(()))
            .await
            .expect_err("nothing is listening on port 9");

        assert!(
            !error.to_string().contains("cannot be watched"),
            "a prefix watch must fail on the connection, not on a refusal: {error}"
        );
    }

    // -----------------------------------------------------------------------
    // Reporting a watch that is failing.
    //
    // The half of a store `dynamic-config` cannot see: a watch that never
    // reaches etcd delivers nothing, so without this the status would still
    // describe the last delivery. What can be asserted without a cluster is
    // the wiring — that a failure arrives, once, and that a source nobody
    // wired behaves exactly as it did. Breaking a *running* stream needs a
    // real server and lives in `tests/against_etcd.rs`.
    // -----------------------------------------------------------------------

    /// A watch that cannot be established is the case this exists for: it
    /// delivers nothing, and the caller has usually spawned it and dropped the
    /// handle, so nothing else would ever say so.
    #[tokio::test]
    async fn a_watch_that_cannot_be_established_reports_the_store_as_unreachable() {
        use dynamic_config::{Remote, RemoteSink};

        // Its own `static`, because a `RemoteSink` needs one and two tests
        // sharing one would race.
        static UNREACHABLE: Remote = Remote::new();

        fn reloaded() -> Result<(), Error> {
            Ok(())
        }

        // Port 9 is discard; nothing listens there.
        let source = Etcd::new(["http://127.0.0.1:9"], Keys::prefix("myapp/"))
            .await
            .expect("the endpoint parses; connecting is lazy")
            .with_format(Format::Json)
            .reporting_to(RemoteSink::new(&UNREACHABLE, reloaded, "etcd"));

        source
            .watch(|_| Ok(()))
            .await
            .expect_err("nothing is listening on port 9");

        let status = UNREACHABLE.status();

        assert_eq!(
            status.consecutive_failures, 1,
            "one attempt, reported exactly once — a site reported twice would \
             make the streak a count of branches rather than of attempts"
        );
        assert_eq!(status.reachable(), Some(false));
        assert_eq!(
            status.fetches, 0,
            "an attempt that returned nothing is not a fetch"
        );
        assert_eq!(
            status.last_fetch, None,
            "and it must not invent a read that never happened"
        );

        let failure = status.last_failure.as_ref().expect("the attempt failed");

        assert_eq!(failure.kind, dynamic_config::ErrorKind::Remote);
        assert!(
            !format!("{status:?}").contains("127.0.0.1"),
            "a store's address never enters a status: {status:?}"
        );
    }

    /// A refusal that arrives *before the first round trip* is not reported,
    /// and this test used to assert the opposite.
    ///
    /// 0.6.1's audit of all seven watch loops found the two halves of the
    /// family disagreeing — etcd and NATS reporting a refusal that never
    /// reached the store, Redis and S3 not — each with a test. What settles it
    /// is `RemoteStatus::reachable()`'s own contract: *whether the store
    /// answered the last time it was asked*. A source that names a list of
    /// keys never asks, so `Some(false)` here was a status saying something
    /// untrue about a cluster that may be perfectly healthy — and the status
    /// carries no message to correct it with. The error still says exactly
    /// what is wrong, to the caller holding it.
    #[tokio::test]
    async fn a_refusal_before_the_first_round_trip_is_not_a_store_that_stopped_answering() {
        use dynamic_config::{Remote, RemoteSink};

        static REFUSED: Remote = Remote::new();

        fn reloaded() -> Result<(), Error> {
            Ok(())
        }

        let source = Etcd::new(
            ["http://127.0.0.1:9"],
            Keys::several(["myapp/db.json", "myapp/server.json"]),
        )
        .await
        .expect("the endpoint parses")
        .reporting_to(RemoteSink::new(&REFUSED, reloaded, "etcd"));

        let error = source
            .watch(|_| Ok(()))
            .await
            .expect_err("a named list cannot be watched");

        assert!(error.to_string().contains("cannot be watched"), "{error}");
        assert_eq!(
            REFUSED.status().reachable(),
            None,
            "nothing has been asked of this cluster, so it is neither up nor down"
        );
    }

    /// The default every source carries: reporting nowhere changes nothing a
    /// caller can see, including the error they get back.
    #[tokio::test]
    async fn a_source_that_reports_nowhere_fails_exactly_as_it_always_did() {
        let source = Etcd::new(["http://127.0.0.1:9"], Keys::prefix("myapp/"))
            .await
            .expect("the endpoint parses")
            .with_format(Format::Json);

        let error = source
            .watch(|_| Ok(()))
            .await
            .expect_err("nothing is listening on port 9");

        assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
        assert!(error.to_string().contains("cannot watch"), "{error}");
    }

    // -----------------------------------------------------------------------
    // TLS: the shared vocabulary, translated into tonic's own types.
    //
    // tonic validates PEM lazily — a `Certificate::from_pem` keeps the bytes
    // and the channel decides later — so what can be asserted without a
    // cluster is the half this crate owns: reading the files, and refusing
    // loudly when it cannot.
    // -----------------------------------------------------------------------

    /// A CA file that is not there is an error naming the path, from the
    /// constructor rather than from a panic somewhere in a builder chain.
    #[cfg(feature = "tls")]
    #[tokio::test]
    async fn a_missing_ca_file_names_the_path_and_the_material() {
        let error = Etcd::with_tls(
            ["https://127.0.0.1:9"],
            "myapp/db.json",
            ConnectOptions::new(),
            &TlsConfig::new().with_ca_certificate_file("/nonexistent/etcd-ca.pem"),
        )
        .await
        .expect_err("the CA file is not there");

        assert!(
            error.to_string().contains("/nonexistent/etcd-ca.pem"),
            "{error}"
        );
        assert!(error.to_string().contains("the CA certificate"), "{error}");
    }

    /// The private key is the sharpest secret in this feature. The file is
    /// read here, so a read failure must name the path and nothing that was
    /// in it — and a key that *was* read must not travel into a diagnostic
    /// either.
    #[cfg(feature = "tls")]
    #[tokio::test]
    async fn a_private_key_never_reaches_an_error_or_a_debug() {
        const PLANTED: &str = "PLANTED-PRIVATE-KEY-MATERIAL";

        let tls = TlsConfig::new()
            .with_ca_certificate_pem("-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----\n")
            .with_client_certificate_pem(
                "-----BEGIN CERTIFICATE-----\ncert\n-----END CERTIFICATE-----\n",
                format!("-----BEGIN PRIVATE KEY-----\n{PLANTED}\n-----END PRIVATE KEY-----\n"),
            );

        assert!(!format!("{tls:?}").contains(PLANTED), "{tls:?}");

        // tonic builds the channel eagerly enough to reject the material, and
        // the message it produces for that is the one this crate quotes. It
        // must carry nothing of what it choked on — which is exactly the
        // hazard `rustls-pki-types`' own renderer has, and the reason no PEM
        // error in this family is wrapped.
        let error = Etcd::with_tls(
            ["https://127.0.0.1:9"],
            "myapp/db.json",
            ConnectOptions::new(),
            &tls,
        )
        .await
        .expect_err("that is not a certificate");

        assert!(!error.to_string().contains(PLANTED), "{error}");
        assert!(!format!("{error:?}").contains(PLANTED), "{error:?}");
    }

    /// Both halves of a client certificate have to arrive, so a missing key
    /// file fails naming the key rather than quietly presenting a certificate
    /// with nothing to prove it.
    #[cfg(feature = "tls")]
    #[tokio::test]
    async fn a_client_certificate_with_no_readable_key_is_refused() {
        let error = Etcd::with_tls(
            ["https://127.0.0.1:9"],
            "myapp/db.json",
            ConnectOptions::new(),
            &TlsConfig::new().with_client_certificate_files(
                "/nonexistent/client.crt",
                "/nonexistent/client.key",
            ),
        )
        .await
        .expect_err("neither file is there");

        assert!(
            error.to_string().contains("the client certificate"),
            "{error}"
        );
    }
}
