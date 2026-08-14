//! Read [`dynamic-config`] configuration from a Firestore document.
//!
//! Firestore's REST API is plain HTTP, so this implements the **blocking**
//! [`RemoteSource`] trait: nothing here needs an async runtime, and neither
//! does using it.
//!
//! ```no_run
//! use dynamic_config_firestore::{Auth, Firestore};
//!
//! # struct DbConfig;
//! # impl DbConfig {
//! #     fn set_remote(_: Firestore) {}
//! #     fn refresh_remote() -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! DbConfig::set_remote(
//!     Firestore::new("my-project", "config/db")
//!         // On GKE, Cloud Run or GCE, the workload's own identity.
//!         .with_auth(Auth::metadata_server()),
//! );
//!
//! // Fetching is explicit; the load that follows touches no network.
//! DbConfig::refresh_remote()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # What it reads
//!
//! One document, at a path like `config/db` — collection, then document. Its
//! fields become the configuration, wrapped under the section key, which is the
//! same shape [`dynamic-config-vault`] uses and for the same reason: Firestore
//! stores a map of named fields, so the natural unit is the field.
//!
//! Firestore types map onto configuration the obvious way — `stringValue`,
//! `integerValue`, `booleanValue`, `doubleValue`, `arrayValue`, `mapValue`. A
//! `timestampValue`, `bytesValue` or `referenceValue` becomes its string form,
//! because a configuration file has no better answer for one either.
//!
//! # Several documents as one section
//!
//! One section can be split across several documents, and [`Keys`] says which:
//!
//! ```no_run
//! # use dynamic_config_firestore::{Firestore, Keys};
//! // Merged in the order given — later wins — and all under the one section key.
//! let firestore = Firestore::new("my-project", Keys::several(["config/db", "overrides/db"]));
//! ```
//!
//! **A named list is one request**, and that is Firestore's own answer rather
//! than a loop wearing a batch's name: `:batchGet` takes the documents the
//! caller named and returns them together. Two things follow from what the API
//! actually promises:
//!
//! - **The answer arrives in whatever order the service likes** — `BatchGet`
//!   says so explicitly — so it is put back into call order here. The order a
//!   caller wrote is the precedence; the order a service replies in is not.
//! - **One request is not one snapshot.** Without a transaction each document
//!   is read at its own time, and this asks for none: an open read-only
//!   transaction is state on the service that a configuration read would have
//!   to remember to release. So a write landing mid-request can still produce
//!   a section that never existed as a whole. One round trip is the win;
//!   atomicity is not.
//!
//! **Every document lands under the same section key**, because that is what a
//! Firestore document is here: the contents of a section, not a whole
//! configuration file. So a list is layering — a shared document and an
//! override.
//!
//! **There is deliberately no collection form.** `documents.list` exists, so
//! the missing piece is not the protocol; it is the mapping. Folding a whole
//! collection into one section makes `config/db` and `config/server` collide
//! on `host` — the ordinary layout, refused — and naming a sub-section after
//! each document's id would invent a convention no other store here has, and
//! would make a list of one document mean something different from one
//! document. A deployment that wants several sections installs one source per
//! section, which is what it did before.
//!
//! Two consequences the multi-document form shares with the rest of the
//! family:
//!
//! - **Provenance becomes store-grained.** The merged section is one layer, so
//!   `source_of` names the set rather than which document supplied a value.
//! - **One unreadable document fails the whole fetch.** A section quietly
//!   missing half of itself is worse than a refresh that failed and left the
//!   last document serving.
//!
//! A **multi-document source cannot be watched**, and refuses at
//! [`watch`](Firestore::watch) rather than pretending to: the `updateTime` it
//! compares belongs to one document, and a set of them has none of its own.
//!
//! # Authenticating
//!
//! | Method | Constructor | For |
//! |---|---|---|
//! | Workload identity | [`Auth::metadata_server`] | GKE, Cloud Run, GCE — no secret to distribute |
//! | An access token | [`Auth::access_token`] | anything that already has one, including `gcloud auth print-access-token` |
//! | None | [`Auth::Emulator`] | the Firestore emulator, which wants no credentials |
//!
//! **A service-account JSON key is deliberately not supported**, and that is a
//! recommendation rather than a gap: signing one means an RS256 stack in a
//! configuration library, and Google's own guidance is that a downloaded key is
//! the option of last resort. Workload identity covers GKE, Cloud Run, GCE and
//! Cloud Functions; for anything else, mint a token outside the process and
//! hand it over with [`Auth::access_token`].
//!
//!
//! # Every failure branch of the watch loop, and what it reports
//!
//! A watch is the half of a store `dynamic-config` cannot see, and
//! [`reporting_to`](Firestore::reporting_to) is what lets it speak: the sink the
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
//! | the source reads several documents, so it cannot be watched | no — rule 3: nothing has been asked of Firestore |
//! | the read fails — a blip, an expired token, a document briefly unreachable | **yes**, and the loop waits out the interval |
//! | the document has no `updateTime`, so a change could never be detected | **yes**, and the watch ends |
//! | the first read, or an update time that has not moved | no — Firestore answered |
//! | `on_change` refuses the document | no — Firestore answered; `apply` counted the delivery, and what the document did next is `ConfigStatus`'s half |
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config
//! [`dynamic-config-vault`]: https://docs.rs/dynamic-config-vault

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod auth;
mod tls;
mod value;

