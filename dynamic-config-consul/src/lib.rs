//! Read [`dynamic-config`] configuration from Consul's key/value store.
//!
//! Consul's KV API is plain HTTP, so this implements the **blocking**
//! [`RemoteSource`] trait: nothing here needs an async runtime, and neither
//! does using it.
//!
//! ```no_run
//! use dynamic_config_consul::Consul;
//!
//! # struct DbConfig;
//! # impl DbConfig {
//! #     fn set_remote(_: Consul) {}
//! #     fn refresh_remote() -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! DbConfig::set_remote(
//!     Consul::new("http://consul.internal:8500", "myapp/db.json")
//!         .with_token(std::env::var("CONSUL_HTTP_TOKEN")?),
//! );
//!
//! DbConfig::refresh_remote()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # What it reads
//!
//! `GET {address}/v1/kv/{key}`, and base64-decodes the single `Value` Consul
//! returns. **The stored value is a whole configuration document** — the same
//! bytes that would be in a config file — so the format comes from the key's
//! extension, or from [`with_format`](Consul::with_format).
//!
//! That is the opposite of [`dynamic-config-vault`], which wraps a secret's
//! fields under a section key. The difference is not a whim: Vault stores a map
//! of named secrets, Consul stores an opaque blob, and each is easiest to use
//! as what it already is.
//!
//! # Several keys as one document
//!
//! A deployment that splits its configuration across a subtree — `myapp/db`,
//! `myapp/server` — can have one source read the lot, and [`Keys`] says which:
//!
//! ```no_run
//! # use dynamic_config_consul::{Consul, Keys};
//! # let address = "http://consul.internal:8500";
//! // Named keys: a list of layers, merged in the order given, later wins.
//! let consul = Consul::new(address, Keys::several(["myapp/base.json", "myapp/local.json"]));
//!
//! // A prefix: disjoint sections, and an overlap between two of them is an error.
//! let consul = Consul::new(address, Keys::prefix("myapp/"))
//!     .with_format(dynamic_config::Format::Json);
//! ```
//!
//! The two forms cost different things, and the difference is the agent's, not
//! this crate's:
//!
//! - **A prefix is one request** — `?recurse`, which Consul answers with the
//!   whole subtree at one index. So the set is consistent: a write landing
//!   mid-read cannot produce a document that never existed.
//! - **A named list is one request per key.** Consul's KV API reads one key or
//!   one subtree and has no batch read of a caller-chosen set, so a list is
//!   **not** read atomically. Its transaction endpoint could do it in one, at
//!   the price of a write-shaped request and a sixty-four operation ceiling;
//!   that trade is recorded rather than taken. Prefer a prefix where the keys
//!   are disjoint anyway.
//!
//! Three consequences that belong here rather than in an incident:
//!
//! - **A prefix that matches more than 512 keys is refused.** A prefix is
//!   caller input and the answer to it is server input.
//! - **Provenance becomes store-grained.** The merged document is one layer, so
//!   `source_of` answers "from consul … keys a, b" rather than naming which key
//!   supplied a value. [`describe`](RemoteSource::describe) names the whole set,
//!   which is as close as one layer gets.
//! - **One unreadable key fails the whole fetch.** A configuration quietly
//!   missing a section is worse than a refresh that failed and left the last
//!   document serving.
//!
//! # Watching
//!
//! Consul cannot push, but it can hold a request open until something changes —
//! a *blocking query*. [`Consul::watch`] is that loop, and it is genuinely
//! change-driven rather than a poll with extra steps: the agent answers the
//! moment the key moves.
//!
//! It blocks, so it belongs on a thread, and a thread cannot be cancelled from
//! outside — hence the [`Watching`] token.
//!
//! **A prefix can be watched; a named list cannot.** A watch on a set is only
//! honest if the store says *the set* changed and the set can then be read as
//! of one instant. A recursive blocking query answers both at once, and is the
//! only watch in this family that needs no re-read at all: the agent holds the
//! request open until the subtree's index moves, and what it then sends back
//! **is the subtree at that index**. The document the callback receives is
//! folded from those exact bytes, so there is no window between noticing and
//! reading for a second write to land in. A named list has no such query —
//! Consul reads one key or one subtree, never a caller-chosen set — so it
//! refuses at [`watch`](Consul::watch), before the first change; poll
//! `refresh_remote()` on a timer instead.
//!
//! ```no_run
//! # use dynamic_config::RemoteWatch;
//! # use dynamic_config_consul::Consul;
//! # fn example(consul: Consul) {
//! # let sink = |_: dynamic_config::Fetched| -> Result<(), dynamic_config::Error> { Ok(()) };
//! let watch = RemoteWatch::new();
//! let watching = watch.watching();
//!
//! std::thread::spawn(move || consul.watch(&watching, move |document| sink(document)));
//!
//! // Dropping `watch` — or calling `watch.stop()` — ends the loop.
//! # }
//! ```
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config
//! [`dynamic-config-vault`]: https://docs.rs/dynamic-config-vault

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::time::Duration;

