//! Read [`dynamic-config`] configuration from a Redis key.
//!
//! Redis speaks a plain request/response protocol, so this implements the
//! **blocking** [`RemoteSource`] trait: nothing here needs an async runtime,
//! and neither does using it.
//!
//! ```no_run
//! use dynamic_config_redis::Redis;
//!
//! # struct DbConfig;
//! # impl DbConfig {
//! #     fn set_remote(_: Redis) {}
//! #     fn refresh_remote() -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! DbConfig::set_remote(Redis::new("redis://redis.internal:6379", "myapp/db.json")?);
//!
//! // Fetching is explicit; the load that follows touches no network.
//! DbConfig::refresh_remote()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # What it reads
//!
//! One key, whose value is **a whole configuration document** — the same bytes
//! that would be in a config file. The format comes from the key's extension,
//! or from [`with_format`](Redis::with_format).
//!
//! A Redis hash would be the other obvious mapping — one field per setting —
//! and is deliberately not what this does. A hash cannot hold a nested table
//! without inventing a flattening convention, and a document already has one.
//!
//! # Several keys as one document
//!
//! A deployment that splits its configuration across several keys can have one
//! source read the lot, and [`Keys`] says which:
//!
//! ```no_run
//! # use dynamic_config_redis::{Keys, Redis};
//! # fn example() -> Result<(), dynamic_config::Error> {
//! # let url = "redis://redis.internal:6379";
//! // Named keys: a list of layers, merged in the order given, later wins.
//! let redis = Redis::new(url, Keys::several(["myapp/base.json", "myapp/local.json"]))?;
//!
//! // A prefix: disjoint sections, and an overlap between two of them is an error.
//! let redis = Redis::new(url, Keys::prefix("myapp/"))?
//!     .with_format(dynamic_config::Format::Json);
//! # Ok(())
//! # }
//! ```
//!
//! The two forms cost different things, and Redis is the store where the
//! difference matters most:
//!
//! - **A named list is one `MGET`** — one command, one round trip, and Redis
//!   runs it as one operation, so the set is consistent. It is also the only
//!   multi-key shape here that can be **watched**; see [`Redis::watch`].
//! - **A prefix is a `SCAN` and then an `MGET`**, and the `SCAN` is
//!   deliberately not `KEYS`: `KEYS` walks the whole key space in one blocking
//!   operation and is the classic way to stall a production Redis. The price is
//!   that `SCAN` is **not atomic** — a key written while the cursor is moving
//!   may or may not be in the set — so a prefix read here can catch a
//!   deployment mid-write in a way a named list cannot, and cannot be watched
//!   at all. Prefer a named list where the keys are known.
//!
//! The prefix is matched as a **literal**, not as a pattern. `SCAN MATCH` takes
//! a glob, so `*`, `?`, `[` and `\` in the prefix are escaped before the
//! command goes out, and every key the server answers with is checked against
//! the literal prefix before it is used — a prefix means the prefix, not
//! whatever a glob would have made of it.
//!
//! Three more consequences that belong here rather than in an incident:
//!
//! - **A prefix that matches more than 512 keys is refused.**
//! - **Provenance becomes store-grained.** The merged document is one layer, so
//!   `source_of` answers "from redis … keys a, b" and not which key supplied a
//!   value. [`describe`](RemoteSource::describe) names the set.
//! - **One unreadable key fails the whole fetch.**
//!
//! # Credentials
//!
//! In the URL, which is where Redis puts them and where every deployment
//! already has them: `redis://user:password@host:6379/0`, or `rediss://` for
//! TLS — which needs this crate's `tls` feature to supply the client's rustls
//! stack. [`from_client`](Redis::from_client) takes a client the program
//! already built, for anything the URL cannot say.
//!
//! A password Redis will not accept — `NOAUTH`, `WRONGPASS`, `NOPERM` — is
//! reported as `ErrorKind::Auth` rather than `Remote`, because reconnecting
//! does not change the server's mind. The password itself never reaches the
//! message: the URL is redacted before it is stored.
//!
//! # Timeouts
//!
//! [`Redis::with_timeout`] is the deadline for a single fetch attempt,
//! excluding retries the underlying client performs — the sentence every store
//! in this family answers to. Ten seconds by default.
//!
//! Redis has three separate knobs and this sets all of them from the one
//! value: connect, write and read. A deadline covering only the connect would
//! sail past a server that accepted the socket and then went quiet, which is
//! what a wedged Redis actually looks like.
//!
//! # Watching
//!
//! Keyspace notifications, so [`Redis::watch`] is genuinely change-driven —
//! no polling, no timer. It runs on a thread rather than a future, because
//! nothing here needs a runtime, so stopping it has to come from outside —
//! hence the [`Watching`] token.
//!
//! **A named list can be watched; a prefix cannot.** A watch on a set is only
//! honest if the store says *the set* changed and the set can then be read as
//! of one instant. A subscription per key answers the first, and `MGET`
//! answers the second: it is one command, and Redis runs one command as one
//! operation, so the values it returns are the set as of one point in the
//! command stream. The document delivered is therefore a state the server
//! really held. A prefix has to *find* its keys again first, and `SCAN` is a
//! cursor walked over many commands with writes free to land between them —
//! so it refuses at [`watch`](Redis::watch), before the first notification;
//! name the keys with [`Keys::several`], or poll `refresh_remote()` on a
//! timer instead.
//!
//! ```no_run
//! # use dynamic_config::RemoteWatch;
//! # use dynamic_config_redis::Redis;
//! # fn example(redis: Redis) {
//! # let sink = |_: dynamic_config::Fetched| -> Result<(), dynamic_config::Error> { Ok(()) };
//! let watch = RemoteWatch::new();
//! let watching = watch.watching();
//!
//! std::thread::spawn(move || redis.watch(&watching, move |document| sink(document)));
//!
//! // Dropping `watch` — or calling `watch.stop()` — ends the loop.
//! # }
//! ```
//!
//! **A failing watch says so, if it is asked to.**
//! [`reporting_to`](Redis::reporting_to) hands the loop the same sink it
//! delivers through, and the failures inside it — a re-read that came back
//! with nothing, and a subscription that died — are reported to the
//! `RemoteStatus` as they happen. Without it a watch is the half of a store
//! `dynamic-config` cannot see: only deliveries are recorded, so
//! `dynamic_config_remote_up` describes the last *delivery* rather than the
//! last *attempt*.
//!
//!
//! # Every failure branch of the watch loop, and what it reports
//!
//! A watch is the half of a store `dynamic-config` cannot see, and
//! [`reporting_to`](Redis::reporting_to) is what lets it speak: the sink the
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
//! | the format is missing, or the keys cannot be watched | no — rule 3: nothing has been asked of the server |
//! | keyspace notifications are switched off on the server | **yes** — a `CONFIG GET` answered, and it answered that this watch cannot work |
//! | the subscriber connection, the database index, a `SUBSCRIBE`, or the read timeout fails | **yes** — every one of those is a round trip, or a socket that has already made one |
//! | the read timeout expires with no message | no — that is how `stop` is noticed |
//! | the subscription fails | **yes**, and the watch ends |
//! | a `del` or `expired` event | no — see the note below |
//! | the re-read after a notification fails | **yes**, and the loop waits for the next notification |
//! | a coalesced duplicate: the set came back the same | no — the server answered |
//! | `on_change` refuses the document | no — the server answered; `apply` counted the delivery, and what the document did next is `ConfigStatus`'s half |
//!
//! **The deletion row is a difference between stores, deliberately left
//! standing.** Here and in `dynamic-config-etcd` a key holding nothing leaves
//! the running snapshot alone and says nothing, because the server is
//! answering and only a delivery clears a streak — reporting it would park
//! `remote_up` at zero for as long as nobody recreated the key.
//! `dynamic-config-consul` records it instead, on the argument that a `fetch`
//! of the same key fails. Both are written down at the branch, and neither
//! moves in a patch release.
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::sync::Mutex;
use std::time::Duration;

use dynamic_config::{Error, Fetched, Format, RemoteSink, RemoteSource, Watching};
use dynamic_config_store_core::attempts::Attempts;
use dynamic_config_store_core::documents::{self, Overlap};
use dynamic_config_store_core::{guarded, LoneAuthority};
use redis::Commands;

/// Redis' own client, re-exported so [`from_client`](Redis::from_client) needs
/// no direct dependency.
pub use redis::Client;

/// A private certificate authority and a client certificate, as data.
///
/// The shared vocabulary all seven store crates take, so that reaching TLS
/// never means naming a `redis` type — see [`Redis::with_tls`]. Visible without
/// the `tls` feature so that the *type* is nameable everywhere; the constructor
/// that consumes it is not, because a TLS stack is what the feature buys.
pub use dynamic_config_store_core::tls::TlsConfig;

/// How long to wait for a change before looking again.
///
/// A subscription blocks until a message arrives; this is how long that block
/// lasts before the loop checks whether it has been told to stop.
const POLL_SLICE: Duration = Duration::from_millis(250);

/// How long one fetch may take before it is given up on.
///
/// Ten seconds, matching the rest of the family. A configuration fetch that
/// hangs is worse than one that fails: the caller can retry a failure.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// How many keys one `SCAN` round asks the server for.
///
/// `COUNT` is a hint, not a limit, and a low one turns a scan into many round
/// trips while a high one asks the server to do more work per reply. A hundred
/// is the shape of a configuration key space rather than of a cache.
const SCAN_BATCH: usize = 100;

/// The most `SCAN` rounds one prefix read will make.
///
/// A cursor is server state and a server that never advances it — a broken
/// proxy, a cluster answering for the wrong slot — would otherwise be an
/// infinite loop inside a fetch. The budget on the *keys* cannot catch that on
/// its own: a scan can return nothing at all, round after round.
const MOST_SCAN_ROUNDS: usize = 1_000;