use std::time::Duration;

use dynamic_config::{Error, Fetched, Format, RemoteSink, RemoteSource, Watching};
use dynamic_config_store_core::attempts::Attempts;
use dynamic_config_store_core::documents::{self, Overlap};
use dynamic_config_store_core::guarded;

pub use auth::Auth;

/// A private certificate authority and a client certificate, as data.
///
/// The shared vocabulary all seven store crates take, so that reaching TLS
/// never means naming `ureq`'s types — see [`with_tls`](Firestore::with_tls).
pub use dynamic_config_store_core::tls::TlsConfig;

/// How long to wait for Firestore before giving up.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a source reads: one document, or several named ones.
///
/// Every constructor takes one, and a bare `&str` or `String` is
/// [`Keys::one`] — so the single-document spelling every caller already wrote
/// keeps working unchanged.
///
/// There is no collection variant, and that is a decision rather than an
/// omission: a Firestore document is a section's *contents*, so a whole
/// collection folded into one section collides on every field name two
/// documents share. The crate documentation says the whole of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Keys {
    /// One document, whose fields are the whole section.
    One(String),
    /// Several named documents, merged **in the order given — later wins**.
    ///
    /// The rule a list of `.file(..)` calls already teaches: the caller wrote
    /// the list, so the list is the precedence. One `:batchGet` request for
    /// the set, put back into call order — the service answers in whatever
    /// order it likes, and says so.
    Several(Vec<String>),
}

impl Keys {
    /// One document, whose fields are the whole section.
    #[must_use]
    pub fn one(path: impl Into<String>) -> Self {
        Self::One(path.into())
    }

    /// Several named documents, merged in the order given — later wins.
    #[must_use]
    pub fn several<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Several(paths.into_iter().map(Into::into).collect())
    }

    /// How a diagnostic names what this source reads.
    ///
    /// One document renders as the path itself, so every message a
    /// single-document source has ever produced is unchanged.
    fn describe(&self) -> String {
        match self {
            Self::One(path) => path.clone(),
            Self::Several(paths) => format!("documents {}", paths.join(", ")),
        }
    }
}

impl From<&str> for Keys {
    fn from(path: &str) -> Self {
        Self::one(path)
    }
}

impl From<String> for Keys {
    fn from(path: String) -> Self {
        Self::One(path)
    }
}