use base64::Engine;
use dynamic_config::{Error, Fetched, Format, RemoteSink, RemoteSource, Watching};
use dynamic_config_store_core::attempts::Attempts;
use dynamic_config_store_core::credential::{Cached, Issued};
use dynamic_config_store_core::documents::{self, Overlap};
use dynamic_config_store_core::guarded;

pub mod auth;
mod tls;

pub use auth::{Auth, Bearer};

/// A private certificate authority and a client certificate, as data.
///
/// The shared vocabulary all seven store crates take, so that reaching TLS
/// never means naming `ureq`'s types — see [`with_tls`](Consul::with_tls).
pub use dynamic_config_store_core::tls::TlsConfig;

/// How long to wait for Consul before giving up.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a blocking query is allowed to hold the connection open.
///
/// Consul's own default is five minutes; this is shorter because it is also the
/// worst case for noticing a stop, and five minutes of that is a long time to
/// wait for a thread to go away. Consul's ceiling is ten minutes.
const DEFAULT_WAIT: Duration = Duration::from_secs(60);

/// How long to pause after a failed blocking query before trying again.
///
/// A restarting agent should not be met with a tight retry loop.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// What a source reads: one key, several named keys, or a subtree.
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
    /// the list, so the list is the precedence. One request per key, because
    /// Consul's KV API has no batch read of a caller-chosen set.
    ///
    /// **Cannot be watched**, for that same reason: a blocking query blocks on
    /// one key or one subtree, so watching a list would mean waking on one key
    /// and then reading the rest — collecting the new value of one beside the
    /// old value of another, a document that never existed.
    Several(Vec<String>),
    /// Every key under a prefix, merged as **disjoint sections**.
    ///
    /// A caller naming a prefix is not expressing an order — the order is
    /// whatever the agent lists — so two keys under it supplying the same path
    /// is a deployment bug, and reported as one rather than resolved. One
    /// `?recurse` request, so the subtree is read at a single index.
    ///
    /// **Can be watched**, and needs no re-read to do it: a recursive blocking
    /// query's answer *is* the subtree at the index it woke on. See
    /// [`Consul::watch`].
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
    /// A prefix has none to list — the set is not known until the agent
    /// answers.
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
            Self::One(key) => format!("kv/{key}"),
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

/// A key in Consul's KV store, as a configuration source.
///
/// Not `Clone`: the session holds the current token, and two clones logging in
/// separately would double the login traffic. Wrap it in an `Arc` if two places
/// need one.
pub struct Consul {
    address: String,
    keys: Keys,
    format: Option<Format>,
    /// Why the keys' own extensions could not settle the format between them.
    ///
    /// Kept rather than reported at construction because `new` cannot fail and
    /// because `with_format` is allowed to settle it afterwards.
    disagreement: Option<String>,
    auth: Auth,
    /// The token the current login produced, and when it needs another.
    session: Cached<String>,
    datacenter: Option<String>,
    timeout: Duration,
    wait: Duration,
    agent: Option<ureq::Agent>,
    /// What [`with_tls`](Consul::with_tls) was given, translated into an agent
    /// per call.
    ///
    /// Not cached the way Vault's and Firestore's are: this crate already
    /// builds an agent per call, because a blocking query needs a timeout the
    /// ordinary read must not have.
    tls: Option<TlsConfig>,
    /// Where [`watch`](Consul::watch) reports a query that came back with
    /// nothing; see [`reporting_to`](Consul::reporting_to). Nobody, by
    /// default, which is what makes reporting free for a caller who never
    /// asked for it.
    attempts: Attempts,
}