/// What a source reads: one key, several named keys, or a prefix.
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
    /// the list, so the list is the precedence. One `MGET`, so Redis reads the
    /// set as one operation — which is also what makes this the one multi-key
    /// shape [`Redis::watch`] accepts.
    Several(Vec<String>),
    /// Every key under a literal prefix, merged as **disjoint sections**.
    ///
    /// A caller naming a prefix is not expressing an order — `SCAN` returns
    /// keys in whatever order the hash table is walked in — so two keys under
    /// it supplying the same path is a deployment bug, and reported as one
    /// rather than resolved.
    ///
    /// `SCAN`, never `KEYS`: `KEYS` blocks the server for the length of the
    /// whole key space. The price is that the scan is not atomic, and that
    /// price is also why a prefix **cannot be watched**: re-finding the keys
    /// after a notification is a cursor walked over many commands, so the set
    /// it collects can be half from before a write and half from after — a
    /// document the server never held. `MGET` on a named list has no such
    /// window.
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
    /// A prefix has none to list — the set is not known until the scan ends.
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

/// A key in Redis, as a configuration source.
///
/// Not `Clone`: it holds a connection, and two clones sharing a key while each
/// opening their own would double the connections for no gain. Wrap it in an
/// `Arc` if two places need one.
pub struct Redis {
    client: Client,
    /// One connection, reused. A source that reconnected on every read would
    /// turn a refresh loop into a connection storm.
    connection: Mutex<Option<redis::Connection>>,
    keys: Keys,
    format: Option<Format>,
    /// Why the keys' own extensions could not settle the format between them.
    ///
    /// Kept rather than reported at construction because `with_format` is
    /// allowed to settle it afterwards.
    disagreement: Option<String>,
    described: String,
    timeout: Duration,
    /// Where the watch loop reports an attempt that came back with nothing.
    ///
    /// Nobody, unless [`reporting_to`](Redis::reporting_to) said otherwise:
    /// a fetch records itself through `refresh_remote`, and a loop is the
    /// half of this store `dynamic-config` cannot see on its own.
    attempts: Attempts,
}

impl Redis {
    /// The key `key`, on the Redis at `url`.
    ///
    /// The format is taken from the key's extension — `myapp/db.json` is JSON.
    /// A key without one needs [`with_format`](Self::with_format).
    ///
    /// # Errors
    ///
    /// If the URL cannot be parsed. **Not** if the server is unreachable: the
    /// connection is opened on the first read, so that construction stays free
    /// of I/O like every other source in this family.
    pub fn new(url: &str, keys: impl Into<Keys>) -> Result<Self, Error> {
        // `redacted`, even here: a malformed URL still carries its password,
        // and a parse error is the error most likely to be pasted somewhere.
        let client = Client::open(url)
            .map_err(|error| Error::remote(format!("redis {}: {error}", redacted(url))))?;

        Ok(Self::build(client, keys, redacted(url)))
    }

    /// The key `key` on the Redis at `url`, with a private certificate
    /// authority or a client certificate.
    ///
    /// The same three settings, spelled the same way, in all seven store
    /// crates — and spelled as *data*, so nothing here names a `redis` type:
    ///
    /// ```no_run
    /// # use dynamic_config_redis::{Redis, TlsConfig};
    /// # fn example() -> Result<(), dynamic_config::Error> {
    /// let redis = Redis::with_tls(
    ///     "rediss://cache.internal:6379",
    ///     "myapp/db.json",
    ///     &TlsConfig::new().with_ca_certificate_file("/etc/ssl/private-ca.pem"),
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Redis expresses all of it: a CA from a file or from bytes, and a client
    /// certificate from either. The credentials still travel in the URL, as
    /// they do for [`new`](Self::new).
    ///
    /// The URL must be `rediss://`. A `redis://` URL with TLS material is a
    /// deployment that believes it is encrypted and is not, so it is refused
    /// here rather than by the client three layers down.
    ///
    /// There is no way to turn verification off; [`TlsConfig`]'s own
    /// documentation argues that one. Redis' client has its own spelling —
    /// the `#insecure` URL fragment, behind a further feature — and it stays
    /// where it is, under its own frightening name.
    ///
    /// # Errors
    ///
    /// If the URL cannot be parsed, if it is not `rediss://`, if a PEM file
    /// cannot be read, or if the material is not PEM. **Not** if the server is
    /// unreachable: the connection is opened on the first read.
    #[cfg(feature = "tls")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
    pub fn with_tls(url: &str, keys: impl Into<Keys>, tls: &TlsConfig) -> Result<Self, Error> {
        use redis::IntoConnectionInfo;

        let described = format!("redis {}", redacted(url));

        let info = url
            .into_connection_info()
            .map_err(|error| Error::remote(format!("{described}: {error}")))?;

        // Refused here rather than by the client, because the client's own
        // refusal arrives as "Constructing a TLS client requires a URL with the
        // `rediss://` scheme" with no address in it — and because a caller who
        // supplied a CA for a plaintext URL has a deployment that believes it
        // is encrypted.
        if !matches!(info.addr(), redis::ConnectionAddr::TcpTls { .. }) {
            return Err(Error::remote(format!(
                "{described}: TLS material was supplied for a URL that is not \
                 `rediss://`; the material is refused rather than ignored"
            )));
        }

        let certificates = redis::TlsCertificates {
            root_cert: tls.ca_certificate_pem(&described)?,
            client_tls: tls
                .client_certificate_pem(&described)?
                .map(|(client_cert, client_key)| redis::ClientTlsConfig {
                    client_cert,
                    client_key,
                }),
        };

        // The client's own error text is deliberately dropped. It embeds
        // `rustls-pki-types`' parse failure, which renders the line it choked
        // on — and the line it choked on in a private key file is private key
        // material. Which of the three is malformed is as far as it is safe to
        // go, and the scheme case above is already ruled out.
        let client = Client::build_with_tls(info, certificates).map_err(|_| {
            Error::remote(format!(
                "{described}: the TLS material was refused; check that the CA \
                 certificate, the client certificate and the private key are \
                 PEM-encoded material of the kind expected"
            ))
        })?;

        Ok(Self::build(client, keys, redacted(url)))
    }

    /// Uses a client the program already has.
    ///
    /// For a caller that already talks to Redis, or one that built its client
    /// with options a URL cannot express — including a TLS configuration
    /// [`with_tls`](Self::with_tls) has no spelling for.
    #[must_use]
    pub fn from_client(client: Client, keys: impl Into<Keys>) -> Self {
        Self::build(client, keys, "<an existing client>".to_owned())
    }