impl From<&String> for Keys {
    fn from(path: &String) -> Self {
        Self::one(path)
    }
}

/// A failed call, sorted by what a caller can do about it.
///
/// Sorted on `ureq`'s *typed* status, before anything becomes a string: an
/// error message mentioning a path like `config/401` must not read as an
/// expired token.
enum CallError {
    /// Firestore said 401: the token is the problem, and a fresh one might be
    /// the cure.
    Unauthorized(Error),
    /// Firestore said 403: the token was accepted and the identity behind it
    /// is not allowed to read this document. Minting another token is the
    /// same identity, so there is nothing to retry — but it is still an auth
    /// failure to the caller, and one that fixing the IAM binding cures.
    ///
    /// Google's REST mapping is what makes this safe to name: `PERMISSION_DENIED`
    /// is the only thing that becomes a 403 here, because exhausted quota
    /// becomes a 429 and a missing credential a 401.
    Forbidden(Error),
    /// Everything else — network, timeouts, 500s. A new token fixes none of
    /// it, and any of it may fix itself.
    Other(Error),
}

impl CallError {
    fn into_error(self) -> Error {
        match self {
            Self::Unauthorized(error) | Self::Forbidden(error) | Self::Other(error) => error,
        }
    }
}

/// A document in Firestore, as a configuration source.
///
/// Not `Clone`: it holds the session that caches an access token, and two
/// clones fetching tokens separately would double the traffic.
pub struct Firestore {
    project: String,
    database: String,
    keys: Keys,
    key: String,
    auth: Auth,
    session: auth::Session,
    endpoint: Option<String>,
    timeout: Duration,
    agent: Option<ureq::Agent>,
    /// What [`with_tls`](Firestore::with_tls) was given, translated into an
    /// agent on first use.
    tls: Option<TlsConfig>,
    /// The fallback client, built once. A fresh agent per request would mean
    /// a fresh connection pool per request — a TLS handshake per poll tick.
    ///
    /// A `Result`, because building it can now fail: a CA file that is not
    /// there is discovered when the client is built, and the first request is
    /// where a caller can be told. Cached either way, so a bad path does not
    /// re-read a missing file once per poll tick. The failure is kept as its
    /// message because `Error` is deliberately not `Clone`; every failure this
    /// can hold is a `remote` one, so re-wrapping loses nothing.
    default_agent: std::sync::OnceLock<Result<ureq::Agent, String>>,
    /// Where [`watch`](Firestore::watch) reports a tick that came back with
    /// nothing; see [`reporting_to`](Firestore::reporting_to). Nobody, by
    /// default, which is what makes reporting free for a caller who never
    /// asked for it.
    attempts: Attempts,
}

impl Firestore {
    /// The document at `path` in `project`'s default database.
    ///
    /// `path` is collection-then-document — `config/db`, or
    /// `environments/prod/config/db` for a nested one — or a [`Keys`], for the
    /// several-documents form.
    ///
    /// The document is wrapped under the section key the configuration type
    /// uses, `"db"` by default; change it with [`with_key`](Self::with_key).
    /// Several documents all land under that one key and merge, later winning.
    #[must_use]
    pub fn new(project: impl Into<String>, path: impl Into<Keys>) -> Self {
        Self {
            project: project.into(),
            database: "(default)".to_owned(),
            keys: trimmed(path.into()),
            key: "db".to_owned(),
            auth: Auth::Emulator,
            session: auth::Session::new(),
            endpoint: None,
            timeout: DEFAULT_TIMEOUT,
            agent: None,
            tls: None,
            default_agent: std::sync::OnceLock::new(),
            attempts: Attempts::default(),
        }
    }