impl Consul {
    /// The key `keys`, served by the Consul agent at `address`.
    ///
    /// `keys` is a key — `"myapp/db.json"` — or a [`Keys`], for the several-keys
    /// and prefix forms.
    ///
    /// The format is taken from the key's extension — `myapp/db.json` is JSON.
    /// A key without one, and every prefix, needs
    /// [`with_format`](Self::with_format).
    pub fn new(address: impl Into<String>, keys: impl Into<Keys>) -> Self {
        let keys = keys.into();

        let (format, disagreement) = match documents::agreed_format(keys.named()) {
            Ok(format) => (format, None),
            Err(complaint) => (None, Some(complaint)),
        };

        Self {
            address: address.into().trim_end_matches('/').to_owned(),
            keys,
            format,
            disagreement,
            auth: Auth::Anonymous,
            session: Cached::new(),
            datacenter: None,
            timeout: DEFAULT_TIMEOUT,
            wait: DEFAULT_WAIT,
            agent: None,
            tls: None,
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

    /// What a blocking query should ask for, and whether it recurses — or an
    /// error saying this source cannot be watched at all.
    ///
    /// One key and one prefix are both **one** blocking query, which is the
    /// property that decides this: the query's answer is what the callback
    /// gets, so there is no second read for a write to slip between. A named
    /// list is not one query — Consul's KV API has no batch read of a
    /// caller-chosen set — so watching it would mean blocking on one key and
    /// then reading the others, which is exactly how a document that never
    /// existed gets delivered.
    fn watched(&self) -> Result<(&str, bool), Error> {
        match &self.keys {
            Keys::One(key) => Ok((key, false)),
            Keys::Prefix(prefix) => Ok((prefix, true)),
            Keys::Several(_) => Err(Error::remote(format!(
                "{}: a source that reads a named list of keys cannot be \
                 watched; Consul has no batch read, so the set would be \
                 blocked on one key and then read key by key, which can \
                 deliver a document that never existed — watch a prefix, or \
                 poll `refresh_remote()` on a timer instead",
                self.describe()
            ))),
        }
    }

    /// The ACL token to authenticate with.
    ///
    /// Shorthand for `with_auth(Auth::token(..))`. A token that stops working
    /// cannot be replaced, because there are no credentials here to log in
    /// again with; [`Auth::kubernetes`] and [`Auth::jwt`] can.
    #[must_use]
    pub fn with_token(self, token: impl Into<String>) -> Self {
        self.with_auth(Auth::token(token))
    }

    /// How to obtain an ACL token.
    ///
    /// ```no_run
    /// # use dynamic_config_consul::{Auth, Consul};
    /// // In Kubernetes, with no secret to distribute at all.
    /// let consul = Consul::new("http://consul:8500", "myapp/db.json")
    ///     .with_auth(Auth::kubernetes("kubernetes"));
    ///
    /// // Or whatever the operator put in the environment.
    /// let consul = Consul::new("http://consul:8500", "myapp/db.json")
    ///     .with_auth(Auth::from_environment());
    /// ```
    ///
    /// Logging in is lazy: this reaches nothing, and the first read does it.
    #[must_use]
    pub fn with_auth(mut self, auth: Auth) -> Self {
        self.auth = auth;
        self.session.invalidate();
        self
    }

    /// Uses an HTTP client the program already has.
    ///
    /// For a caller with its own proxy settings, a private CA, a client
    /// certificate, or a connection pool it would rather not have a second copy
    /// of. The agent's own timeout applies instead of
    /// [`with_timeout`](Self::with_timeout) — including for the long blocking
    /// query [`watch`](Self::watch) issues, so an agent used for watching needs
    /// a timeout above [`with_wait`](Self::with_wait).
    ///
    /// The escape hatch, and it stays one: [`with_tls`](Self::with_tls) covers
    /// a private CA and a client certificate, and everything else — a proxy, a
    /// connection pool, an option this crate has never heard of — still lives
    /// here. Setting both is refused rather than resolved; see
    /// [`with_tls`](Self::with_tls).
    #[must_use]
    pub fn with_agent(mut self, agent: ureq::Agent) -> Self {
        self.agent = Some(agent);
        self
    }

    /// A private certificate authority, a client certificate, or both.
    ///
    /// The same three settings, spelled the same way, in all seven store
    /// crates — and spelled as *data*, so nothing here names a `ureq` type:
    ///
    /// ```no_run
    /// # use dynamic_config_consul::{Consul, TlsConfig};
    /// let consul = Consul::new("https://consul.internal:8501", "myapp/db.json")
    ///     .with_tls(
    ///         TlsConfig::new()
    ///             .with_ca_certificate_file("/etc/consul.d/consul-agent-ca.pem")
    ///             .with_client_certificate_files(
    ///                 "/etc/consul.d/client.crt",
    ///                 "/etc/consul.d/client.key",
    ///             ),
    ///     );
    /// ```
    ///
    /// Consul expresses all of it: a CA from a file or from bytes, and a client
    /// certificate from either. A CA replaces the platform trust store rather
    /// than adding to it — naming a private authority is saying the public ones
    /// do not apply to this host — so a deployment that needs both puts both in
    /// the file. Consul's own agent CA is exactly this case: `consul tls ca
    /// create` mints an authority no public store has heard of.
    ///
    /// There is no way to turn verification off; [`TlsConfig`]'s own
    /// documentation argues that one.
    ///
    /// **Nothing is read here.** The files are opened when a request builds its
    /// client, so a missing CA is an error naming the path rather than a panic
    /// in a builder chain.
    ///
    /// # With `with_agent`
    ///
    /// Setting both is **refused**, at the first request, naming both calls.
    /// An agent already carries a complete TLS configuration, so "apply this
    /// too" has no meaning that is not a guess — and the guess that loses
    /// silently discards a CA, which is the failure this whole surface exists
    /// to prevent. Put the CA on the agent, or drop the agent.
    #[must_use]
    pub fn with_tls(mut self, tls: TlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    /// The datacenter to read from, when it is not the agent's own.
    #[must_use]
    pub fn with_datacenter(mut self, datacenter: impl Into<String>) -> Self {
        self.datacenter = Some(datacenter.into());
        self
    }

    /// How long a single fetch may take before it is given up on. Ten seconds
    /// by default.
    ///
    /// The deadline for **one fetch attempt**, excluding retries the
    /// underlying client performs — the same sentence every store in this
    /// family answers to. `ureq` performs none of its own, so here the
    /// deadline is the whole story.
    ///
    /// The blocking query [`watch`](Self::watch) issues is the exception, and
    /// deliberately so: it is one fetch that is *meant* to be held open, so
    /// its client-side timeout is sized from [`with_wait`](Self::with_wait)
    /// plus this value plus the jitter Consul adds.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// How long a blocking query may hold the connection open, when
    /// [`watch`](Self::watch) is used. One minute by default.
    ///
    /// This is also how long a stopped watch can take to notice, so it trades
    /// one against the other: longer means fewer requests, and a slower exit.
    /// Consul's own ceiling is ten minutes, so anything above it is clamped
    /// there — the agent would cap it silently anyway, and this way the
    /// client-side timeout stays sized to what the agent will actually do.
    #[must_use]
    pub fn with_wait(mut self, wait: Duration) -> Self {
        /// Consul rejects (well: caps) waits over ten minutes.
        const CEILING: Duration = Duration::from_secs(600);

        self.wait = wait.min(CEILING);
        self
    }

    /// Reports the watch loop's *failed* attempts to `sink`.
    ///
    /// A watch loop is the half of a store `dynamic-config` cannot otherwise
    /// see. [`RemoteSink::apply`] records a delivery, so a working watch keeps
    /// [`RemoteStatus`] current — but a loop whose blocking query is erroring,
    /// whose key was deleted or whose ACL token was refused delivers nothing,
    /// and without this says nothing: `dynamic_config_remote_up` would report
    /// the last *delivery* rather than the last *attempt*, and an agent that
    /// stopped answering an hour ago would look healthy until something called
    /// `refresh_remote`.
    ///
    /// ```no_run
    /// # use dynamic_config::{RemoteWatch, Watching};
    /// # use dynamic_config_consul::Consul;
    /// # struct DbConfig;
    /// # impl DbConfig {
    /// #     fn remote_sink() -> dynamic_config::RemoteSink { unimplemented!() }
    /// # }
    /// # fn example(address: &str, watching: Watching) -> Result<(), dynamic_config::Error> {
    /// let sink = DbConfig::remote_sink();
    ///
    /// Consul::new(address, "myapp/db.json")
    ///     .reporting_to(sink)
    ///     .watch(&watching, move |document| sink.apply(document))
    /// # }
    /// ```
    ///
    /// One sink serves both halves, and it is taken **once, where the loop is
    /// wired**: a sink is `Copy`, and the generation it captures there is what
    /// fences a loop winding down after its source was replaced from charging
    /// its failures to the replacement.
    ///
    /// A failure to report a failure never reaches the loop — reporting is
    /// infallible and silent — and what it moves is deliberately narrow: the
    /// failure streak and the last failure, never the fetch clock. So
    /// `dynamic_config_remote_last_fetch_seconds` keeps ageing while
    /// `dynamic_config_remote_up` goes to zero, which is the pair that says
    /// both *the store is not answering* and *how stale what it last said has
    /// become*.
    ///
    /// A [`fetch`](RemoteSource::fetch) needs none of this: a fetch records
    /// itself, through the `Remote` that performed it.
    ///
    /// [`RemoteStatus`]: dynamic_config::RemoteStatus
    #[must_use]
    pub fn reporting_to(mut self, sink: RemoteSink) -> Self {
        self.attempts = Attempts::to(sink);
        self
    }

    /// Calls `on_change` whenever what this source reads changes.
    ///
    /// Uses Consul's blocking queries: each request carries the index the last
    /// one returned, and the agent holds it open until that index moves or
    /// [`with_wait`](Self::with_wait) expires. So this is change-driven, not a
    /// poll — the callback runs when the value actually moves.
    ///
    /// **One key or a prefix.** A prefix watch is the cheapest correct watch on
    /// a set anywhere in this family, because it needs no re-read at all: a
    /// recursive blocking query's *answer is the subtree at one index*, so the
    /// document handed to `on_change` is folded from the very bytes the agent
    /// blocked to send. There is no window between "the set changed" and
    /// "read the set" for a second write to land in. A **named list** is
    /// refused; the reason is on [`Keys::Several`].
    ///
    /// The current value is **not** delivered at startup, for the same reason a
    /// file watcher does not report an edit when it starts. Fetch first if the
    /// starting value matters, which it usually does:
    ///
    /// ```no_run
    /// # use dynamic_config::{RemoteSource, RemoteWatch};
    /// # use dynamic_config_consul::Consul;
    /// # struct Sink;
    /// # impl Sink {
    /// #     fn apply(&self, _: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
    /// # }
    /// # fn example(consul: Consul, watching: dynamic_config::Watching) -> Result<(), dynamic_config::Error> {
    /// # let sink = Sink;
    /// sink.apply(consul.fetch()?)?;
    /// consul.watch(&watching, move |document| sink.apply(document))
    /// # }
    /// ```
    ///
    /// A failed query does not end the watch: the agent restarting, a network
    /// blip, or a key that does not exist *yet* are all exactly what a watch is
    /// supposed to survive. It pauses briefly and tries again, and gives up only
    /// when `watching` says to. A document identical to the last one is not
    /// reported — Consul bumps the index on every write, including one that
    /// changed nothing.
    ///
    /// Surviving a failure quietly is not the same as hiding it:
    /// [`reporting_to`](Self::reporting_to) hands each failed attempt to a
    /// [`RemoteSink`], so a loop that has been erroring for an hour stops
    /// reporting the store as healthy.
    ///
    /// # Errors
    ///
    /// If `on_change` returns an error, which ends the watch — so a caller that
    /// wants to survive a bad document should log it and return `Ok`. Transport
    /// failures do not surface here; they are retried.
    ///
    /// Under a prefix, also if the subtree cannot be folded into a document:
    /// two keys supplying the same path, a key the agent answered with that is
    /// not under the prefix, or more keys than the budget. None of those is a
    /// blip a retry cures, and retrying them forever with nothing said would
    /// leave the configuration frozen and silent — the failure this crate's
    /// watch loops are shaped to avoid.
    pub fn watch<F>(&self, watching: &Watching, mut on_change: F) -> Result<(), Error>
    where
        F: FnMut(Fetched) -> Result<(), Error>,
    {
        let format = self.format()?;
        // Refused up front, so a source that cannot be watched fails at
        // `watch` rather than on the first change, hours later.
        let (watched, recurse) = self.watched()?;

        // A blocking query must be allowed to outlast its own wait plus the
        // jitter Consul adds — up to a sixteenth of it — or every query would
        // end as a client timeout instead of an answer. An eighth, with the
        // ordinary timeout on top, leaves room for a slow answer as well.
        // Saturating: both terms are caller input, and a caller who says
        // `Duration::MAX` deserves a very long timeout, not a panic.
        let agent = self.agent(
            self.wait
                .saturating_add(self.wait / 8)
                .saturating_add(self.timeout),
        )?;

        let mut index = 0;
        let mut last: Option<String> = None;
        // The first query carries index 0, which Consul answers immediately
        // with whatever is stored. That is the value the caller already has —
        // it primes the index and the comparison, and reports nothing, the same
        // way a file watcher does not announce an edit when it starts.
        let mut priming = true;

        while watching.keep_going() {
            let answered = match self.blocking_read(&agent, watched, recurse, index) {
                Ok(answered) => answered,
                Err(error) => {
                    // Retried rather than reported *to the caller*: the loop's
                    // whole job is to survive the store going away for a
                    // while. Recorded all the same, because surviving it
                    // silently is how an agent that stopped answering an hour
                    // ago goes on looking healthy. Sleeping in slices keeps a
                    // stop from waiting out the whole pause.
                    self.attempts.failed(&error);

                    watching.sleep_for(RETRY_AFTER);
                    continue;
                }
            };

            // Consul resets its index on a restart or a key being recreated; a
            // stale one would then park the query forever.
            index = if answered.index < index {
                0
            } else {
                answered.index
            };

            // The fold happens *here* rather than inside the read, because the
            // two failures must be told apart: a read that failed is retried,
            // and a subtree that cannot become a document is reported. The
            // second is a deployment bug, and a loop that retried it would be
            // a configuration that stopped updating with nothing said.
            let folded = self
                .watched_document(&answered.entries, recurse, format)
                // A subtree that cannot become a document ends the watch, so
                // this is the last thing that will ever be recorded about this
                // store — which is exactly why it is recorded. A watch that
                // stopped is a configuration that has stopped updating.
                .inspect_err(|error| self.attempts.failed(error))?;

            let Some(text) = folded else {
                // The key holds nothing, or the subtree is empty. Consul will
                // still block on the next query, but only while its index is
                // meaningful — a pause here is what stops a degenerate index
                // from becoming a hot loop.
                //
                // Reported as a failed attempt, because that is what a fetch
                // of the same key would record: `fetch` calls an empty answer
                // a failure and leaves the last document serving, and a watch
                // that called it health would have a deleted key read as a
                // healthy store for as long as nobody recreated it. The
                // message is built only when somebody is listening.
                if self.attempts.is_reporting() {
                    self.attempts.failed(&Error::remote(format!(
                        "{}: the watched key holds no value",
                        self.describe()
                    )));
                }

                watching.sleep_for(RETRY_AFTER);

                continue;
            };

            let unchanged = last.as_ref() == Some(&text);

            last = Some(text.clone());

            if std::mem::take(&mut priming) || unchanged {
                continue;
            }

            // Deliberately not reported as a failed attempt: the agent answered
            // and the document arrived. What the callback then did with it —
            // `apply`, a reload, a validation that refused — is
            // `ConfigStatus`'s business, and `RemoteSink::apply` already
            // records it there. Charging it to the store would make a bad
            // document read as an unreachable agent.
            guarded(&mut on_change, Fetched::new(text, format), &self.describe())?;
        }

        Ok(())
    }

    /// The HTTP client: the caller's if they supplied one, otherwise ours.
    ///
    /// # Errors
    ///
    /// If both `with_agent` and `with_tls` were called, or if the TLS material
    /// cannot be read or parsed.
    fn agent(&self, timeout: Duration) -> Result<ureq::Agent, Error> {
        if let Some(agent) = &self.agent {
            // Refused rather than resolved: an agent is already a complete TLS
            // configuration, so applying a second one on top would mean
            // silently dropping one of them, and the one that would be dropped
            // is a CA the caller believes is pinned.
            if self.tls.is_some() {
                return Err(Error::remote(format!(
                    "{}: `with_agent` and `with_tls` were both called; \
                     an agent already carries its own TLS configuration, so \
                     this is refused rather than resolved — put the certificate \
                     authority on the agent, or drop the agent",
                    self.describe()
                )));
            }

            return Ok(agent.clone());
        }

        match &self.tls {
            Some(tls) => tls::agent(tls, timeout, &self.describe()),
            None => Ok(ureq::Agent::config_builder()
                .timeout_global(Some(timeout))
                .build()
                .new_agent()),
        }
    }

    /// The token to present, logging in if it is time.
    ///
    /// `Ok(None)` when there is nothing to present, which is the right answer
    /// for a Consul with ACLs disabled.
    fn token(&self) -> Result<Option<String>, Error> {
        match &self.auth {
            Auth::Anonymous => Ok(None),
            Auth::Token(supplied) => Ok(Some(supplied.clone())),
            // The previous token is ignored: Consul does not extend a
            // token, it issues another.
            Auth::Login { .. } => self.session.get(|_previous| self.login()).map(Some),
        }
    }

    /// Exchanges a bearer token for an ACL token.
    fn login(&self) -> Result<Issued<String>, Error> {
        let Some(body) = self.auth.login_body()? else {
            // Unreachable: `token()` handles the other variants before this.
            return Err(Error::remote(format!(
                "{}: {} needs no login",
                self.describe(),
                self.auth.describe()
            )));
        };

        let url = format!("{}/v1/acl/login", self.address);

        let response: serde_json::Value = self
            .agent(self.timeout)?
            .post(&url)
            .send_json(&body)
            .map_err(|error| {
                let described = format!(
                    "{}: logging in with {} failed: {error}",
                    self.describe(),
                    self.auth.describe()
                );

                // On the *login* endpoint the request shape is ours and
                // correct, so a 403 is the agent rejecting the bearer token —
                // a token that could not be obtained, which waiting does not
                // fix. Everything else may yet come good and stays `Remote`.
                match error {
                    ureq::Error::StatusCode(403) => Error::auth(described),
                    _ => Error::remote(described),
                }
            })?
            .body_mut()
            .read_json()
            .map_err(|error| {
                Error::remote(format!(
                    "{}: the login response was not JSON: {error}",
                    self.describe()
                ))
            })?;

        let secret = response
            .get("SecretID")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                Error::remote(format!(
                    "{}: the login response has no `SecretID`",
                    self.describe()
                ))
            })?
            .to_owned();

        // Consul reports the expiry as a duration in nanoseconds, and omits it
        // for a token the auth method did not put one on.
        let ttl = response
            .get("ExpirationTTL")
            .and_then(serde_json::Value::as_u64)
            .filter(|nanos| *nanos > 0)
            .map(Duration::from_nanos);

        Ok(Issued { value: secret, ttl })
    }

    /// Adds the ACL token to a request, if there is one.
    fn authenticated(
        &self,
        request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    ) -> Result<ureq::RequestBuilder<ureq::typestate::WithoutBody>, Error> {
        match self.token()? {
            Some(token) => Ok(request.header("X-Consul-Token", &token)),
            None => Ok(request),
        }
    }

    /// One blocking query on what this source watches — a key, or a subtree
    /// with `recurse`. `index` of zero returns immediately.
    ///
    /// It returns the agent's answer undecoded. That is the whole reason a
    /// prefix watch can be honest here: **this response *is* the subtree at one
    /// index**, so nothing is read back afterwards and there is no window for a
    /// second write to land in.
    fn blocking_read(
        &self,
        agent: &ureq::Agent,
        key: &str,
        recurse: bool,
        index: u64,
    ) -> Result<Answered, Error> {
        let mut url = self.url(key, recurse);

        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&format!(
            "index={index}&wait={}s",
            self.wait.as_secs().max(1)
        ));

        let mut response = self.get(agent, &url)?;

        let index = response
            .headers()
            .get("X-Consul-Index")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .unwrap_or(index);

        let entries: Vec<serde_json::Value> = response.body_mut().read_json().map_err(|error| {
            Error::remote(format!(
                "{}: the response was not JSON: {error}",
                self.describe()
            ))
        })?;

        Ok(Answered { index, entries })
    }