    fn build(client: Client, keys: impl Into<Keys>, described: String) -> Self {
        let keys = keys.into();

        let (format, disagreement) = match documents::agreed_format(keys.named()) {
            Ok(format) => (format, None),
            Err(complaint) => (None, Some(complaint)),
        };

        Self {
            client,
            connection: Mutex::new(None),
            keys,
            format,
            disagreement,
            described,
            timeout: DEFAULT_TIMEOUT,
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

    /// How long a single fetch may take before it is given up on. Ten seconds
    /// by default.
    ///
    /// The deadline for **one fetch attempt**, excluding retries the
    /// underlying client performs — the same sentence every store in this
    /// family answers to. Redis splits it into three, and this sets all of
    /// them from the one value: opening the connection, writing the command,
    /// and waiting for the reply.
    ///
    /// All three, because any one of them alone is the mistake: a deadline
    /// that only covers connecting sails straight past a server that accepted
    /// the socket and then stopped answering, which is what a wedged Redis
    /// actually looks like.
    ///
    /// It bounds each read [`watch`](Self::watch) performs, not the watch
    /// itself — a subscription waiting for the next notification is supposed
    /// to wait.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        // A connection opened under the old deadline carries it in its socket
        // options, so the cached one has to go.
        *self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self
    }

    /// Reports this source's **watch** failures to `sink`.
    ///
    /// A watch loop is the half of a store `dynamic-config` cannot see. A
    /// delivery keeps `RemoteStatus` current because
    /// [`RemoteSink::apply`] records one — but a loop whose subscription
    /// died, or whose re-read keeps failing, delivers nothing and would
    /// otherwise say nothing: `dynamic_config_remote_up` would report the
    /// last *delivery* rather than the last *attempt*, and a Redis that
    /// stopped answering an hour ago would look healthy until something
    /// called `refresh_remote()`.
    ///
    /// ```no_run
    /// # use dynamic_config::RemoteWatch;
    /// # use dynamic_config_redis::Redis;
    /// # struct DbConfig;
    /// # impl DbConfig {
    /// #     fn remote_sink() -> dynamic_config::RemoteSink { unimplemented!() }
    /// # }
    /// # fn example(url: &str) -> Result<(), dynamic_config::Error> {
    /// // Taken once, where the loop is wired: a sink captures the generation
    /// // of the source installed at that moment, which is what stops a loop
    /// // winding down from charging its failures to its replacement.
    /// let sink = DbConfig::remote_sink();
    ///
    /// let watcher = Redis::new(url, "myapp/db.json")?.reporting_to(sink);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// **A failure moves the failure streak and nothing else.** The fetch
    /// count and the clock are left alone, so
    /// `dynamic_config_remote_last_fetch_seconds` keeps ageing while
    /// `dynamic_config_remote_up` goes to zero — the pair an alert wants.
    /// Only the failure's kind and key path are recorded; a Redis URL never
    /// reaches a `RemoteStatus`.
    ///
    /// It changes nothing about what [`watch`](Self::watch) *returns*, and
    /// nothing about [`fetch`](RemoteSource::fetch), which already records
    /// itself through `refresh_remote()`.
    ///
    /// [`RemoteSink::apply`]: dynamic_config::RemoteSink::apply
    #[must_use]
    pub fn reporting_to(mut self, sink: RemoteSink) -> Self {
        self.attempts = Attempts::to(sink);
        self
    }

    /// Reports `error` to whatever asked to hear about failed attempts, and
    /// hands it straight back.
    ///
    /// Every failure that *ends* a watch goes through here, so reporting is
    /// one word at each site rather than a branch that can be left out of the
    /// next one. It cannot fail and it does not touch the error: a loop must
    /// never have to handle a failure to report a failure, and the caller sees
    /// exactly what it always saw.
    fn failing(&self, error: Error) -> Error {
        self.attempts.failed(&error);

        error
    }

    /// Calls `on_change` whenever what this source reads changes.
    ///
    /// Uses **keyspace notifications**: Redis publishes to
    /// `__keyspace@{db}__:{key}` when a key is written, and this subscribes to
    /// exactly that channel — one per key of the set. Genuinely change-driven —
    /// no polling, no timer.
    ///
    /// **One key or a named list.** A named list is the multi-key case Redis
    /// can answer honestly, and the whole reason is `MGET`: it is *one*
    /// command, and Redis executes commands one at a time, so the values it
    /// answers with are the set as of one point in the command stream. The
    /// document delivered here is therefore a state the server really held —
    /// never one key's new value beside another's old one, which is the tear
    /// that made every other network store refuse. A **prefix** is refused;
    /// the reason is on [`Keys::Prefix`].
    ///
    /// What the read is *not* is simultaneous with the notification. It
    /// follows the event, so the document may be **newer** than the write that
    /// woke the loop, and two writes landing together can deliver the later
    /// state rather than each state in turn. **Spurious, never torn** — the
    /// same bargain `dynamic-config-git`'s watch makes, and the one that
    /// matters: a delivery is always a state the store was in, and never an
    /// older one than the delivery before it.
    ///
    /// A named list therefore also **coalesces**: writing three keys together
    /// publishes three notifications and this delivers once, because the
    /// document the second and third would carry is the one already delivered.
    /// Every reload hook running three times for one deployment is a cost with
    /// nothing to buy it.
    ///
    /// Keyspace notifications are **off by default** in Redis. A server that
    /// has not enabled them publishes nothing, and this loop would wait
    /// forever, so it checks at start-up and reports rather than hanging:
    ///
    /// ```text
    /// CONFIG SET notify-keyspace-events KEA
    /// ```
    ///
    /// The current value is not delivered at startup, for the same reason a
    /// file watcher does not report an edit when it starts. Fetch first if the
    /// starting value matters.
    ///
    /// A key that holds nothing is not a change this reports. For one key that
    /// is a deletion; for a named list it is one member of the set going away,
    /// which fails the read the same way [`fetch`](RemoteSource::fetch) does —
    /// and a failed read here is treated as transient, because the next write
    /// notifies again. No configuration is not a configuration, so the running
    /// snapshot stays either way.
    ///
    /// # What a failing loop reports
    ///
    /// Nothing, unless [`reporting_to`](Self::reporting_to) was given a sink.
    /// With one, the two failures **inside** the loop are reported to the
    /// `RemoteStatus` as they happen: a re-read that came back with nothing,
    /// and a subscription that died. The refusals **at the door** — a prefix,
    /// no format, no keys, notifications off, a server that will not accept
    /// the subscription — are not, because they are returned to the caller by
    /// this very call, before there is a loop to be silent in; and half of
    /// them are deployment mistakes rather than a store that stopped
    /// answering, which is not what `dynamic_config_remote_up` means.
    ///
    /// # Errors
    ///
    /// If the subscription cannot be established, if keyspace notifications
    /// are off, if the source reads a prefix, if the subscription itself
    /// breaks — a dead connection ends the watch with an error rather than
    /// spinning; restart it to resubscribe — or if `on_change` returns an
    /// error, so a caller that wants to survive a bad document should log it
    /// and return `Ok`.
    pub fn watch<F>(&self, watching: &Watching, mut on_change: F) -> Result<(), Error>
    where
        F: FnMut(Fetched) -> Result<(), Error>,
    {
        // Validated up front so a key with no format fails at `watch` rather
        // than on the first notification, hours later. The reads themselves go
        // through `fetch`, which resolves the format again.
        // These two are returned and recorded nowhere: nothing has been asked
        // of the server yet, and `reachable()` is *whether the store answered
        // the last time it was asked*. Everything below this line is a round
        // trip, and every one of those reports.
        self.format()?;
        let keys: Vec<String> = self.watched()?.to_vec();

        self.require_keyspace_notifications()
            .map_err(|error| self.failing(error))?;

        // A subscription needs a connection of its own: Redis puts the
        // connection into a mode where ordinary commands are refused.
        let mut subscriber = self
            .client
            .get_connection_with_timeout(self.timeout)
            .map_err(|error| {
                self.failing(Error::remote(format!(
                    "{}: cannot subscribe: {error}",
                    self.describe()
                )))
            })?;

        // The database index is not readable from the client in this version,
        // so it is asked for: `CLIENT INFO` reports the connection's own, which
        // is the one the notifications will be published on. An index that
        // cannot be determined is a hard error, not a guess of `0` — a watch
        // subscribed to the wrong database is a watch that never fires, which
        // reads as "configuration stopped changing" rather than as a failure.
        let database = self.database().ok_or_else(|| {
            self.failing(Error::remote(format!(
                "{}: cannot determine the database index the connection lands on, so the keyspace channel cannot be named",
                self.describe()
            )))
        })?;

        let mut pubsub = subscriber.as_pubsub();

        // One `SUBSCRIBE` per key rather than one carrying the lot: Redis
        // confirms a subscription per channel, and the client reads one
        // confirmation per call — so a single batched command would leave the
        // rest of the confirmations in the buffer to be read later as if they
        // were notifications.
        for key in &keys {
            let channel = format!("__keyspace@{database}__:{key}");

            pubsub.subscribe(&channel).map_err(|error| {
                self.failing(Error::remote(format!(
                    "{}: cannot subscribe: {error}",
                    self.describe()
                )))
            })?;
        }

        // Bounded, so `stop` is noticed without a message having to arrive.
        pubsub.set_read_timeout(Some(POLL_SLICE)).map_err(|error| {
            self.failing(Error::remote(format!("{}: {error}", self.describe())))
        })?;

        // What the last delivery carried, so a set written together is not
        // delivered once per key. `None` for a single key: one write is one
        // notification there, so there is nothing to coalesce, and suppressing
        // a re-write of an identical value would be a change to behaviour that
        // has already shipped.
        let mut last: Option<String> = None;
        let coalescing = keys.len() > 1;

        while watching.keep_going() {
            let message = match pubsub.get_message() {
                Ok(message) => message,
                // A timeout is the design: it is how this loop gets a chance
                // to notice `stop`. Anything else is a broken subscription —
                // and a broken socket returns *immediately*, so treating it
                // as a timeout used to spin this loop at full CPU forever
                // while the handle still looked alive. etcd and NATS end
                // their watch with an error for the same condition; so does
                // this now.
                Err(error) if error.is_timeout() => continue,
                Err(error) => {
                    let error = Error::remote(format!(
                        "{}: the subscription failed: {error}",
                        self.describe()
                    ));

                    // Reported *as well as* returned, and this is the failure
                    // that most needs it: the loop is usually on a thread
                    // nobody joins, so returning here is a watch that stops
                    // and a program that never hears about it. Configuration
                    // simply stops updating, which looks like a store that
                    // has nothing new to say.
                    self.attempts.failed(&error);

                    return Err(error);
                }
            };

            let event: String = message.get_payload().unwrap_or_default();

            // `del` and `expired` mean the key holds nothing. No configuration
            // is not a configuration, so the running snapshot stays.
            if event == "del" || event == "expired" {
                continue;
            }

            // Through `fetch`, not `read`: fetch drops the cached connection
            // on failure, so a read that died with its socket does not leave a
            // dead connection for every later notification to trip over.
            //
            // For a named list that one call is one `MGET`, which is what makes
            // this loop honest: the whole set comes back from a single command,
            // so the document is a state the server was actually in rather than
            // a merge of one key read here and another read a moment later.
            let document = match self.fetch() {
                Ok(document) => document,
                // The notification arrived and the read did not: a transient
                // failure, and the next write will notify again. A member of
                // the set holding nothing lands here too.
                //
                // Reported even so, and deliberately: this call is the whole
                // re-read — one `MGET` for a named list — so a credential the
                // server has started refusing, or a member of the set that
                // has gone missing, is a store answering nothing at every
                // notification while the loop stays alive and silent. The
                // streak is what tells the two apart from a blip: one failure
                // between deliveries clears on the next one, and a store that
                // has really stopped answering climbs.
                Err(error) => {
                    self.attempts.failed(&error);

                    continue;
                }
            };

            if coalescing {
                if last.as_deref() == Some(document.text.as_str()) {
                    continue;
                }

                last = Some(document.text.clone());
            }

            guarded(&mut on_change, document, &self.describe())?;
        }

        Ok(())
    }

    /// The database index this client's connections land on.
    ///
    /// Notifications are published per database, so subscribing to the wrong
    /// one is a watch that never fires.
    fn database(&self) -> Option<i64> {
        let mut connection = self.client.get_connection_with_timeout(self.timeout).ok()?;
        let info: String = redis::cmd("CLIENT")
            .arg("INFO")
            .query(&mut connection)
            .ok()?;

        info.split_whitespace()
            .find_map(|field| field.strip_prefix("db="))
            .and_then(|value| value.parse().ok())
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

    /// The keys a watch subscribes to, or an error saying this source cannot
    /// be watched at all.
    ///
    /// One key and a named list are both watchable, and the property that
    /// decides it is the same one in both cases: the re-read after a
    /// notification is a single `MGET`, and Redis runs one command as one
    /// operation — so there is no window between "something changed" and "read
    /// the set" for a second write to land in. A prefix has to *find* its keys
    /// again first, with a `SCAN`, and a cursor is many commands with writes
    /// free to land between them.
    fn watched(&self) -> Result<&[String], Error> {
        if let Keys::Prefix(_) = &self.keys {
            return Err(Error::remote(format!(
                "{}: a source that reads a prefix cannot be watched; finding \
                 the keys again means a `SCAN`, which is a cursor over many \
                 commands rather than one operation, so the set could be \
                 collected half from before a write and half from after — \
                 name the keys with `Keys::several`, which is watched as one \
                 `MGET`, or poll `refresh_remote()` on a timer instead",
                self.describe()
            )));
        }

        match self.keys.named() {
            // `Keys::several([])` reads nothing, so a watch on it would
            // subscribe to nothing and wait forever — which reads as
            // "configuration stopped changing" rather than as a failure.
            [] => Err(Error::remote(format!(
                "{}: there are no keys to watch",
                self.describe()
            ))),
            keys => Ok(keys),
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

    /// Opens a connection with the deadline applied to all three of its
    /// halves — connecting, writing, and waiting for the reply.
    fn open(&self) -> Result<redis::Connection, Error> {
        let connection = self
            .client
            .get_connection_with_timeout(self.timeout)
            .map_err(|error| self.classified(&error))?;

        // Set after the connection exists, because these are socket options
        // and there is no socket before it. A failure here is not fatal to
        // the read — it means the transport has no timeouts to set, which is
        // true of a unix socket on some platforms — so it is not propagated;
        // the connect deadline above already applied.
        let _ = connection.set_read_timeout(Some(self.timeout));
        let _ = connection.set_write_timeout(Some(self.timeout));

        Ok(connection)
    }

    /// Sorts one of Redis' failures into a kind.
    ///
    /// `NOAUTH`, `WRONGPASS` and `NOPERM` are the server's own words for a
    /// credential it will not accept, and no amount of reconnecting changes
    /// its mind — which is exactly what separates them from the socket
    /// failures that share this path.
    fn classified(&self, error: &redis::RedisError) -> Error {
        let described = format!("{}: {error}", self.describe());

        if error.kind() == redis::ErrorKind::AuthenticationFailed
            || matches!(error.code(), Some("NOAUTH" | "WRONGPASS" | "NOPERM"))
        {
            return Error::auth(described);
        }

        Error::remote(described)
    }

    /// Reads whatever this source reads, opening the connection if this is the
    /// first time.
    ///
    /// Every shape ends in one `MGET`: Redis reads a set of keys as a single
    /// operation, so a document assembled from several keys was never half of
    /// one set and half of another. Only *finding* the keys differs, and only
    /// the prefix has to.
    fn read(&self) -> Result<Vec<(String, String)>, Error> {
        let mut slot = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let connection = match slot.as_mut() {
            Some(connection) => connection,
            None => slot.insert(self.open()?),
        };

        let keys = match &self.keys {
            Keys::One(key) => vec![key.clone()],
            Keys::Several(keys) => keys.clone(),
            Keys::Prefix(prefix) => self.scan(connection, prefix)?,
        };

        // An `MGET` with no keys is a protocol error, and a prefix that matched
        // nothing is a missing configuration rather than an empty one.
        if keys.is_empty() {
            return Err(Error::remote(format!(
                "{}: nothing matched, so there is nothing to load",
                self.describe()
            )));
        }

        let values: Vec<Option<String>> = connection.mget(&keys).map_err(|error| {
            // The connection may be the thing that broke; `fetch` drops it so
            // the next read opens a fresh one rather than reusing a dead
            // socket.
            self.classified(&error)
        })?;

        keys.into_iter()
            .zip(values)
            .map(|(key, value)| {
                // Fail-whole, not merge-what-came-back: a configuration
                // quietly missing a section is worse than a refresh that
                // failed and left the last document serving.
                let text = value.ok_or_else(|| {
                    Error::remote(format!("{}: `{key}` holds no value", self.describe()))
                })?;

                Ok((key, text))
            })
            .collect()
    }

    /// Every key under `prefix`, found with `SCAN` and never with `KEYS`.
    ///
    /// `KEYS` is one blocking operation over the whole key space, which is the
    /// classic way to stall a production Redis; `SCAN` is a cursor, and the
    /// cost of the cursor is that the set is not read atomically.
    ///
    /// Sorted before it is returned, so the same keys produce the same
    /// document and the same diagnostic — `SCAN` returns them in hash-table
    /// order, which is neither stable nor anybody's precedence. Sorting is not
    /// a precedence rule either, which is exactly why an overlap under a prefix
    /// is refused rather than resolved.
    fn scan(&self, connection: &mut redis::Connection, prefix: &str) -> Result<Vec<String>, Error> {
        let pattern = format!("{}*", globbed(prefix));

        let mut cursor = 0_u64;
        let mut found: Vec<String> = Vec::new();

        for _ in 0..MOST_SCAN_ROUNDS {
            let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(SCAN_BATCH)
                .query(connection)
                .map_err(|error| self.classified(&error))?;

            for key in batch {
                // The escaping above should make this unreachable, and it is
                // one comparison: a prefix has to mean the prefix whatever a
                // proxy, a cluster or a future glob syntax makes of the
                // pattern.
                documents::under_prefix(&key, prefix, &self.describe())?;

                found.push(key);
            }

            // A cursor can hand the same key back more than once — it is a
            // guarantee about *coverage*, not about uniqueness — and the same
            // key twice would collide with itself under the prefix rule.
            found.sort();
            found.dedup();

            documents::within_key_budget(found.len(), &self.describe())?;

            cursor = next;

            if cursor == 0 {
                return Ok(found);
            }
        }

        Err(Error::remote(format!(
            "{}: the scan did not finish in {MOST_SCAN_ROUNDS} rounds; \
             the server is not advancing the cursor",
            self.describe()
        )))
    }

    /// Reports if the server will never publish what the watch waits for.
    fn require_keyspace_notifications(&self) -> Result<(), Error> {
        let mut connection = self.open()?;

        let settings: Vec<String> = redis::cmd("CONFIG")
            .arg("GET")
            .arg("notify-keyspace-events")
            .query(&mut connection)
            .map_err(|error| self.classified(&error))?;

        let value = settings.get(1).map(String::as_str).unwrap_or_default();

        // `K` is the keyspace class; without it nothing is published on the
        // channel this subscribes to, whatever else is enabled.
        if value.contains('K') {
            return Ok(());
        }

        Err(Error::remote(format!(
            "{}: keyspace notifications are off, so nothing would ever arrive; \
             `CONFIG SET notify-keyspace-events KEA` on the server",
            self.describe()
        )))
    }
}

impl RemoteSource for Redis {
    fn fetch(&self) -> Result<Fetched, Error> {
        let format = self.format()?;

        // A named list is read in call order and a prefix scan is sorted, which
        // is what each rule wants — so nothing is reordered here.
        match self.read().and_then(|documents| {
            documents::merged(&documents, format, self.overlap(), &self.describe())
        }) {
            Ok(document) => Ok(document),
            Err(error) => {
                // A failed read may have been a dead connection; drop it so the
                // next attempt opens a fresh one.
                *self
                    .connection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

                Err(error)
            }
        }
    }

    fn describe(&self) -> String {
        format!("redis {} {}", self.described, self.keys.describe())
    }
}

impl std::fmt::Debug for Redis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redis")
            .field("server", &self.described)
            .field("keys", &self.keys)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

/// A literal string, escaped so `SCAN MATCH` reads it as itself.
///
/// The one place a prefix could quietly stop meaning what it says: `MATCH`
/// takes a glob, so a prefix containing `[` or `*` — a tenant id, a key
/// namespace with brackets in it — would match keys the caller never named.
/// Redis' own glob escape is a backslash, and the backslash itself has to go
/// first or it would escape the escapes.
fn globbed(literal: &str) -> String {
    let mut escaped = String::with_capacity(literal.len());

    for character in literal.chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | ']') {
            escaped.push('\\');
        }

        escaped.push(character);
    }

    escaped
}

/// A URL with its password removed, for error messages.
///
/// `redis://user:hunter2@host` in a log is a credential in a log.
///
/// [`LoneAuthority::Username`] is the Redis-specific half: `redis://app@host`
/// names a user with the password elsewhere, so blanking the authority would
/// hide the half worth seeing. The NATS crate reads the same shape as a
/// token, which is why the two pass different arguments to one
/// implementation rather than keeping two.
fn redacted(url: &str) -> String {
    dynamic_config_store_core::redacted(url, LoneAuthority::Username)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn a_password_never_reaches_an_error_message() {
        assert_eq!(
            redacted("redis://app:hunter2@redis.internal:6379"),
            "redis://app:***@redis.internal:6379"
        );
    }

    #[test]
    fn a_password_containing_at_signs_is_fully_redacted() {
        assert_eq!(
            redacted("redis://app:p@ss@w@rd@redis.internal:6379"),
            "redis://app:***@redis.internal:6379"
        );
    }

    #[test]
    fn a_url_with_no_credentials_is_left_alone() {
        assert_eq!(
            redacted("redis://redis.internal:6379"),
            "redis://redis.internal:6379"
        );
        assert_eq!(redacted("not a url"), "not a url");
    }

    #[test]
    fn the_format_comes_from_the_keys_extension() {
        let client = Client::open("redis://127.0.0.1:6379").unwrap();
        let source = Redis::from_client(client, "myapp/db.json");

        assert_eq!(source.format, Some(Format::Json));
    }

    /// A bare string still means one key, which is what keeps every caller who
    /// wrote the single-key spelling compiling.
    #[test]
    fn a_bare_key_is_still_one_key() {
        assert_eq!(Keys::from("myapp/db.json"), Keys::one("myapp/db.json"));
        assert_eq!(
            Keys::from("myapp/db.json".to_owned()),
            Keys::one("myapp/db.json")
        );
    }

    /// The one place a prefix could quietly stop meaning what it says: `MATCH`
    /// takes a glob, so a tenant id with a bracket in it would otherwise
    /// select keys nobody asked for.
    #[test]
    fn a_prefix_is_matched_as_a_literal_and_not_as_a_glob() {
        assert_eq!(globbed("myapp/"), "myapp/");
        assert_eq!(globbed("my[a]pp/"), r"my\[a\]pp/");
        assert_eq!(globbed("my*pp/"), r"my\*pp/");
        assert_eq!(globbed("my?pp/"), r"my\?pp/");
        // The backslash goes first, or it would escape the escapes.
        assert_eq!(globbed(r"my\pp/"), r"my\\pp/");
    }

    /// Provenance is store-grained once several keys become one document, so
    /// the one thing `describe()` can still do is name the whole set.
    #[test]
    fn describe_names_every_key_in_the_set() {
        let client = Client::open("redis://127.0.0.1:6379").unwrap();

        let several = Redis::from_client(client.clone(), Keys::several(["a.json", "b.json"]));
        assert!(
            several.describe().contains("a.json") && several.describe().contains("b.json"),
            "{}",
            several.describe()
        );

        let prefix = Redis::from_client(client, Keys::prefix("myapp/"));
        assert!(
            prefix.describe().contains("prefix myapp/"),
            "{}",
            prefix.describe()
        );
    }

    /// Both refuse before any round trip, and both name the call that settles
    /// it — a misconfiguration should not need a server to be reported.
    #[test]
    fn a_format_that_cannot_be_inferred_is_refused_before_any_request() {
        let client = Client::open("redis://127.0.0.1:9").unwrap();

        let error = Redis::from_client(client.clone(), Keys::prefix("myapp/"))
            .fetch()
            .expect_err("a prefix names no format");

        assert!(error.to_string().contains("with_format"), "{error}");

        let error = Redis::from_client(client, Keys::several(["db.json", "server.toml"]))
            .fetch()
            .expect_err("json and toml cannot both be it");

        assert!(error.to_string().contains("db.json"), "{error}");
        assert!(error.to_string().contains("server.toml"), "{error}");
    }

    /// The line between the two multi-key shapes, drawn where the protocol
    /// draws it: a named list is re-read with one `MGET` and is watched, and a
    /// prefix has to find its keys again with a `SCAN` — many commands, with
    /// writes free to land between them — and is refused before any round
    /// trip, naming the call that does work.
    #[test]
    fn a_prefix_refuses_to_be_watched_and_a_named_list_is_not_refused_with_it() {
        let client = Client::open("redis://127.0.0.1:9").unwrap();
        let source =
            Redis::from_client(client.clone(), Keys::prefix("myapp/")).with_format(Format::Json);

        let watch = dynamic_config::RemoteWatch::new();
        let error = source
            .watch(&watch.watching(), |_| Ok(()))
            .expect_err("a prefix cannot be watched");

        assert!(error.to_string().contains("cannot be watched"), "{error}");
        assert!(error.to_string().contains("SCAN"), "{error}");
        assert!(error.to_string().contains("Keys::several"), "{error}");

        // Nothing is listening on port 9, so a named list gets as far as the
        // network and fails there — which is the point: it is not turned away
        // at the door the way the prefix is.
        let source = Redis::from_client(client, Keys::several(["a.json", "b.json"]));
        let error = source
            .watch(&watch.watching(), |_| Ok(()))
            .expect_err("nothing is listening");

        assert!(
            !error.to_string().contains("cannot be watched"),
            "a named list is watchable: {error}"
        );
    }

    /// Every error path a watch can take names the source, and naming the
    /// source means quoting the URL — so each of them gets the redaction test
    /// the fetch paths have. The password may contain `@`, which is the shape
    /// that has caught this crate out before.
    #[test]
    fn no_watch_error_path_prints_a_credential() {
        const URL: &str = "redis://app:p@ss@w@rd@127.0.0.1:9";

        let watch = dynamic_config::RemoteWatch::new();

        let refusals = [
            // Refused at the door: the prefix shape.
            Redis::new(URL, Keys::prefix("myapp/"))
                .unwrap()
                .with_format(Format::Json),
            // Refused at the door: nothing to subscribe to.
            Redis::new(URL, Keys::several(Vec::<String>::new()))
                .unwrap()
                .with_format(Format::Json),
            // Refused by the network: nothing is listening on port 9, so this
            // fails while checking for keyspace notifications.
            Redis::new(URL, Keys::several(["myapp/db.json", "myapp/server.json"])).unwrap(),
        ];

        for source in refusals {
            let error = source
                .watch(&watch.watching(), |_| Ok(()))
                .expect_err("every one of these refuses");

            let printed = format!("{error} {error:?} {source:?}");

            assert!(!printed.contains("p@ss"), "{printed}");
            assert!(!printed.contains("w@rd"), "{printed}");
            assert!(printed.contains("app:***@"), "{printed}");
        }
    }

    /// `Keys::several([])` reads nothing, so a watch on it would subscribe to
    /// nothing and wait forever — which reads as "configuration stopped
    /// changing" rather than as a failure.
    #[test]
    fn a_watch_on_an_empty_named_list_is_refused_rather_than_parked_forever() {
        let client = Client::open("redis://127.0.0.1:9").unwrap();
        let source = Redis::from_client(client, Keys::several(Vec::<String>::new()))
            .with_format(Format::Json);

        let watch = dynamic_config::RemoteWatch::new();
        let error = source
            .watch(&watch.watching(), |_| Ok(()))
            .expect_err("there is nothing to subscribe to");

        assert!(error.to_string().contains("no keys to watch"), "{error}");
    }

    /// A listener that answers every command with `reply`, until the client
    /// hangs up.
    ///
    /// No Docker: a RESP error is one line, which is all a refusal needs. One
    /// reply *per command* rather than per read, because `redis-rs` pipelines
    /// its two `CLIENT SETINFO` handshake commands into a single write and
    /// then waits for both — answer once and the connection times out instead
    /// of failing the way the test means to test.
    fn scripted(reply: &'static str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("redis://{}", listener.local_addr().unwrap());

        let server = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };

            let mut buffer = [0u8; 4096];

            while let Ok(read) = stream.read(&mut buffer) {
                if read == 0 {
                    return;
                }

                // Every RESP command is an array, and every array opens with
                // `*`, so the count of those lines is the count of replies
                // owed.
                let commands = String::from_utf8_lossy(&buffer[..read])
                    .lines()
                    .filter(|line| line.starts_with('*'))
                    .count();

                for _ in 0..commands.max(1) {
                    if stream.write_all(reply.as_bytes()).is_err() {
                        return;
                    }
                }
            }
        });