    /// The section key to wrap the document under.
    ///
    /// Must match the key the config type's `builder(..)` was given.
    #[must_use]
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = key.into();
        self
    }

    /// A database other than `(default)`.
    #[must_use]
    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = database.into();
        self
    }

    /// How to obtain an access token.
    ///
    /// Defaults to [`Auth::Emulator`], which sends none — right for the
    /// emulator and wrong for anything else, so a real deployment always names
    /// one.
    #[must_use]
    pub fn with_auth(mut self, auth: Auth) -> Self {
        self.auth = auth;
        self.session.invalidate();
        self
    }

    /// A different API endpoint.
    ///
    /// What the Firestore emulator needs: `FIRESTORE_EMULATOR_HOST` is
    /// `127.0.0.1:8080`, and this takes `http://127.0.0.1:8080`.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into().trim_end_matches('/').to_owned());
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
    /// It covers fetching a token from the metadata server too: that request
    /// goes through the same client, and a token fetch that hangs stalls the
    /// read behind it.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        // The cached fallback client baked in the old timeout.
        self.default_agent = std::sync::OnceLock::new();
        self
    }

    /// Uses an HTTP client the program already has.
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
    /// # use dynamic_config_firestore::{Firestore, TlsConfig};
    /// let firestore = Firestore::new("my-project", "config/db")
    ///     .with_endpoint("https://firestore.internal")
    ///     .with_tls(TlsConfig::new().with_ca_certificate_file("/etc/ssl/private-ca.pem"));
    /// ```
    ///
    /// Firestore expresses all of it: a CA from a file or from bytes, and a
    /// client certificate from either. A CA replaces the platform trust store
    /// rather than adding to it, so a deployment that needs both puts both in
    /// the file.
    ///
    /// Against Google's own endpoint this is rarely what you want — their
    /// certificates chain to a public authority the platform already trusts.
    /// It is for the deployments that do not go there directly: an enterprise
    /// TLS-inspecting proxy, or an emulator behind
    /// [`with_endpoint`](Self::with_endpoint) with a certificate of its own.
    ///
    /// There is no way to turn verification off; [`TlsConfig`]'s own
    /// documentation argues that one.
    ///
    /// **Nothing is read here.** The files are opened when the first request
    /// builds the client, so a missing CA is an error naming the path.
    ///
    /// # With `with_agent`
    ///
    /// Setting both is **refused**, at the first request, naming both calls.
    /// An agent already carries a complete TLS configuration, so "apply this
    /// too" has no meaning that is not a guess — and the guess that loses
    /// silently discards a CA.
    #[must_use]
    pub fn with_tls(mut self, tls: TlsConfig) -> Self {
        self.tls = Some(tls);
        // The cached fallback client baked in the old configuration.
        self.default_agent = std::sync::OnceLock::new();
        self
    }

    /// Reports the watch loop's *failed* attempts to `sink`.
    ///
    /// A watch loop is the half of a store `dynamic-config` cannot otherwise
    /// see. [`RemoteSink::apply`] records a delivery, so a working watch keeps
    /// [`RemoteStatus`] current — but a loop whose poll is failing, whose
    /// document was deleted or whose access token was refused delivers
    /// nothing, and without this says nothing: `dynamic_config_remote_up`
    /// would report the last *delivery* rather than the last *attempt*, and a
    /// project that stopped answering an hour ago would look healthy until
    /// something called `refresh_remote`.
    ///
    /// ```no_run
    /// # use dynamic_config::Watching;
    /// # use dynamic_config_firestore::{Auth, Firestore};
    /// # use std::time::Duration;
    /// # struct DbConfig;
    /// # impl DbConfig {
    /// #     fn remote_sink() -> dynamic_config::RemoteSink { unimplemented!() }
    /// # }
    /// # fn example(watching: Watching) -> Result<(), dynamic_config::Error> {
    /// let sink = DbConfig::remote_sink();
    ///
    /// Firestore::new("my-project", "config/db")
    ///     .with_auth(Auth::metadata_server())
    ///     .reporting_to(sink)
    ///     .watch(&watching, Duration::from_secs(30), move |document| sink.apply(document))
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

    /// Calls `on_change` when the document's update time moves, checking every
    /// `interval`.
    ///
    /// Firestore *can* push — the real-time API is a gRPC stream — and this
    /// deliberately does not use it: that would put a gRPC stack in a crate
    /// whose whole point is a plain HTTP read. Polling reads one small document
    /// and compares `updateTime`, which for a configuration document checked
    /// every thirty seconds is a rounding error against a project's quota.
    ///
    /// The current value is **not** delivered at startup, for the same reason a
    /// file watcher does not report an edit when it starts.
    ///
    /// A failed check does not end the watch. `stop` is noticed within a
    /// quarter second whatever `interval` is. Surviving a failure quietly is
    /// not the same as hiding it: [`reporting_to`](Self::reporting_to) hands
    /// each failed attempt to a [`RemoteSink`], so a loop that has been
    /// failing for an hour stops reporting the store as healthy.
    ///
    /// # Errors
    ///
    /// If the document comes back without an `updateTime` — there is then
    /// nothing to compare, so every tick would find "no change" and the watch
    /// would silently never fire. Or if `on_change` returns an error, which
    /// ends the watch. Transport failures do not surface here; they are
    /// retried.
    pub fn watch<F>(
        &self,
        watching: &Watching,
        interval: Duration,
        mut on_change: F,
    ) -> Result<(), Error>
    where
        F: FnMut(Fetched) -> Result<(), Error>,
    {
        // Refused before the first tick, so a multi-document source fails at
        // `watch` rather than never firing: the loop below treats a failed
        // read as a blip worth waiting out, which is right for a network and
        // wrong for a configuration mistake. Returned and recorded nowhere:
        // nothing has been asked of Firestore yet, and `reachable()` is
        // *whether the store answered the last time it was asked*.
        self.single_path()?;

        let mut seen: Option<String> = None;

        while watching.keep_going() {
            // A failed read — a blip, an expired token, a document briefly
            // unreachable — does not reach the caller: that is what a watch
            // exists to survive. It is still recorded, because a poll that
            // has been failing since yesterday and a document that simply has
            // not changed deliver the same nothing, and only this tells the
            // two apart.
            match self.read() {
                Ok((document, updated)) => {
                    // No `updateTime` means no way to ever detect a change:
                    // every tick would compare nothing to nothing and find "no
                    // change", and the watch would sit silent forever. A
                    // server answering like that is misconfigured, and that is
                    // reported, not waited out — and recorded on the way out,
                    // since a watch that has ended is a configuration that has
                    // stopped updating for good.
                    let Some(updated) = updated else {
                        let error = Error::remote(format!(
                            "{}: the document has no `updateTime`, so changes cannot be detected; is this a real Firestore?",
                            self.describe()
                        ));

                        self.attempts.failed(&error);

                        return Err(error);
                    };

                    // The first read records the time without firing: the
                    // document it names is the one the caller already has.
                    if seen.is_none() {
                        seen = Some(updated);
                    } else if seen.as_deref() != Some(&*updated) {
                        seen = Some(updated);

                        // A callback that refuses is deliberately not a failed
                        // attempt: the store answered and the document arrived.
                        // What the callback then did with it is
                        // `ConfigStatus`'s business, and `RemoteSink::apply`
                        // already records it there.
                        guarded(&mut on_change, document, &self.describe())?;
                    }
                }
                Err(error) => self.attempts.failed(&error),
            }

            watching.sleep_for(interval);
        }

        Ok(())
    }

    /// The one document a watch follows, and the `updateTime` it was read at.
    fn read(&self) -> Result<(Fetched, Option<String>), Error> {
        let path = self.single_path()?;
        let body = self.get(&self.url(path))?;

        let (document, updated) = self.section_of(&body, path)?;

        Ok((Fetched::new(document, Format::Json), updated))
    }

    /// One document's `fields`, wrapped under the section key, and its
    /// `updateTime`.
    ///
    /// The wrapping happens per document rather than after the merge because
    /// that is what makes the merge mean something: two documents supplying
    /// `host` collide at `db.host`, which is the path a reader of the
    /// configuration would name, not at a bare `host` that belongs to nothing.
    fn section_of(
        &self,
        body: &serde_json::Value,
        path: &str,
    ) -> Result<(String, Option<String>), Error> {
        let fields = body.get("fields").ok_or_else(|| {
            Error::remote(format!(
                "{}: `{path}` answered without `fields`; is that a document?",
                self.describe()
            ))
        })?;

        let values = value::to_json(fields);
        let document = serde_json::json!({ &self.key: values });

        let updated = body
            .get("updateTime")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);

        Ok((document.to_string(), updated))
    }

    /// The one document this source reads, or an error saying it reads
    /// several.
    ///
    /// A watch here compares `updateTime`, and that belongs to a document: a
    /// set of them has none, and following one member's would fire on that
    /// member and miss every other.
    fn single_path(&self) -> Result<&str, Error> {
        match &self.keys {
            Keys::One(path) => Ok(path),
            Keys::Several(_) => Err(Error::remote(format!(
                "{}: a source that reads several documents cannot be watched; \
                 poll `refresh_remote()` on a timer instead",
                self.describe()
            ))),
        }
    }

    /// What two of this source's documents supplying one field means.
    ///
    /// Only [`Overlap::LaterWins`] here: a caller who wrote the list wrote the
    /// precedence with it, and there is no collection form whose order nobody
    /// chose.
    fn overlap(&self) -> Overlap {
        Overlap::LaterWins
    }

    /// The `(path, document)` pairs this source reads, in merge order.
    fn documents(&self) -> Result<Vec<(String, String)>, Error> {
        match &self.keys {
            Keys::One(path) => {
                let body = self.get(&self.url(path))?;

                Ok(vec![(path.clone(), self.section_of(&body, path)?.0)])
            }
            Keys::Several(paths) => self.batch(paths),
        }
    }

    /// Every named document, in one `:batchGet`, put back into call order.
    ///
    /// The reordering is not tidiness. `BatchGetDocuments` states that the
    /// documents are not returned in the order they were asked for, and the
    /// order the caller wrote *is* the precedence — so a merge in reply order
    /// would make which value wins a property of the service's mood.
    fn batch(&self, paths: &[String]) -> Result<Vec<(String, String)>, Error> {
        let names: Vec<String> = paths.iter().map(|path| self.name_of(path)).collect();

        let answered = self.post(
            &self.batch_url(),
            &serde_json::json!({ "documents": names }),
        )?;

        let entries = answered.as_array().ok_or_else(|| {
            Error::remote(format!(
                "{}: the batch response is not a list of results",
                self.describe()
            ))
        })?;

        // A server can answer with any number of results, in any order, for
        // documents nobody asked about. Held by name and looked up afterwards,
        // so every one of those is refused rather than merged.
        let mut held: Vec<(String, String)> = Vec::with_capacity(entries.len());

        for entry in entries {
            if let Some(missing) = entry.get("missing").and_then(serde_json::Value::as_str) {
                // Fail-whole, not merge-what-came-back: a section quietly
                // missing half of itself is worse than a refresh that failed.
                return Err(Error::remote(format!(
                    "{}: `{}` holds no document",
                    self.describe(),
                    self.path_of(missing)
                )));
            }

            let found = entry.get("found").ok_or_else(|| {
                Error::remote(format!(
                    "{}: a batch result is neither `found` nor `missing`",
                    self.describe()
                ))
            })?;

            let name = found
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    Error::remote(format!(
                        "{}: a batch result names no document",
                        self.describe()
                    ))
                })?;

            let path = self.path_of(name);

            if !paths.contains(&path) {
                return Err(Error::remote(format!(
                    "{}: the store answered with `{path}`, which is not one of \
                     the documents that were asked for",
                    self.describe()
                )));
            }

            if held.iter().any(|(held, _)| *held == path) {
                return Err(Error::remote(format!(
                    "{}: the store answered for `{path}` twice",
                    self.describe()
                )));
            }

            held.push((path.clone(), self.section_of(found, &path)?.0));
        }

        paths
            .iter()
            .map(|path| {
                held.iter()
                    .find(|(held, _)| held == path)
                    .cloned()
                    .ok_or_else(|| {
                        Error::remote(format!(
                            "{}: the store answered nothing at all about `{path}`",
                            self.describe()
                        ))
                    })
            })
            .collect()
    }

    /// One GET, retried once if the token turned out to be dead.
    fn get(&self, url: &str) -> Result<serde_json::Value, Error> {
        match self.get_once(url) {
            Err(CallError::Unauthorized(_)) if self.can_refresh() => {
                // The proactive refresh should have caught an expiring token,
                // but clocks skew. One fresh token and one retry — not a loop.
                self.session.invalidate();

                self.get_once(url).map_err(CallError::into_error)
            }
            outcome => outcome.map_err(CallError::into_error),
        }
    }

    /// One POST, with the same one-fresh-token retry a GET gets.
    fn post(&self, url: &str, body: &serde_json::Value) -> Result<serde_json::Value, Error> {
        match self.post_once(url, body) {
            Err(CallError::Unauthorized(_)) if self.can_refresh() => {
                self.session.invalidate();

                self.post_once(url, body).map_err(CallError::into_error)
            }
            outcome => outcome.map_err(CallError::into_error),
        }
    }

    /// Whether a refused token can be traded for a fresh one.
    ///
    /// Only the metadata server can mint another: a supplied access token is
    /// whatever it is, and the emulator sends none at all.
    fn can_refresh(&self) -> bool {
        matches!(self.auth, Auth::MetadataServer { .. })
    }

    fn get_once(&self, url: &str) -> Result<serde_json::Value, CallError> {
        // A TLS configuration that will not build is not a refused token, so
        // it must not become one: `Other` is what keeps a bad CA path out of
        // the refresh-and-retry path it would otherwise loop through.
        let agent = self.agent().map_err(CallError::Other)?;

        let mut request = agent.get(url);

        if let Some(token) = self.bearer()? {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }

        let response = request.call().map_err(|error| self.sorted(&error))?;

        Self::json(response, &self.describe())
    }

    fn post_once(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let agent = self.agent().map_err(CallError::Other)?;

        let mut request = agent.post(url);

        if let Some(token) = self.bearer()? {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }

        let response = request
            .send_json(body)
            .map_err(|error| self.sorted(&error))?;

        Self::json(response, &self.describe())
    }

    /// The access token to present, if this method has one.
    fn bearer(&self) -> Result<Option<String>, CallError> {
        let agent = self.agent().map_err(CallError::Other)?;

        self.session
            .token(&self.auth, agent)
            .map_err(CallError::Other)
    }

    /// Sorts one of `ureq`'s failures into a kind.
    ///
    /// Only the typed status decides it. `{error}` here is `ureq`'s own
    /// rendering of the status — never the request, so neither the
    /// `Authorization` header nor a request body can ride along.
    fn sorted(&self, error: &ureq::Error) -> CallError {
        let described = format!("{}: {error}", self.describe());

        match error {
            ureq::Error::StatusCode(401) => CallError::Unauthorized(Error::auth(described)),
            ureq::Error::StatusCode(403) => CallError::Forbidden(Error::auth(described)),
            _ => CallError::Other(Error::remote(described)),
        }
    }

    /// The response body, as JSON.
    fn json(
        mut response: ureq::http::Response<ureq::Body>,
        described: &str,
    ) -> Result<serde_json::Value, CallError> {
        response.body_mut().read_json().map_err(|error| {
            CallError::Other(Error::remote(format!(
                "{described}: the response was not JSON: {error}"
            )))
        })
    }

    /// The API host: the emulator's if one was named, otherwise Google's.
    fn host(&self) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| "https://firestore.googleapis.com".to_owned())
    }

    /// Where the documents of this database live.
    fn root(&self) -> String {
        format!(
            "projects/{}/databases/{}/documents",
            self.project, self.database
        )
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1/{}/{path}", self.host(), self.root())
    }

    fn batch_url(&self) -> String {
        format!("{}/v1/{}:batchGet", self.host(), self.root())
    }

    /// The full resource name `:batchGet` asks for.
    fn name_of(&self, path: &str) -> String {
        format!("{}/{path}", self.root())
    }

    /// The path inside the database, from a full resource name.
    ///
    /// A name that is not under this database's documents is returned whole,
    /// so the caller compares it against what was asked for and refuses it —
    /// rather than being silently trimmed into something that matches.
    fn path_of(&self, name: &str) -> String {
        let root = format!("{}/", self.root());

        name.strip_prefix(&root).unwrap_or(name).to_owned()
    }

    /// The HTTP client: the caller's if they supplied one, otherwise ours.
    ///
    /// Ours is built once and kept: an agent owns a connection pool and a TLS
    /// session cache, and rebuilding it per request would pay a handshake per
    /// poll tick.
    fn agent(&self) -> Result<&ureq::Agent, Error> {
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

            return Ok(agent);
        }

        self.default_agent
            .get_or_init(|| match &self.tls {
                Some(tls) => tls::agent(tls, self.timeout, &self.describe())
                    .map_err(|error| error.to_string()),
                None => Ok(ureq::Agent::config_builder()
                    .timeout_global(Some(self.timeout))
                    .build()
                    .new_agent()),
            })
            .as_ref()
            .map_err(Error::remote)
    }
}