    /// The document one blocking query's answer folds into, if it holds one.
    ///
    /// `Ok(None)` is *no configuration* — a key holding nothing, or a subtree
    /// with nothing under it — which the loop waits through rather than
    /// reporting, because no configuration is not a configuration.
    ///
    /// The two shapes differ in what an error means, and that is deliberate:
    ///
    /// - **One key.** Every failure here is dominated by the deleted-key case,
    ///   which is the thing the loop exists to wait through, so it becomes
    ///   `None` exactly as it always has.
    /// - **A prefix.** The failures are an overlap between two sections, a key
    ///   the agent answered with that is not under the prefix, and a subtree
    ///   over the budget. No retry cures any of them, and a silent retry loop
    ///   is the failure mode this crate's watches are shaped to avoid — so
    ///   they are reported and the watch ends.
    fn watched_document(
        &self,
        entries: &[serde_json::Value],
        recurse: bool,
        format: Format,
    ) -> Result<Option<String>, Error> {
        if !recurse {
            return Ok(self
                .pairs_of(entries, self.keys.named().first().map(String::as_str))
                .ok()
                .and_then(|mut pairs| pairs.pop())
                .map(|(_, text)| text));
        }

        documents::within_key_budget(entries.len(), &self.describe())?;

        let pairs = self.pairs_of(entries, None)?;

        if pairs.is_empty() {
            return Ok(None);
        }

        // `Overlap::Refused`, the same rule `fetch` applies: a prefix is a set
        // of disjoint sections, and the order the agent lists them in is
        // nobody's precedence.
        Ok(Some(
            documents::merged(&pairs, format, Overlap::Refused, &self.describe())?.text,
        ))
    }