        (url, server)
    }

    /// A password the server will not take is `Auth`: reconnecting with the
    /// same one is not a recovery, and a watch loop should stop rather than
    /// spin.
    #[test]
    fn a_refused_password_is_an_auth_failure() {
        let (url, server) = scripted("-WRONGPASS invalid username-password pair\r\n");

        let source = Redis::new(&url, "myapp/db.json").unwrap();
        let error = source.fetch().expect_err("the server refused the password");

        drop(source);
        let _ = server.join();

        assert_eq!(error.kind(), dynamic_config::ErrorKind::Auth, "{error}");
    }

    /// The same, for the server that wants a password and was given none.
    #[test]
    fn a_missing_password_is_an_auth_failure() {
        let (url, server) = scripted("-NOAUTH Authentication required.\r\n");

        let source = Redis::new(&url, "myapp/db.json").unwrap();
        let error = source.fetch().expect_err("the server wants a password");

        drop(source);
        let _ = server.join();

        assert_eq!(error.kind(), dynamic_config::ErrorKind::Auth, "{error}");
    }

    /// The over-classification that would cost the most: a store that is
    /// simply not there stays `Remote`, so a watch loop backs off and
    /// recovers when it comes back.
    #[test]
    fn an_unreachable_server_is_remote_rather_than_auth() {
        // Port 9 is discard; nothing listens there.
        let source = Redis::new("redis://app:hunter2@127.0.0.1:9", "myapp/db.json").unwrap();

        let error = source.fetch().expect_err("nothing is listening");

        assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
        assert!(
            !error.to_string().contains("hunter2"),
            "the password must not reach the message: {error}"
        );
    }

    // -----------------------------------------------------------------------
    // Several keys as one document, against a scripted server
    // -----------------------------------------------------------------------

    /// A listener that answers each command by its verb, and records the
    /// commands it was sent.
    ///
    /// Verb-keyed rather than positional: `redis-rs` opens a connection with a
    /// handshake whose length is its business, and a test that counted replies
    /// would break the day it sends one more. Anything not in `replies` gets
    /// `+OK`, which is what the handshake wants anyway.
    /// Every command a scripted server was sent, as its arguments.
    type Asked = Arc<std::sync::Mutex<Vec<Vec<String>>>>;

    fn by_verb(
        replies: Vec<(&'static str, Vec<String>)>,
    ) -> (String, Asked, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("redis://{}", listener.local_addr().unwrap());
        let asked: Asked = Arc::default();

        let seen = Arc::clone(&asked);
        let server = std::thread::spawn(move || {
            let mut queued: std::collections::HashMap<&str, std::collections::VecDeque<String>> =
                replies
                    .into_iter()
                    .map(|(verb, answers)| (verb, answers.into_iter().collect()))
                    .collect();

            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };

            let mut buffer = [0u8; 8192];

            while let Ok(read) = stream.read(&mut buffer) {
                if read == 0 {
                    return;
                }

                for command in parsed(&String::from_utf8_lossy(&buffer[..read])) {
                    let verb = command[0].to_ascii_uppercase();

                    seen.lock().unwrap().push(command.clone());

                    let reply = queued
                        .get_mut(verb.as_str())
                        .and_then(std::collections::VecDeque::pop_front)
                        .unwrap_or_else(|| "+OK\r\n".to_owned());

                    if stream.write_all(reply.as_bytes()).is_err() {
                        return;
                    }
                }
            }
        });

        (url, asked, server)
    }

    /// Every command in a (possibly pipelined) RESP write, as its arguments.
    fn parsed(text: &str) -> Vec<Vec<String>> {
        let lines: Vec<&str> = text.split("\r\n").collect();
        let mut commands = Vec::new();
        let mut at = 0;

        while at < lines.len() {
            let Some(count) = lines[at]
                .strip_prefix('*')
                .and_then(|n| n.parse::<usize>().ok())
            else {
                at += 1;
                continue;
            };

            // Each argument is a `$len` line and a value line, so the command
            // occupies exactly `2 * count` lines after its header.
            let arguments: Vec<String> = (0..count)
                .filter_map(|n| lines.get(at + 2 + n * 2).map(|value| (*value).to_owned()))
                .collect();

            if !arguments.is_empty() {
                commands.push(arguments);
            }

            at += 1 + count * 2;
        }

        commands
    }

    /// A RESP array of bulk strings, `None` becoming a nil.
    fn resp_array(values: &[Option<&str>]) -> String {
        let mut encoded = format!("*{}\r\n", values.len());

        for value in values {
            match value {
                Some(text) => encoded.push_str(&format!("${}\r\n{text}\r\n", text.len())),
                None => encoded.push_str("$-1\r\n"),
            }
        }

        encoded
    }

    /// One `SCAN` reply: the next cursor, and the keys in this round.
    fn resp_scan(cursor: &str, keys: &[&str]) -> String {
        let keys: Vec<Option<&str>> = keys.iter().map(|key| Some(*key)).collect();

        format!(
            "*2\r\n${}\r\n{cursor}\r\n{}",
            cursor.len(),
            resp_array(&keys)
        )
    }

    /// A named list is one `MGET` — one command, one round trip, one operation
    /// on the server — and the later key wins where two of them meet.
    #[test]
    fn a_named_list_is_one_mget_in_call_order() {
        let (url, asked, server) = by_verb(vec![(
            "MGET",
            vec![resp_array(&[
                Some(r#"{"db": {"host": "base", "port": 5432}}"#),
                Some(r#"{"db": {"port": 6432}}"#),
            ])],
        )]);

        let source =
            Redis::new(&url, Keys::several(["myapp/base.json", "myapp/local.json"])).unwrap();
        let fetched = source.fetch().expect("both keys answered");

        drop(source);
        let _ = server.join();

        let asked = asked.lock().unwrap();
        let mget = asked
            .iter()
            .find(|command| command[0].eq_ignore_ascii_case("MGET"))
            .expect("one MGET");

        assert_eq!(
            mget[1..],
            ["myapp/base.json".to_owned(), "myapp/local.json".to_owned()],
            "the caller's order is the merge order"
        );
        assert!(
            !asked
                .iter()
                .any(|command| command[0].eq_ignore_ascii_case("GET")),
            "a named list must not become one GET per key: {asked:?}"
        );

        let merged = dynamic_config::Value::parse(&fetched.text, Format::Json).unwrap();

        assert_eq!(
            merged.get("db.host"),
            Some(&dynamic_config::Value::String("base".to_owned())),
            "a key the later document never mentions survives"
        );
        assert_eq!(
            merged.get("db.port"),
            Some(&dynamic_config::Value::Integer(6432)),
            "and the later document wins where they meet"
        );
    }

    /// `SCAN`, never `KEYS`: `KEYS` walks the whole key space in one blocking
    /// operation, which is the classic way to stall a production server. The
    /// cursor is followed to the end, and a key handed back twice is folded.
    #[test]
    fn a_prefix_scans_in_rounds_and_never_asks_for_keys() {
        let (url, asked, server) = by_verb(vec![
            (
                "SCAN",
                vec![
                    resp_scan("17", &["myapp/db.json"]),
                    // The same key again: a cursor guarantees coverage, not
                    // uniqueness, and the duplicate would collide with itself
                    // under the disjoint rule.
                    resp_scan("0", &["myapp/server.json", "myapp/db.json"]),
                ],
            ),
            (
                "MGET",
                vec![resp_array(&[
                    Some(r#"{"db": {"host": "db.internal"}}"#),
                    Some(r#"{"server": {"port": 8080}}"#),
                ])],
            ),
        ]);

        let source = Redis::new(&url, Keys::prefix("myapp/"))
            .unwrap()
            .with_format(Format::Json);
        let fetched = source.fetch().expect("the scan finished");

        drop(source);
        let _ = server.join();

        let asked = asked.lock().unwrap();
        let scans: Vec<&Vec<String>> = asked
            .iter()
            .filter(|command| command[0].eq_ignore_ascii_case("SCAN"))
            .collect();

        assert_eq!(scans.len(), 2, "the cursor is followed: {asked:?}");
        assert_eq!(scans[0][1], "0", "the first round starts at zero");
        assert_eq!(scans[1][1], "17", "and the next carries the cursor back");
        assert!(
            scans[0].iter().any(|argument| argument == "myapp/*"),
            "the prefix goes out as a literal with one trailing star: {scans:?}"
        );
        assert!(
            !asked
                .iter()
                .any(|command| command[0].eq_ignore_ascii_case("KEYS")),
            "KEYS blocks a production server: {asked:?}"
        );

        let mget = asked
            .iter()
            .find(|command| command[0].eq_ignore_ascii_case("MGET"))
            .expect("one MGET for the whole set");

        assert_eq!(
            mget[1..],
            ["myapp/db.json".to_owned(), "myapp/server.json".to_owned()],
            "sorted and deduplicated, so the same keys give the same document"
        );

        let merged = dynamic_config::Value::parse(&fetched.text, Format::Json).unwrap();

        assert_eq!(
            merged.get("db.host"),
            Some(&dynamic_config::Value::String("db.internal".to_owned()))
        );
        assert_eq!(
            merged.get("server.port"),
            Some(&dynamic_config::Value::Integer(8080))
        );
    }

    /// A prefix says "these are disjoint sections". Two of them supplying one
    /// path is a deployment bug, named rather than resolved — by path, never
    /// by value.
    #[test]
    fn two_keys_under_a_prefix_supplying_one_path_are_refused_by_name() {
        let (url, _asked, server) = by_verb(vec![
            (
                "SCAN",
                vec![resp_scan("0", &["myapp/a.json", "myapp/b.json"])],
            ),
            (
                "MGET",
                vec![resp_array(&[
                    Some(r#"{"db": {"password": "hunter2-first"}}"#),
                    Some(r#"{"db": {"password": "hunter2-second"}}"#),
                ])],
            ),
        ]);

        let source = Redis::new(&url, Keys::prefix("myapp/"))
            .unwrap()
            .with_format(Format::Json);
        let error = source.fetch().expect_err("both keys supply db.password");

        drop(source);
        let _ = server.join();

        let printed = format!("{error} {error:?}");

        assert!(printed.contains("myapp/a.json"), "{printed}");
        assert!(printed.contains("myapp/b.json"), "{printed}");
        assert!(printed.contains("db.password"), "{printed}");
        assert!(
            !printed.contains("hunter2"),
            "a collision report names paths and never values: {printed}"
        );
    }

    /// Fail-whole, not merge-what-came-back: `MGET` answers a missing key with
    /// a nil, and four sections out of five is a configuration with a hole in
    /// it that nobody would notice.
    #[test]
    fn one_key_holding_nothing_fails_the_whole_fetch_and_names_it() {
        let (url, _asked, server) = by_verb(vec![(
            "MGET",
            vec![resp_array(&[Some(r#"{"db": {"host": "here"}}"#), None])],
        )]);

        let source =
            Redis::new(&url, Keys::several(["myapp/db.json", "myapp/absent.json"])).unwrap();
        let error = source.fetch().expect_err("the second key is not there");

        drop(source);
        let _ = server.join();

        assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
        assert!(error.to_string().contains("myapp/absent.json"), "{error}");
        assert!(error.to_string().contains("holds no value"), "{error}");
    }

    /// A key the server answers with that is not under the prefix asked for is
    /// refused rather than merged — the check the glob escaping should make
    /// unreachable, kept because a prefix has to mean the prefix.
    #[test]
    fn a_key_outside_the_prefix_is_refused() {
        let (url, _asked, server) = by_verb(vec![(
            "SCAN",
            vec![resp_scan("0", &["myapp/db.json", "other/db.json"])],
        )]);

        let source = Redis::new(&url, Keys::prefix("myapp/"))
            .unwrap()
            .with_format(Format::Json);
        let error = source.fetch().expect_err("one key escaped the prefix");

        drop(source);
        let _ = server.join();

        assert!(error.to_string().contains("other/db.json"), "{error}");
    }

    /// A cursor is server state, and a server that never advances it would
    /// otherwise be an infinite loop inside a fetch — one the key budget
    /// cannot catch, because a scan can return nothing at all round after
    /// round.
    #[test]
    fn a_cursor_that_never_advances_ends_the_scan_rather_than_the_process() {
        let (url, _asked, server) = by_verb(vec![(
            "SCAN",
            // Far more than the loop will take, all of them going nowhere.
            std::iter::repeat_with(|| resp_scan("17", &[]))
                .take(MOST_SCAN_ROUNDS + 8)
                .collect(),
        )]);

        let source = Redis::new(&url, Keys::prefix("myapp/"))
            .unwrap()
            .with_format(Format::Json);
        let error = source
            .fetch()
            .expect_err("the cursor never comes back to 0");

        drop(source);
        let _ = server.join();

        assert!(
            error.to_string().contains("advancing the cursor"),
            "{error}"
        );
    }

    /// A prefix that matched nothing is a missing configuration, not an empty
    /// one — and an `MGET` with no keys is a protocol error, so it must not be
    /// sent either.
    #[test]
    fn a_prefix_that_matches_nothing_is_a_failure_rather_than_an_empty_document() {
        let (url, asked, server) = by_verb(vec![("SCAN", vec![resp_scan("0", &[])])]);

        let source = Redis::new(&url, Keys::prefix("myapp/"))
            .unwrap()
            .with_format(Format::Json);
        let error = source.fetch().expect_err("nothing matched");

        drop(source);
        let _ = server.join();

        assert!(error.to_string().contains("nothing matched"), "{error}");
        assert!(
            !asked
                .lock()
                .unwrap()
                .iter()
                .any(|command| command[0].eq_ignore_ascii_case("MGET")),
            "an MGET with no keys is a protocol error"
        );
    }

    /// The credential rule at the new error paths: a password must not be
    /// quoted back in a complaint about a set of keys.
    #[test]
    fn no_multi_key_error_path_prints_a_credential() {
        let source = Redis::new(
            "redis://app:hunter2@127.0.0.1:9",
            Keys::several(["myapp/db.json", "myapp/second.json"]),
        )
        .unwrap();

        let error = source.fetch().expect_err("nothing is listening");
        let printed = format!("{error} {error:?} {source:?}");

        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("myapp/second.json"), "{printed}");
    }

    /// The mistake the deadline exists to prevent: a server that accepts the
    /// socket and then never answers. A connect-only timeout sails past this.
    #[test]
    fn a_read_from_a_server_that_never_answers_ends_at_the_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("redis://{}", listener.local_addr().unwrap());

        let silent = std::thread::spawn(move || {
            let held = listener.accept();
            std::thread::sleep(Duration::from_secs(2));
            drop(held);
        });

        let source = Redis::new(&url, "myapp/db.json")
            .unwrap()
            .with_timeout(Duration::from_millis(200));

        let started = std::time::Instant::now();
        let error = source.fetch().expect_err("nothing ever answers");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "the deadline must bound the read, not merely the connect: {elapsed:?}"
        );
        assert_eq!(
            error.kind(),
            dynamic_config::ErrorKind::Remote,
            "a store that went quiet may yet come back: {error}"
        );

        let _ = silent.join();
    }
    // -----------------------------------------------------------------------
    // Watching a named list, against a scripted server
    // -----------------------------------------------------------------------

    /// A bulk string.
    fn resp_bulk(text: &str) -> String {
        format!("${}\r\n{text}\r\n", text.len())
    }

    /// A scripted server that accepts several connections at once and answers
    /// by verb and subcommand.
    ///
    /// Several, because one watch opens four: the notification check, the
    /// database probe, the subscriber, and the connection the reads reuse. The
    /// single-connection server the fetch tests use would deadlock on the
    /// second.
    ///
    /// Once every expected channel has been subscribed to it does what `then`
    /// says.
    fn subscribable(
        channels: usize,
        mget: Vec<String>,
        then: Then,
    ) -> (String, Asked, Arc<std::sync::atomic::AtomicBool>) {
        use std::sync::atomic::{AtomicBool, Ordering};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("redis://{}", listener.local_addr().unwrap());
        let asked: Asked = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));

        listener.set_nonblocking(true).unwrap();

        let seen = Arc::clone(&asked);
        let stopping = Arc::clone(&stop);
        let queued: Arc<Mutex<std::collections::VecDeque<String>>> =
            Arc::new(Mutex::new(mget.into_iter().collect()));

        std::thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // An accepted socket does not reliably inherit the
                        // listener's blocking mode, and a scripted server that
                        // read `WouldBlock` as end-of-connection would answer
                        // nothing at all.
                        stream.set_nonblocking(false).unwrap();

                        let seen = Arc::clone(&seen);
                        let queued = Arc::clone(&queued);

                        std::thread::spawn(move || serve(stream, &seen, &queued, channels, then));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });

        (url, asked, stop)
    }

    /// What [`subscribable`] does once every expected channel has been
    /// subscribed to.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Then {
        /// Publishes one notification **per channel**, which is what one
        /// `MSET` over the set really does — so this is also the shape that
        /// would catch a set being delivered once per key.
        Publishes,
        /// Closes the subscriber's connection without a word, which is what a
        /// server restart, a `CLIENT KILL` or a proxy dropping the connection
        /// looks like from inside the loop.
        HangsUp,
    }

    /// One connection of [`subscribable`].
    fn serve(
        mut stream: std::net::TcpStream,
        seen: &Asked,
        queued: &Mutex<std::collections::VecDeque<String>>,
        channels: usize,
        then: Then,
    ) {
        use std::io::{Read, Write};

        let mut buffer = [0u8; 8192];
        let mut subscribed: Vec<String> = Vec::new();

        while let Ok(read) = stream.read(&mut buffer) {
            if read == 0 {
                return;
            }

            for command in parsed(&String::from_utf8_lossy(&buffer[..read])) {
                seen.lock().unwrap().push(command.clone());

                let verb = command[0].to_ascii_uppercase();
                let sub = command
                    .get(1)
                    .map(|argument| argument.to_ascii_uppercase())
                    .unwrap_or_default();

                let reply = match (verb.as_str(), sub.as_str()) {
                    ("CLIENT", "INFO") => {
                        resp_bulk("id=4 addr=127.0.0.1:1 laddr=127.0.0.1:2 fd=8 name= db=0 age=0")
                    }
                    ("CONFIG", "GET") => resp_array(&[Some("notify-keyspace-events"), Some("KEA")]),
                    ("SUBSCRIBE", _) => {
                        subscribed.push(command[1].clone());

                        format!(
                            "*3\r\n{}{}:{}\r\n",
                            resp_bulk("subscribe"),
                            resp_bulk(&command[1]),
                            subscribed.len()
                        )
                    }
                    ("MGET", _) => queued
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| "*-1\r\n".to_owned()),
                    _ => "+OK\r\n".to_owned(),
                };

                if stream.write_all(reply.as_bytes()).is_err() {
                    return;
                }

                if verb == "SUBSCRIBE" && subscribed.len() == channels {
                    if then == Then::HangsUp {
                        return;
                    }

                    for channel in &subscribed {
                        let published = format!(
                            "*3\r\n{}{}{}",
                            resp_bulk("message"),
                            resp_bulk(channel),
                            resp_bulk("set")
                        );

                        if stream.write_all(published.as_bytes()).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }

    /// The two halves of the claim that a named list can be watched honestly:
    /// the loop hears about **every** key of the set, and the re-read that
    /// follows is **one `MGET`** — one command, so one operation on the server.
    ///
    /// A watch that subscribed to the first key only would go quiet the moment
    /// a deployment changed the second; one that re-read key by key would
    /// deliver a document the server never held.
    #[test]
    fn a_named_list_subscribes_to_every_key_and_re_reads_the_set_with_one_mget() {
        let answer = resp_array(&[
            Some(r#"{"db": {"host": "base", "port": 5432}}"#),
            Some(r#"{"db": {"port": 6432}}"#),
        ]);
        let (url, asked, stop) = subscribable(2, vec![answer.clone(), answer], Then::Publishes);

        let source =
            Redis::new(&url, Keys::several(["myapp/base.json", "myapp/local.json"])).unwrap();

        let watch = dynamic_config::RemoteWatch::new();
        let watching = watch.watching();
        let (sender, receiver) = std::sync::mpsc::channel();

        let loops = std::thread::spawn(move || {
            source.watch(&watching, move |document| {
                let _ = sender.send(document.text);
                Ok(())
            })
        });

        let text = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the notification should reach the callback");

        let merged = dynamic_config::Value::parse(&text, Format::Json).unwrap();

        assert_eq!(
            merged.get("db.host"),
            Some(&dynamic_config::Value::String("base".to_owned())),
            "the whole set is delivered, not the key that changed"
        );
        assert_eq!(
            merged.get("db.port"),
            Some(&dynamic_config::Value::Integer(6432)),
            "and the caller's order is still the merge order"
        );

        // Both notifications have to have been *processed* before the
        // coalescing claim below means anything, and the second `MGET` is
        // what says so.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);

        while std::time::Instant::now() < deadline {
            let mgets = asked
                .lock()
                .unwrap()
                .iter()
                .filter(|command| command[0].eq_ignore_ascii_case("MGET"))
                .count();

            if mgets >= 2 {
                break;
            }

            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(
            receiver.recv_timeout(Duration::from_millis(500)).is_err(),
            "a set written together publishes once per key and is one document; \
             delivering it per key would run every reload hook per key"
        );

        watch.stop();
        stop.store(true, std::sync::atomic::Ordering::Release);

        let outcome = loops.join().expect("the loop should end");

        assert!(outcome.is_ok(), "{outcome:?}");

        let asked = asked.lock().unwrap();
        let subscribed: Vec<&String> = asked
            .iter()
            .filter(|command| command[0].eq_ignore_ascii_case("SUBSCRIBE"))
            .map(|command| &command[1])
            .collect();

        assert_eq!(
            subscribed,
            [
                "__keyspace@0__:myapp/base.json",
                "__keyspace@0__:myapp/local.json"
            ],
            "every key of the set is subscribed to, on the database the \
             connection lands on: {asked:?}"
        );

        let mget = asked
            .iter()
            .find(|command| command[0].eq_ignore_ascii_case("MGET"))
            .expect("the set is re-read with one MGET");

        assert_eq!(
            mget[1..],
            ["myapp/base.json".to_owned(), "myapp/local.json".to_owned()],
            "one command carrying the whole set is what makes the delivery a \
             state the server really had"
        );
        assert!(
            !asked
                .iter()
                .any(|command| command[0].eq_ignore_ascii_case("GET")),
            "a re-read key by key is the torn document this watch exists to \
             avoid: {asked:?}"
        );
        assert!(
            !asked
                .iter()
                .any(|command| command[0].eq_ignore_ascii_case("SCAN")),
            "a named list knows its keys: {asked:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Reporting a failing watch
    //
    // One `#[dynamic_config]` type per test: the snapshot, the remote slot and
    // the sink's generation all live in statics keyed by the type, so two
    // tests sharing one would race and — worse — pass alone.
    // -----------------------------------------------------------------------

    /// The failure nobody notices: the loop was working, the subscription
    /// died, and the watch ends on a thread whose result is usually dropped.
    /// Configuration simply stops updating.
    ///
    /// What the status must say afterwards is a *pair*: `reachable()` goes to
    /// `Some(false)` while `last_fetch` keeps the instant the last document
    /// really arrived — so an alert can ask "down, and stale for how long".
    /// A failure that reset the clock would hide the second half.
    #[test]
    fn a_dead_subscription_reports_the_store_as_down_and_leaves_the_clock_running() {
        use dynamic_config::dynamic_config;

        #[dynamic_config]
        #[derive(Debug, serde::Deserialize)]
        struct Subscribed {
            // Never read: this test is about the status the store records,
            // not about the document, which never gets as far as a snapshot.
            #[allow(dead_code)]
            host: String,
        }

        let (url, _asked, stop) = subscribable(
            1,
            vec![resp_array(&[Some(r#"{"db": {"host": "base"}}"#)])],
            Then::HangsUp,
        );

        Subscribed::set_remote(Redis::new(&url, "myapp/db.json").unwrap());
        Subscribed::refresh_remote().expect("the store answers the first read");

        // Taken after the source is installed, which is what fences it.
        let sink = Subscribed::remote_sink();
        let before = sink.status();

        assert_eq!(before.reachable(), Some(true), "one fetch, and it answered");
        assert!(before.last_fetch.is_some());

        let watcher = Redis::new(&url, "myapp/db.json")
            .unwrap()
            .reporting_to(sink);
        let watch = dynamic_config::RemoteWatch::new();
        let watching = watch.watching();
        let (ended, ending) = std::sync::mpsc::channel();

        let loops = std::thread::spawn(move || {
            let outcome = watcher.watch(&watching, |_| Ok(()));
            let _ = ended.send(());
            outcome
        });

        ending
            .recv_timeout(Duration::from_secs(10))
            .expect("a subscription that died ends the watch rather than spinning");

        let outcome = loops.join().expect("the thread should end");
        let error = outcome.expect_err("the subscription died");

        watch.stop();
        stop.store(true, std::sync::atomic::Ordering::Release);

        assert!(
            error.to_string().contains("the subscription failed"),
            "{error}"
        );

        let after = sink.status();

        assert_eq!(
            after.reachable(),
            Some(false),
            "a loop that stopped reaching its store is a store that is down"
        );
        assert_eq!(after.consecutive_failures, 1);
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
            "a subscription that dropped may yet come back"
        );

        // The recorded failure is a kind and a path, and the URL that carries
        // the password is in neither.
        assert!(!format!("{:?}", after.last_failure).contains("myapp/db.json"));
    }

    /// The other shape, and the one that argues for itself least obviously: a
    /// re-read that failed does not end the watch, so nothing else will ever
    /// mention it. One failure between deliveries is a blip the streak clears;
    /// a credential the server has started refusing is every notification from
    /// now on, and the streak is what says which this is.
    #[test]
    fn a_failed_re_read_is_reported_and_the_watch_carries_on() {
        use dynamic_config::dynamic_config;

        #[dynamic_config]
        #[derive(Debug, serde::Deserialize)]
        struct Rereading {
            // Never read: this test is about the status the store records,
            // not about the document, which never gets as far as a snapshot.
            #[allow(dead_code)]
            host: String,
        }

        let (url, _asked, stop) = subscribable(
            1,
            vec![
                // The first read is the caller's own `refresh_remote()`.
                resp_array(&[Some(r#"{"db": {"host": "base"}}"#)]),
                // The re-read the notification triggers: the key holds
                // nothing, which is what a deleted member of a set looks like.
                resp_array(&[None]),
            ],
            Then::Publishes,
        );

        Rereading::set_remote(Redis::new(&url, "myapp/db.json").unwrap());
        Rereading::refresh_remote().expect("the store answers the first read");

        let sink = Rereading::remote_sink();
        let before = sink.status();

        let watcher = Redis::new(&url, "myapp/db.json")
            .unwrap()
            .reporting_to(sink);
        let watch = dynamic_config::RemoteWatch::new();
        let watching = watch.watching();
        let (ended, ending) = std::sync::mpsc::channel();

        let loops = std::thread::spawn(move || {
            let outcome = watcher.watch(&watching, |_| Ok(()));
            let _ = ended.send(());
            outcome
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(10);

        while sink.status().consecutive_failures == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }

        let after = sink.status();

        assert_eq!(
            after.reachable(),
            Some(false),
            "the notification arrived and the document did not"
        );
        assert_eq!(
            after.last_fetch, before.last_fetch,
            "a failed re-read leaves the clock where the last document left it"
        );
        assert_eq!(after.fetches, before.fetches);

        assert!(
            ending.recv_timeout(Duration::from_millis(500)).is_err(),
            "a failed re-read is transient: the next write notifies again, so \
             the loop must still be running"
        );

        watch.stop();
        stop.store(true, std::sync::atomic::Ordering::Release);

        let outcome = loops.join().expect("the thread should end");

        assert!(outcome.is_ok(), "stopping is not a failure: {outcome:?}");
    }

    /// A watch refused at the door is *not* a store that stopped answering.
    ///
    /// The line, and 0.6.1's audit of all seven watch loops is what put it
    /// here rather than in one crate's habit: etcd and NATS used to report a
    /// refusal that never reached the store, Redis and S3 did not, and both
    /// had a test saying so. `RemoteStatus::reachable()` settles it — it is
    /// *whether the store answered the last time it was asked*, and a source
    /// with an unwatchable key shape never asks. Charging it to
    /// `dynamic_config_remote_up` would page somebody about Redis for a typo,
    /// and the status carries no message to correct them with.
    #[test]
    fn a_watch_refused_at_the_door_is_not_a_store_that_stopped_answering() {
        use dynamic_config::dynamic_config;

        #[dynamic_config]
        #[derive(Debug, serde::Deserialize)]
        struct Refused {
            // Never read: this test is about the status the store records,
            // not about the document, which never gets as far as a snapshot.
            #[allow(dead_code)]
            host: String,
        }

        // No source is installed: a sink does not need one, and this is the
        // state a program that only ever watches is in.
        let sink = Refused::remote_sink();

        let client = Client::open("redis://127.0.0.1:9").unwrap();
        let source = Redis::from_client(client, Keys::prefix("myapp/"))
            .with_format(Format::Json)
            .reporting_to(sink);

        let watch = dynamic_config::RemoteWatch::new();
        let error = source
            .watch(&watch.watching(), |_| Ok(()))
            .expect_err("a prefix cannot be watched");

        assert!(error.to_string().contains("cannot be watched"), "{error}");
        assert_eq!(
            sink.status().reachable(),
            None,
            "nothing has been asked of this store, so it is neither up nor down"
        );
    }

    // -----------------------------------------------------------------------
    // TLS: the shared vocabulary, translated into the client's own
    // `TlsCertificates`.
    // -----------------------------------------------------------------------

    /// A certificate authority and a client certificate signed by it,
    /// generated here. A committed fixture expires, and a suite that fails on
    /// a date nobody chose is worse than one that costs a millisecond.
    #[cfg(feature = "tls")]
    fn material() -> (String, String, String) {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();

        let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
        let client_key = KeyPair::generate().unwrap();
        let client = CertificateParams::new(vec!["myapp".to_owned()])
            .unwrap()
            .signed_by(&client_key, &issuer)
            .unwrap();

        (ca.pem(), client.pem(), client_key.serialize_pem())
    }

    /// The happy path, with material nothing on this machine trusts: the
    /// client is built, and construction still reaches no network.
    #[cfg(feature = "tls")]
    #[test]
    fn a_private_authority_and_a_client_certificate_build_a_client() {
        let (ca, certificate, key) = material();

        Redis::with_tls(
            "rediss://127.0.0.1:6379",
            "myapp/db.json",
            &TlsConfig::new()
                .with_ca_certificate_pem(ca)
                .with_client_certificate_pem(certificate, key),
        )
        .expect("the generated material is valid PEM");
    }

    /// TLS material on a `redis://` URL is a deployment that believes it is
    /// encrypted and is not. Refused here, naming the scheme, rather than
    /// three layers down where the message has no address in it.
    #[cfg(feature = "tls")]
    #[test]
    fn tls_material_on_a_plaintext_url_is_refused_rather_than_ignored() {
        let (ca, _, _) = material();

        let error = Redis::with_tls(
            "redis://127.0.0.1:6379",
            "myapp/db.json",
            &TlsConfig::new().with_ca_certificate_pem(ca),
        )
        .expect_err("that URL negotiates no TLS at all");

        assert!(error.to_string().contains("rediss://"), "{error}");
        assert!(
            error.to_string().contains("refused rather than ignored"),
            "{error}"
        );
    }

    /// The sharpest rule in this feature: the client's own message for a key
    /// it cannot parse embeds the parser's, and the parser's renders the line
    /// it choked on. So the upstream text is dropped, and this is the test
    /// that says so.
    #[cfg(feature = "tls")]
    #[test]
    fn a_malformed_private_key_never_quotes_itself_into_the_error() {
        const PLANTED: &str = "PLANTED-PRIVATE-KEY-MATERIAL";

        let (ca, certificate, _) = material();

        let error = Redis::with_tls(
            "rediss://127.0.0.1:6379",
            "myapp/db.json",
            &TlsConfig::new()
                .with_ca_certificate_pem(ca)
                .with_client_certificate_pem(
                    certificate,
                    format!("-----BEGIN PRIVATE KEY-----\n{PLANTED}\n-----END PRIVATE KEY-----\n"),
                ),
        )
        .expect_err("the key is not a key");

        assert!(!error.to_string().contains(PLANTED), "{error}");
        assert!(!format!("{error:?}").contains(PLANTED), "{error:?}");
    }

    /// A Redis URL carries its password in the authority, and the password
    /// may contain `@`. Every new error path gets the same redaction test the
    /// old ones have.
    #[cfg(feature = "tls")]
    #[test]
    fn a_tls_failure_never_carries_the_password_out_of_the_url() {
        let error = Redis::with_tls(
            "rediss://app:p@ss@w@rd@127.0.0.1:6379",
            "myapp/db.json",
            &TlsConfig::new().with_ca_certificate_file("/nonexistent/private-ca.pem"),
        )
        .expect_err("the CA file is not there");

        let printed = format!("{error} {error:?}");

        assert!(!printed.contains("p@ss"), "{printed}");
        assert!(printed.contains("app:***@"), "{printed}");
        assert!(printed.contains("/nonexistent/private-ca.pem"), "{printed}");
    }
}