// Hand-written, never derived: a derive would print every field, and the
// fields include credentials. `{:?}` reaching a log is an ordinary accident —
// a `dbg!`, a `tracing::debug!(?source)` — and an accident must not disclose
// a secret. The other store crates follow the same rule.
impl std::fmt::Debug for Firestore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Firestore")
            .field("project", &self.project)
            .field("database", &self.database)
            .field("keys", &self.keys)
            .field("key", &self.key)
            .field("endpoint", &self.endpoint)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl RemoteSource for Firestore {
    fn fetch(&self) -> Result<Fetched, Error> {
        let documents = self.documents()?;

        // Already put back into call order, which is the order the rule wants
        // — so nothing is sorted here.
        documents::merged(&documents, Format::Json, self.overlap(), &self.describe())
    }

    fn describe(&self) -> String {
        // The endpoint tells the emulator apart from the real service — the
        // question an error actually raises. The auth method is not part of
        // *where*, so it no longer rides along.
        match &self.endpoint {
            Some(endpoint) => format!(
                "firestore {endpoint} {}/{}",
                self.project,
                self.keys.describe()
            ),
            None => format!("firestore {}/{}", self.project, self.keys.describe()),
        }
    }
}

/// Every path with its slashes trimmed, the way one path always was.
///
/// A leading or trailing slash in a document path produces a URL with a double
/// slash in it, which Firestore answers with a 404 about a document nobody
/// meant to ask for.
fn trimmed(keys: Keys) -> Keys {
    match keys {
        Keys::One(path) => Keys::One(path.trim_matches('/').to_owned()),
        Keys::Several(paths) => Keys::Several(
            paths
                .into_iter()
                .map(|path| path.trim_matches('/').to_owned())
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_a_credential() {
        let source = Firestore::new("my-project", "config/db")
            .with_auth(Auth::access_token("hunter2-access-token"));

        let printed = format!(
            "{source:?} {:?}",
            Auth::access_token("hunter2-access-token")
        );

        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("AccessToken(***)"), "{printed}");
    }
}