    /// One authenticated GET, with the one-fresh-login retry every read gets.
    fn get(
        &self,
        agent: &ureq::Agent,
        url: &str,
    ) -> Result<ureq::http::Response<ureq::Body>, Error> {
        match self.call(agent.get(url)) {
            Err(CallError::Forbidden(_)) if self.can_relogin() => {
                // The token stopped working. One fresh login and one retry, not
                // a loop: if a new token is also refused, the policy is wrong
                // and retrying would turn a clear failure into a hang.
                self.session.invalidate();

                self.call(agent.get(url)).map_err(CallError::into_error)
            }
            outcome => outcome.map_err(CallError::into_error),
        }
    }

    /// The `(key, document)` pairs this source reads, in merge order.
    ///
    /// A named list is one request per key and **every one of them must
    /// answer**: merging the four that did would leave a process running a
    /// configuration with a section quietly missing from it.
    fn documents(&self, agent: &ureq::Agent) -> Result<Vec<(String, String)>, Error> {
        match &self.keys {
            Keys::One(key) => self.read(agent, key, false),
            Keys::Several(keys) => {
                let mut documents = Vec::with_capacity(keys.len());

                for key in keys {
                    documents.extend(self.read(agent, key, false)?);
                }

                Ok(documents)
            }
            // `?recurse`: one request for the whole subtree, at one index.
            Keys::Prefix(prefix) => self.read(agent, prefix, true),
        }
    }

    /// One request, and the pairs it answered with.
    fn read(
        &self,
        agent: &ureq::Agent,
        key: &str,
        recurse: bool,
    ) -> Result<Vec<(String, String)>, Error> {
        let mut response = self.get(agent, &self.url(key, recurse))?;

        let entries: Vec<serde_json::Value> = response.body_mut().read_json().map_err(|error| {
            Error::remote(format!(
                "{}: the response was not JSON: {error}",
                self.describe()
            ))
        })?;

        if recurse {
            // Checked before anything is decoded: the whole subtree is already
            // in memory by now — `?recurse` is one response — but base64
            // decoding and parsing every entry on top of it is the part worth
            // not doing.
            documents::within_key_budget(entries.len(), &self.describe())?;
        }

        self.pairs_of(&entries, (!recurse).then_some(key))
    }

    /// Sends a request with the current token.
    fn call(
        &self,
        request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    ) -> Result<ureq::http::Response<ureq::Body>, CallError> {
        self.authenticated(request)
            .map_err(CallError::Other)?
            .call()
            .map_err(|error| {
                // The message is the same either way; only the kind differs,
                // and only the typed status decides it. `{error}` here is
                // `ureq`'s own rendering of the status — never the request,
                // so the `X-Consul-Token` header cannot ride along.
                let described = format!("{}: {error}", self.describe());

                match error {
                    ureq::Error::StatusCode(403) => CallError::Forbidden(Error::auth(described)),
                    _ => CallError::Other(Error::remote(described)),
                }
            })
    }

    /// Whether a refused token can be traded for a fresh one.
    ///
    /// Only a login can: `Auth::Token` was handed in from outside, and
    /// invalidating it would just retry the identical string — one wasted
    /// request per read against a broken ACL.
    fn can_relogin(&self) -> bool {
        matches!(self.auth, Auth::Login { .. })
    }

    /// Turns Consul's array of entries into `(key, document)` pairs.
    ///
    /// `expected` is the key that was asked for, when a single key was: Consul
    /// answers a single-key read with a one-element array, and an empty one
    /// means the key is not there — a missing configuration rather than a
    /// transport failure, but still nothing to load, and still a whole fetch
    /// that fails.
    fn pairs_of(
        &self,
        entries: &[serde_json::Value],
        expected: Option<&str>,
    ) -> Result<Vec<(String, String)>, Error> {
        if let Some(key) = expected {
            if entries.is_empty() {
                return Err(Error::remote(format!(
                    "{}: `{key}` holds no value",
                    self.describe()
                )));
            }
        }

        let mut pairs = Vec::with_capacity(entries.len());

        for entry in entries {
            let key = entry
                .get("Key")
                .and_then(serde_json::Value::as_str)
                .or(expected)
                .unwrap_or_default()
                .to_owned();

            if let Keys::Prefix(prefix) = &self.keys {
                // Trimmed on both sides of the comparison: the URL drops a
                // leading slash and Consul answers without one, so a caller who
                // wrote `/myapp/` must not be told their own keys escaped it.
                documents::under_prefix(&key, prefix.trim_start_matches('/'), &self.describe())?;

                // Consul's own spelling of a folder: a key ending in `/` with
                // no value, which the UI creates when somebody makes a
                // directory. It is not a missing document; it is not a
                // document.
                if key.ends_with('/') {
                    continue;
                }
            }

            let encoded = entry
                .get("Value")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    Error::remote(format!("{}: `{key}` holds no value", self.describe()))
                })?;

            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| {
                    Error::remote(format!(
                        "{}: `{key}` is not valid base64: {error}",
                        self.describe()
                    ))
                })?;

            let text = String::from_utf8(decoded).map_err(|error| {
                Error::remote(format!(
                    "{}: `{key}` is not UTF-8: {error}",
                    self.describe()
                ))
            })?;

            pairs.push((key, text));
        }

        Ok(pairs)
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

    fn url(&self, key: &str, recurse: bool) -> String {
        let mut url = format!("{}/v1/kv/{}", self.address, key.trim_start_matches('/'));

        if recurse {
            url.push_str("?recurse=true");
        }

        if let Some(datacenter) = &self.datacenter {
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str("dc=");
            url.push_str(datacenter);
        }

        url
    }
}

/// A failed call, sorted by what a caller can do about it.
///
/// The sorting happens on `ureq`'s *typed* error, before anything becomes a
/// string. The old string test — does the message contain `"403"`? — read
/// true for any error mentioning a key like `myapp/403.json`, and a retry
/// decision should not depend on what somebody named their key.
enum CallError {
    /// The agent said 403: the token is the problem, and a fresh login might
    /// be the cure. Carries an [`ErrorKind::Auth`] error, because if the
    /// fresh login is refused too then waiting will not help — which is the
    /// one thing a watch loop needs to know.
    ///
    /// [`ErrorKind::Auth`]: dynamic_config::ErrorKind::Auth
    Forbidden(Error),
    /// Everything else — network, timeouts, 500s. A new token fixes none of
    /// it, and any of it may fix itself, so it stays `Remote`. Consul answers
    /// an ACL refusal with 403 and nothing else, so there is no 401 arm to
    /// write here; a 401 in front of a Consul is a proxy, and a proxy's
    /// verdict is not the store's.
    Other(Error),
}

impl CallError {
    fn into_error(self) -> Error {
        match self {
            Self::Forbidden(error) | Self::Other(error) => error,
        }
    }
}

/// What one blocking query came back with.
struct Answered {
    index: u64,
    /// The agent's entries, undecoded.
    ///
    /// Kept in this shape on purpose: for a `?recurse` query these *are* the
    /// subtree as of `index`, so folding them is the whole of the watch's
    /// re-read. Decoding them here would put the two failures a watch has to
    /// tell apart — a query that failed, and an answer that is not a
    /// configuration — behind one `Result`.
    entries: Vec<serde_json::Value>,
}

// Hand-written, never derived: a derive would print every field, and the
// fields include credentials. `{:?}` reaching a log is an ordinary accident —
// a `dbg!`, a `tracing::debug!(?source)` — and an accident must not disclose
// a secret. The other store crates follow the same rule.
impl std::fmt::Debug for Consul {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Consul")
            .field("address", &self.address)
            .field("keys", &self.keys)
            .field("format", &self.format)
            .field("datacenter", &self.datacenter)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl RemoteSource for Consul {
    fn fetch(&self) -> Result<Fetched, Error> {
        let format = self.format()?;

        let documents = self.documents(&self.agent(self.timeout)?)?;

        // Consul lists a subtree in key order and a named list is read in call
        // order, which is exactly the order each rule wants — so nothing is
        // sorted here.
        documents::merged(&documents, format, self.overlap(), &self.describe())
    }

    fn describe(&self) -> String {
        // The address too: "the key holds no value" helps nobody who has a
        // staging Consul and a production Consul and a wrong environment
        // variable.
        format!("consul {} {}", self.address, self.keys.describe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_a_credential() {
        let source = Consul::new("http://consul:8500", "myapp/db.json")
            .with_auth(Auth::token("hunter2-consul-token"));

        let printed = format!("{source:?} {:?}", Auth::token("hunter2-consul-token"));

        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("Token(***)"), "{printed}");
    }
}
