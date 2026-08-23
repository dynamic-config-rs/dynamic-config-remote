//! Read [`dynamic-config`] configuration from HashiCorp Vault.
//!
//! Vault's KV v2 store speaks plain HTTP, so this implements the **blocking**
//! [`RemoteSource`] trait: nothing here needs an async runtime, and neither
//! does using it.
//!
//! ```no_run
//! use dynamic_config_vault::Vault;
//!
//! # struct DbConfigBuilder;
//! # impl DbConfigBuilder {
//! #     fn init(&self) -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! # struct DbConfig;
//! # impl DbConfig {
//! #     fn set_remote(_: Vault) {}
//! #     fn refresh_remote() -> Result<(), dynamic_config::Error> { Ok(()) }
//! #     fn builder(_: &str) -> DbConfigBuilder { DbConfigBuilder }
//! # }
//! DbConfig::set_remote(
//!     Vault::new("https://vault.internal:8200", "secret", "myapp/db")
//!         .with_token(std::env::var("VAULT_TOKEN")?),
//! );
//!
//! // Fetching is explicit; the load that follows touches no network.
//! DbConfig::refresh_remote()?;
//! DbConfig::builder("db").init()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # What it reads
//!
//! `GET {address}/v1/{mount}/data/{path}`, and takes `data.data` — the value
//! half of a KV v2 response. That object becomes the configuration document,
//! so a secret stored as `{"host": "db", "port": 5432}` maps onto a struct
//! with those fields.
//!
//! The document is handed over as JSON with the section key wrapped around it,
//! because Vault stores the section's *contents* rather than a whole
//! configuration file.
//!
//! # Several paths as one section
//!
//! One section can be split across several secrets, and [`Keys`] says which:
//!
//! ```no_run
//! # use dynamic_config_vault::{Keys, Vault};
//! # let address = "https://vault.internal:8200";
//! // Merged in the order given — later wins — and all under the one section key.
//! let vault = Vault::new(
//!     address,
//!     "secret",
//!     Keys::several(["myapp/db-defaults", "myapp/db-credentials"]),
//! );
//! ```
//!
//! That is what Vault's shape actually offers, and the two halves of the
//! sentence are worth separating:
//!
//! - **A named list is one request per path.** KV v2 reads one secret at a
//!   time — there is no batch read of a caller-chosen set — so the list is
//!   **not** read atomically: a write landing between two of the requests can
//!   produce a section that never existed as a whole. It is also one *audited*
//!   read per path per fetch, which somebody pays for.
//! - **Every path lands under the same section key**, because that is what a
//!   Vault secret is: the contents of a section, not a document. So a list is
//!   layering — a shared secret and an override, or a public half and a
//!   restricted half whose difference is a policy on the path, which is the
//!   thing Vault has that the document stores do not.
//!
//! **There is deliberately no prefix form.** KV v2 has `LIST`, so the missing
//! piece is not the protocol; it is the mapping. Folding a whole subtree into
//! one section makes `myapp/db` and `myapp/server` collide on `host` — the
//! ordinary layout, refused — and naming a sub-section after each secret's
//! path would invent a convention no other store here has, and would make a
//! list of one path mean something different from one path. A deployment that
//! wants several sections installs one source per section, which is what it
//! did before. [`dynamic-config-consul`] and [`dynamic-config-s3`] store whole
//! documents, and read prefixes for that reason.
//!
//! Two consequences the multi-path form shares with the rest of the family:
//!
//! - **Provenance becomes store-grained.** The merged section is one layer, so
//!   `source_of` names the set rather than which path supplied a value.
//! - **One unreadable path fails the whole fetch.** A section quietly missing
//!   half of itself is worse than a refresh that failed and left the last
//!   document serving.
//!
//! # Watching
//!
//! Vault is the one store here that cannot tell you when something changed:
//! there is no watch, no blocking query, no stream. So [`Vault::watch`] polls —
//! and says so, rather than dressing a timer up as a subscription.
//!
//! What it does *not* do is pull the secret every tick. KV v2 keeps a version
//! counter in its metadata, so the loop asks the metadata endpoint for
//! `current_version` and only reads the secret when that number moves. A secret
//! that has not changed is never transferred, never decrypted, and never
//! written to an audit log as a read.
//!
//! A **multi-path source cannot be watched**, and refuses at
//! [`watch`](Vault::watch) rather than pretending to: the version counter it
//! polls belongs to one secret, and a set of secrets has no counter of its own.
//! Poll `refresh_remote()` on a timer instead.
//!
//! ```no_run
//! # use dynamic_config::RemoteWatch;
//! # use dynamic_config_vault::Vault;
//! # use std::time::Duration;
//! # struct Sink;
//! # impl Sink {
//! #     fn apply(&self, _: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! # fn example(vault: Vault) {
//! # let sink = Sink;
//! let watch = RemoteWatch::new();
//! let watching = watch.watching();
//!
//! std::thread::spawn(move || {
//!     vault.watch(&watching, Duration::from_secs(30), move |document| sink.apply(document))
//! });
//!
//! // Dropping `watch` — or calling `watch.stop()` — ends the loop.
//! # }
//! ```
//!
//!
//! # Every failure branch of the watch loop, and what it reports
//!
//! A watch is the half of a store `dynamic-config` cannot see, and
//! [`reporting_to`](Vault::reporting_to) is what lets it speak: the sink the
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
//! | the source reads several paths, so it cannot be watched | no — rule 3: nothing has been asked of Vault |
//! | the version check fails and may yet come good — a sealed Vault, a network blip | **yes**, and the loop waits out the interval |
//! | the mount is not KV v2, so there is no version to poll | **yes**, and the watch ends |
//! | the read after a version move fails | **yes**, and `seen` is left where it was so the next tick tries again |
//! | the first tick, or a version that has not moved | no — Vault answered |
//! | `on_change` refuses the document | no — Vault answered; `apply` counted the delivery, and what the document did next is `ConfigStatus`'s half |
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config
//! [`dynamic-config-consul`]: https://docs.rs/dynamic-config-consul
//! [`dynamic-config-s3`]: https://docs.rs/dynamic-config-s3

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::time::Duration;

use dynamic_config::{
    Error, Fetched, Format, Lease, Pace, RemoteSink, RemoteSource, RenewableSource, Revision,
    WatchCapability, Watching,
};
use dynamic_config_store_core::attempts::Attempts;
use dynamic_config_store_core::credential::Issued;
use dynamic_config_store_core::documents::{self, Overlap};
use dynamic_config_store_core::guarded;

pub mod auth;
mod tls;
pub mod transit;

pub use auth::Auth;
use auth::{Session, Token};
pub use transit::Transit;

/// A private certificate authority and a client certificate, as data.
///
/// The shared vocabulary all seven store crates take, so that reaching TLS
/// never means naming `ureq`'s types — see [`with_tls`](Vault::with_tls).
pub use dynamic_config_store_core::tls::TlsConfig;

/// How long to wait for Vault before giving up.
///
/// A configuration fetch that hangs is worse than one that fails: the caller
/// can retry a failure.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a source reads: one path, or several named ones.
///
/// Every constructor takes one, and a bare `&str` or `String` is
/// [`Keys::one`] — so the single-path spelling every caller already wrote keeps
/// working unchanged.
///
/// There is no prefix variant, and that is a decision rather than an omission:
/// KV v2 has `LIST`, but a secret is a section's *contents*, so a subtree
/// folded into one section collides on every field name two secrets share. The
/// crate documentation says the whole of it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Keys {
    /// One path, whose fields are the whole section.
    One(String),
    /// Several named paths, merged **in the order given — later wins**.
    ///
    /// The rule a list of `.file(..)` calls already teaches: the caller wrote
    /// the list, so the list is the precedence. One request per path, because
    /// KV v2 has no batch read of a caller-chosen set — so the set is **not**
    /// read atomically, and every path costs an audited read per fetch.
    Several(Vec<String>),
}

impl Keys {
    /// One path, whose fields are the whole section.
    #[must_use]
    pub fn one(path: impl Into<String>) -> Self {
        Self::One(path.into())
    }

    /// Several named paths, merged in the order given — later wins.
    #[must_use]
    pub fn several<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Several(paths.into_iter().map(Into::into).collect())
    }

    /// The paths as a slice, in the order they are read.
    fn named(&self) -> &[String] {
        match self {
            Self::One(path) => std::slice::from_ref(path),
            Self::Several(paths) => paths,
        }
    }

    /// How a diagnostic names what this source reads.
    ///
    /// One path renders as the path itself, so every message a single-path
    /// source has ever produced is unchanged.
    fn describe(&self) -> String {
        match self {
            Self::One(path) => path.clone(),
            Self::Several(paths) => format!("paths {}", paths.join(", ")),
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
/// error message mentioning a path like `myapp/403` must not read as a
/// refused token.
enum CallError {
    /// Vault said 403: the token is the problem, and a fresh login might be
    /// the cure. Carries an [`ErrorKind::Auth`] error, because if the fresh
    /// login is refused too then waiting will not help — which is the one
    /// thing a watch loop needs to know.
    ///
    /// [`ErrorKind::Auth`]: dynamic_config::ErrorKind::Auth
    Forbidden(Error),
    /// Everything else — network, timeouts, a sealed Vault. A new token fixes
    /// none of it, and a sealed Vault does un-seal, so it stays `Remote`.
    /// Vault answers a denied or expired token with 403 and nothing else, so
    /// there is no 401 arm to write here; a 401 in front of a Vault is a
    /// proxy, and a proxy's verdict is not the store's.
    Other(Error),
}

impl CallError {
    fn into_error(self) -> Error {
        match self {
            Self::Forbidden(error) | Self::Other(error) => error,
        }
    }
}

/// Why a version check failed, sorted by whether waiting can help.
enum CheckError {
    /// The mount has no version counter: a v1 mount, or not a KV mount at
    /// all. No number of retries will grow one.
    NotKv2(Error),
    /// The check itself failed — network, a sealed Vault, an expired token.
    /// The next tick may well succeed, so the loop waits rather than ending —
    /// and reports the attempt through
    /// [`reporting_to`](Vault::reporting_to), because a tick that keeps
    /// failing is the case a `remote_up` of 1 would be lying about.
    Transient(Error),
}

/// Which shape of secret this source reads.
///
/// Vault's engines answer in two different shapes, and a source that
/// guessed between them would turn a mistyped mount into a puzzle. So the
/// caller says which, and the default is the one every existing user has.
///
/// ```text
/// Kv2      GET /v1/{mount}/data/{path}   values under `data.data`, a version
/// Dynamic  GET /v1/{mount}/{path}        values under `data`, a lease
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Mode {
    /// The KV version 2 engine: a document somebody wrote, with a version.
    #[default]
    Kv2,
    /// An engine that *mints* a credential — `database/creds`, `pki/issue`,
    /// `aws/creds` — answering with a lease that can be renewed and revoked.
    ///
    /// There is no version counter and no metadata endpoint here, so a
    /// source in this mode reports
    /// [`Interval`](dynamic_config::WatchCapability::Interval): the only way
    /// to learn that a credential changed is that a new one was issued.
    Dynamic,
}

/// A secret in Vault's KV v2 store, as a configuration source.
///
/// Not `Clone`: the session holds the current token, and two clones sharing a
/// path while logging in separately would double the login traffic and halve
/// the usefulness of the cache. Wrap it in an `Arc` if two places need one.
pub struct Vault {
    address: String,
    mount: String,
    mode: Mode,
    keys: Keys,
    key: String,
    auth: Auth,
    session: Session,
    namespace: Option<String>,
    timeout: Duration,
    agent: Option<ureq::Agent>,
    /// What [`with_tls`](Vault::with_tls) was given, translated into an agent
    /// on first use.
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
    /// How many requests this source may have out at once.
    ///
    /// A watch, a resync, an auth refresh and a lease renewal can all be in
    /// flight together, and five thousand pods doing that to one Vault is
    /// the number this exists for.
    concurrency: dynamic_config_store_core::concurrency::StoreRuntimePolicy,
    /// Where [`watch`](Vault::watch) reports a tick that came back with
    /// nothing; see [`reporting_to`](Vault::reporting_to). Nobody, by default,
    /// which is what makes reporting free for a caller who never asked for it.
    attempts: Attempts,
}

/// The lease a dynamic-secret response was issued under.
///
/// Modelled on `token_from`, which already reads the same two fields out of
/// the `auth` block — including the rule that zero means *never expires*
/// rather than *already expired*.
fn lease_of(body: &serde_json::Value) -> Option<Lease> {
    let id = body.get("lease_id").and_then(serde_json::Value::as_str)?;

    // An empty `lease_id` is Vault's way of saying "this one is not leased",
    // and a lease nobody can name is a lease nobody can renew or revoke.
    if id.is_empty() {
        return None;
    }

    let ttl = body
        .get("lease_duration")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    let ttl = certificate_life(body).map_or(Duration::from_secs(ttl), |until| {
        // Whichever runs out first. A PKI role can hand back a lease longer
        // than the certificate it issued — the lease is an accounting
        // record, the `notAfter` is what a TLS peer enforces — and a
        // credential re-issued after its certificate stopped verifying is
        // an outage that looks like a network fault.
        if ttl == 0 {
            until
        } else {
            until.min(Duration::from_secs(ttl))
        }
    });

    Some(Lease {
        id: id.to_owned(),
        ttl,
        renewable: body
            .get("renewable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

/// How long a `pki/issue` certificate has left, where the response is one.
///
/// Vault answers `data.expiration` as the certificate's `notAfter` in
/// seconds since the epoch, so the certificate's own clock is readable
/// without an X.509 parser — and without a dependency whose whole job would
/// be re-deriving a number the server already sent.
///
/// `None` for every other engine: a database credential has no `expiration`
/// and its lease is the only clock there is.
fn certificate_life(body: &serde_json::Value) -> Option<Duration> {
    let expiration = body
        .get("data")?
        .get("expiration")
        .and_then(serde_json::Value::as_i64)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();

    let remaining = expiration - i64::try_from(now).ok()?;

    // Wall clocks, and two of them: this is the one place in the crate that
    // cannot use an `Instant`, because the number came from another machine.
    // A certificate that looks already expired *from here* is therefore far
    // more likely to be a skewed clock than an expired certificate — and
    // answering `None` falls back to the lease rather than re-issuing in a
    // tight loop against a Vault that thinks everything is fine.
    //
    // Zero is refused for the same reason as negative, and for one more: a
    // zero TTL would schedule the next re-issue immediately, which is the
    // fetch storm this guard exists to prevent.
    if remaining <= 0 {
        return None;
    }

    u64::try_from(remaining).ok().map(Duration::from_secs)
}

/// One read, in the three things a caller can want from it.
struct Reading {
    document: String,
    /// Zero on any engine that has no version counter.
    version: u64,
    /// Present only for an engine that issues leases.
    lease: Option<Lease>,
}

impl Vault {
    /// A secret at `{mount}/{path}`, served by the Vault at `address`.
    ///
    /// `path` is a path — `"myapp/db"` — or a [`Keys`], for the several-paths
    /// form.
    ///
    /// The document is wrapped under the section key the configuration type
    /// uses — `"db"` by default, changed with [`with_key`](Self::with_key) —
    /// because Vault stores a section's contents, not a whole file. Several
    /// paths all land under that one key and merge, later winning.
    pub fn new(
        address: impl Into<String>,
        mount: impl Into<String>,
        path: impl Into<Keys>,
    ) -> Self {
        Self {
            address: address.into().trim_end_matches('/').to_owned(),
            mount: mount.into(),
            mode: Mode::Kv2,
            keys: path.into(),
            key: "db".to_owned(),
            // No credentials until one is supplied; the first read then says so
            // rather than sending an unauthenticated request and reporting
            // Vault's answer to it.
            auth: Auth::Token(String::new()),
            session: Session::new(),
            namespace: None,
            timeout: DEFAULT_TIMEOUT,
            agent: None,
            tls: None,
            default_agent: std::sync::OnceLock::new(),
            concurrency: dynamic_config_store_core::concurrency::StoreRuntimePolicy::default(),
            attempts: Attempts::default(),
        }
    }

    /// Reads a *dynamic* secret rather than a KV v2 document.
    ///
    /// Turns the read into `GET /v1/{mount}/{path}` — no `data/` segment —
    /// takes the values from `data` rather than `data.data`, and keeps the
    /// `lease_id`, `lease_duration` and `renewable` the engine answered
    /// with. That last part is the point: a dynamic credential belongs to
    /// this reader alone and expires, so somebody has to renew it and hand
    /// it back, and nobody can do either without the lease.
    ///
    /// Explicit rather than detected from the response, because a mistyped
    /// mount would otherwise fall quietly into the other mode and read as a
    /// puzzle instead of an error.
    ///
    /// See [`RenewableSource`] for the other half.
    #[must_use]
    pub fn with_dynamic(mut self) -> Self {
        self.mode = Mode::Dynamic;
        self
    }

    /// The section key to wrap the secret under.
    ///
    /// Must match the key the config type's `builder(..)` was given.
    #[must_use]
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = key.into();
        self
    }

    /// A token somebody already obtained.
    ///
    /// Shorthand for `with_auth(Auth::token(..))`. A renewable token is still
    /// renewed; a token that stops working cannot be replaced, because there
    /// are no credentials here to log in again with. Every other [`Auth`] can.
    #[must_use]
    pub fn with_token(self, token: impl Into<String>) -> Self {
        self.with_auth(Auth::token(token))
    }

    /// How to obtain a token.
    ///
    /// ```no_run
    /// # use dynamic_config_vault::{Auth, Vault};
    /// // In Kubernetes, with no secret to distribute at all.
    /// let vault = Vault::new("https://vault.internal:8200", "secret", "myapp/db")
    ///     .with_auth(Auth::kubernetes("myapp"));
    ///
    /// // Or AppRole, for a service outside it.
    /// let vault = Vault::new("https://vault.internal:8200", "secret", "myapp/db")
    ///     .with_auth(Auth::app_role(
    ///         std::env::var("VAULT_ROLE_ID").unwrap(),
    ///         std::env::var("VAULT_SECRET_ID").unwrap(),
    ///     ));
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
    /// [`with_timeout`](Self::with_timeout).
    ///
    /// ```no_run
    /// # use dynamic_config_vault::Vault;
    /// # fn example(agent: ureq::Agent) {
    /// let vault = Vault::new("https://vault.internal:8200", "secret", "myapp/db")
    ///     .with_agent(agent);
    /// # }
    /// ```
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
    /// # use dynamic_config_vault::{TlsConfig, Vault};
    /// let vault = Vault::new("https://vault.internal:8200", "secret", "myapp/db")
    ///     .with_token(std::env::var("VAULT_TOKEN").unwrap())
    ///     .with_tls(
    ///         TlsConfig::new()
    ///             .with_ca_certificate_file("/etc/ssl/private-ca.pem")
    ///             .with_client_certificate_files("/etc/ssl/app.crt", "/etc/ssl/app.key"),
    ///     );
    /// ```
    ///
    /// Vault expresses all of it: a CA from a file or from bytes, and a client
    /// certificate from either. A CA replaces the platform trust store rather
    /// than adding to it — naming a private authority is saying the public
    /// ones do not apply to this host — so a deployment that needs both puts
    /// both in the file.
    ///
    /// There is no way to turn verification off. The reasoning is in
    /// [`TlsConfig`]'s own documentation, and the short version is that the
    /// answer to a self-signed server is to trust its certificate, not to stop
    /// checking.
    ///
    /// **Nothing is read here.** The files are opened when the first request
    /// builds the client, so a missing CA is an error naming the path rather
    /// than a panic in a builder chain — the same laziness every other
    /// constructor in this family has.
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
        // The cached fallback client baked in the old configuration.
        self.default_agent = std::sync::OnceLock::new();
        self
    }

    /// The Vault Enterprise namespace, if there is one.
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
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
    /// It bounds each request [`watch`](Self::watch) makes — the metadata
    /// check, and the read that follows a version change — not the loop,
    /// which runs until it is stopped.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        // The cached fallback client baked in the old timeout.
        self.default_agent = std::sync::OnceLock::new();
        self
    }

    /// Reports the watch loop's *failed* attempts to `sink`.
    ///
    /// A watch loop is the half of a store `dynamic-config` cannot otherwise
    /// see. [`RemoteSink::apply`] records a delivery, so a working watch keeps
    /// [`RemoteStatus`] current — but a loop whose metadata poll is failing,
    /// whose Vault is sealed or whose token was refused delivers nothing, and
    /// without this says nothing: `dynamic_config_remote_up` would report the
    /// last *delivery* rather than the last *attempt*, and a Vault that
    /// stopped answering an hour ago would look healthy until something called
    /// `refresh_remote`.
    ///
    /// That gap is wider here than anywhere else in this family, because this
    /// watch is a poll of a *version counter*: a secret that has not changed
    /// is never read, so a healthy Vault and a Vault whose token expired
    /// yesterday deliver exactly the same thing — nothing.
    ///
    /// ```no_run
    /// # use dynamic_config::Watching;
    /// # use dynamic_config_vault::Vault;
    /// # use std::time::Duration;
    /// # struct DbConfig;
    /// # impl DbConfig {
    /// #     fn remote_sink() -> dynamic_config::RemoteSink { unimplemented!() }
    /// # }
    /// # fn example(address: &str, watching: Watching) -> Result<(), dynamic_config::Error> {
    /// let sink = DbConfig::remote_sink();
    ///
    /// Vault::new(address, "secret", "myapp/db")
    ///     .with_token(std::env::var("VAULT_TOKEN").unwrap())
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

    /// Calls `on_change` when the secret's version moves, checking every
    /// `interval`.
    ///
    /// Polling, because Vault offers nothing better — and *metadata* polling,
    /// because reading a secret every thirty seconds to discover it has not
    /// changed is a poor thing to do to a secrets store. Each tick reads
    /// `{mount}/metadata/{path}` for `current_version`; only a new version
    /// triggers a read of the secret itself.
    ///
    /// The current value is **not** delivered at startup, for the same reason a
    /// file watcher does not report an edit when it starts. Fetch first if the
    /// starting value matters, which it usually does:
    ///
    /// ```no_run
    /// # use dynamic_config::{RemoteSource, RemoteWatch};
    /// # use dynamic_config_vault::Vault;
    /// # use std::time::Duration;
    /// # struct Sink;
    /// # impl Sink {
    /// #     fn apply(&self, _: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
    /// # }
    /// # fn example(vault: Vault, watching: dynamic_config::Watching) -> Result<(), dynamic_config::Error> {
    /// # let sink = Sink;
    /// sink.apply(vault.fetch()?)?;
    /// vault.watch(&watching, Duration::from_secs(30), move |document| sink.apply(document))
    /// # }
    /// ```
    ///
    /// A failed check does not end the watch — an expired token, a sealed
    /// Vault, a network blip — it waits out the interval and tries again. `stop`
    /// is noticed within a quarter second regardless of how long `interval` is.
    ///
    /// Surviving a failure quietly is not the same as hiding it:
    /// [`reporting_to`](Self::reporting_to) hands each failed attempt to a
    /// [`RemoteSink`], so a loop that has been failing for an hour stops
    /// reporting the store as healthy.
    ///
    /// # Errors
    ///
    /// If the source reads several paths: the counter this polls belongs to
    /// one secret, and a set of secrets has none of its own. Or if the mount
    /// turns out not to be KV v2: a v1 mount has no version counter, so every
    /// tick would find "no change" and the watch would silently never fire — a
    /// misconfiguration, reported as one. Or if `on_change` returns an error,
    /// which ends the watch — so a caller that wants to survive a bad document
    /// should log it and return `Ok`. Transport failures do not surface here;
    /// they are retried.
    pub fn watch<F>(
        &self,
        watching: &Watching,
        interval: Duration,
        mut on_change: F,
    ) -> Result<(), Error>
    where
        F: FnMut(Fetched) -> Result<(), Error>,
    {
        // Refused up front, so a multi-path source fails at `watch` rather
        // than on the first change, hours later. Returned and recorded
        // nowhere: nothing has been asked of Vault yet, and `reachable()` is
        // *whether the store answered the last time it was asked*.
        self.single_path()?;

        let mut seen: Option<u64> = None;

        // The waits are paced rather than flat. Two failures deserve it for
        // different reasons: a sealed Vault should not be polled at full
        // rate by every pod that depends on it, and a *deleted* secret is a
        // condition that will not resolve on its own — the loop keeps
        // watching, because it may be written again, but it stops asking
        // every interval forever.
        let mut pace = Pace::new(interval);

        while watching.keep_going() {
            match self.current_version() {
                // The first tick records the version without firing: the value
                // it names is the one the caller already has.
                Ok(version) if seen.is_none() => seen = Some(version),

                // A failed read leaves `seen` where it was on purpose, so the
                // next tick tries again rather than skipping the change.
                Ok(version) if seen != Some(version) => {
                    // The version is taken from the read itself, not from the
                    // check that preceded it: a write landing between the two
                    // would otherwise be delivered now and again on the next
                    // tick, as though the same document had changed twice.
                    match self.read() {
                        Ok((document, version)) => {
                            seen = Some(version);
                            pace.succeeded();

                            // A callback that refuses is deliberately not a
                            // failed attempt: Vault answered and the secret
                            // arrived. What the callback then did with it is
                            // `ConfigStatus`'s business, and `RemoteSink::apply`
                            // already records it there.
                            guarded(&mut on_change, document, &self.describe())?;
                        }
                        // `seen` is deliberately left where it was, so the next
                        // tick reads again rather than skipping the change —
                        // and the attempt is recorded, because a counter that
                        // moved and a secret that will not come back is the
                        // shape of a policy that stopped allowing the read.
                        Err(error) => {
                            pace.failed();
                            self.attempts.failed(&error);
                        }
                    }
                }

                // Not a KV v2 mount: there is no version counter to poll, so
                // "retry next tick" would run forever and deliver nothing.
                // A misconfiguration is reported, not waited out — and
                // recorded on the way out, since a watch that has ended is a
                // configuration that has stopped updating for good.
                Err(CheckError::NotKv2(error)) => {
                    self.attempts.failed(&error);

                    return Err(error);
                }

                // The check failed and may yet come good: waited out rather
                // than reported to the caller, recorded rather than swallowed.
                // This is the arm that made a sealed Vault look healthy.
                Err(CheckError::Transient(error)) => {
                    pace.failed();
                    self.attempts.failed(&error);
                }

                // Unchanged: the store answered, and there is nothing to say.
                Ok(_) => pace.succeeded(),
            }

            pace.wait(watching);
        }

        Ok(())
    }

    /// The one path this source reads, or an error saying it reads several.
    ///
    /// A watch here is a version counter being polled, and the counter belongs
    /// to a secret: a set of secrets has no version of its own, and polling
    /// one member's would fire on that member and miss every other. Reading
    /// all of them every tick is a poll wearing a watch's name, which is the
    /// thing `refresh_remote()` on a timer already is, honestly.
    fn single_path(&self) -> Result<&str, Error> {
        match &self.keys {
            Keys::One(path) => Ok(path),
            Keys::Several(_) => Err(Error::remote(format!(
                "{}: a source that reads several paths cannot be watched; \
                 poll `refresh_remote()` on a timer instead",
                self.describe()
            ))),
        }
    }

    /// The one path a *dynamic* source reads.
    ///
    /// Dynamic mode is deliberately single-path. Every dynamic read mints a
    /// credential with a lease of its own, and a document carries one lease
    /// — so merging several would leave every lease but one unrenewed and
    /// unrevoked, which is a credential leak wearing a merge's clothes.
    fn single_dynamic_path(&self) -> Result<&str, Error> {
        match &self.keys {
            Keys::One(path) => Ok(path),
            Keys::Several(_) => Err(Error::remote(format!(
                "{}: a dynamic-secret source reads one path. Each read mints \
                 a credential with its own lease, and merging them would \
                 leave every lease but one unrenewed — use one source per \
                 credential",
                self.describe()
            ))),
        }
    }

    /// What two of this source's paths supplying one field means.
    ///
    /// Only [`Overlap::LaterWins`] here: a caller who wrote the list wrote the
    /// precedence with it, and there is no prefix form whose order nobody
    /// chose.
    fn overlap(&self) -> Overlap {
        Overlap::LaterWins
    }

    /// The version counter KV v2 keeps beside the secret.
    fn current_version(&self) -> Result<u64, CheckError> {
        let path = self.single_path().map_err(CheckError::Transient)?;

        let body = self
            .get(&self.metadata_url(path), "metadata")
            .map_err(CheckError::Transient)?;

        body.get("data")
            .and_then(|data| data.get("current_version"))
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                CheckError::NotKv2(Error::remote(format!(
                    "{}: the metadata has no `data.current_version`; is this a KV v2 mount?",
                    self.describe()
                )))
            })
    }

    /// An authenticated GET, retried once if the token turned out to be dead.
    ///
    /// `what` names the thing being read, for the error message.
    fn get(&self, url: &str, what: &str) -> Result<serde_json::Value, Error> {
        match self.get_once(url, what) {
            Err(CallError::Forbidden(_)) if self.can_relogin() => {
                // The proactive renewal should have caught an expiring token,
                // but clocks skew and leases get revoked. One fresh login and
                // one retry — not a loop: if a new token is also refused, the
                // policy is wrong and retrying would turn a clear failure into
                // a hang.
                self.session.invalidate();

                self.get_once(url, what).map_err(CallError::into_error)
            }
            outcome => outcome.map_err(CallError::into_error),
        }
    }

    /// Whether a refused token can be traded for a fresh one.
    ///
    /// Only a login can: `Auth::Token` was handed in from outside, and
    /// invalidating it would just retry the identical string — one wasted
    /// request per read against a broken policy.
    fn can_relogin(&self) -> bool {
        !matches!(self.auth, Auth::Token(_))
    }

    fn get_once(&self, url: &str, what: &str) -> Result<serde_json::Value, CallError> {
        // The ceiling, taken for the length of the request. A refusal is
        // not a queue: the caller backs off through the `Pace` it already
        // has, because a watch delivery held behind a resync held behind an
        // auth refresh helps nobody and none of them is improved by being
        // later.
        let _permit = self.concurrency.admit().ok_or_else(|| {
            CallError::Other(Error::remote(format!(
                "{}: {} requests already in flight",
                self.describe(),
                self.concurrency.limit()
            )))
        })?;

        let token = self.token().map_err(CallError::Other)?;

        // A TLS configuration that will not build is not a refused token, so
        // it must not become one: `Other` is what keeps a bad CA path out of
        // the relogin-and-retry path it would otherwise loop through.
        let agent = self.agent().map_err(CallError::Other)?;

        let mut request = agent.get(url).header("X-Vault-Token", &token);

        if let Some(namespace) = &self.namespace {
            request = request.header("X-Vault-Namespace", namespace);
        }

        request
            .call()
            .map_err(|error| {
                // The message is the same either way; only the kind differs,
                // and only the typed status decides it. `{error}` here is
                // `ureq`'s own rendering of the status — never the request,
                // so the `X-Vault-Token` header cannot ride along.
                let described = format!("{}: {error}", self.describe());

                match error {
                    ureq::Error::StatusCode(403) => CallError::Forbidden(Error::auth(described)),
                    // Vault answers 404 for a path that holds nothing —
                    // deleted, or never written. The store answered, so
                    // this is not an outage, and calling it one is how a
                    // deleted secret goes on being served in silence.
                    ureq::Error::StatusCode(404) => CallError::Other(Error::absent(described)),
                    _ => CallError::Other(Error::remote(described)),
                }
            })?
            .body_mut()
            .read_json()
            .map_err(|error| {
                CallError::Other(Error::remote(format!(
                    "{}: the {what} response was not JSON: {error}",
                    self.describe()
                )))
            })
    }

    /// The token to present, logging in or renewing if it is time.
    fn token(&self) -> Result<String, Error> {
        // A token supplied by the caller is used as it is: there is nothing to
        // log in with, so the session would only wrap it.
        if let Auth::Token(supplied) = &self.auth {
            if supplied.is_empty() {
                // `Auth` rather than `Remote`: nothing was ever sent, so the
                // store is not the problem and no amount of retrying will
                // produce a credential that was never supplied.
                return Err(Error::auth(format!(
                    "{}: no credentials; call `with_token` or `with_auth`",
                    self.describe()
                )));
            }

            return Ok(supplied.clone());
        }

        self.session
            .token(|| self.login(), |token| self.renew(token))
    }

    /// Exchanges credentials for a token.
    fn login(&self) -> Result<Issued<Token>, Error> {
        let Some(path) = self.auth.path() else {
            // Unreachable: `token()` handles `Auth::Token` before it gets here.
            return Err(Error::remote(format!(
                "{}: {} needs no login",
                self.describe(),
                self.auth.describe()
            )));
        };

        let body = self.auth.body()?;
        let url = format!("{}/v1/{path}", self.address);

        let mut request = self.agent()?.post(&url);

        if let Some(namespace) = &self.namespace {
            request = request.header("X-Vault-Namespace", namespace);
        }

        let response: serde_json::Value = request
            .send_json(&body)
            .map_err(|error| {
                let described = format!(
                    "{}: logging in with {} failed: {error}",
                    self.describe(),
                    self.auth.describe()
                );

                // On the *login* endpoint the request shape is ours and
                // correct, so a 400 or a 403 is Vault saying these
                // credentials are not accepted — a token that could not be
                // obtained, which waiting does not fix. A 503 (sealed) or a
                // network failure does fix itself, and stays `Remote`.
                match error {
                    ureq::Error::StatusCode(400 | 403) => Error::auth(described),
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

        self.token_from(&response, "auth")
    }

    /// Extends the current token's lease.
    fn renew(&self, token: &str) -> Result<Issued<Token>, Error> {
        let url = format!("{}/v1/auth/token/renew-self", self.address);
        let response = self.post(&url, token, serde_json::json!({}), "renewal")?;

        // Renewal answers with the lease but not the token: it is the same one.
        let mut renewed = self.token_from(&response, "auth")?;
        renewed.value.secret = token.to_owned();

        Ok(renewed)
    }

    /// One authenticated POST against this Vault, for a caller outside the
    /// read path.
    ///
    /// The transit decryptor is that caller: it needs this Vault's address,
    /// credential, TLS configuration and namespace, and needs none of the
    /// KV or dynamic read logic above them. Authenticating a second time
    /// from a second object would mean a second token, a second renewal and
    /// a second login on every restart.
    ///
    /// # Errors
    ///
    /// If the credential cannot be obtained, or the call fails. `what`
    /// names the call; the body never reaches the message.
    pub fn post_authenticated(
        &self,
        url: &str,
        body: serde_json::Value,
        what: &str,
    ) -> Result<serde_json::Value, Error> {
        let token = self.token()?;

        self.post(url, &token, body, what)
    }

    /// The address this Vault was built with, without its trailing slash.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// One authenticated POST, in the shape `get_once` already established.
    ///
    /// `login` and the token renewal each built their own request, header
    /// and error mapping; the lease calls would have been the third and
    /// fourth copy. `what` names the call in a failure — never the body,
    /// which for a dynamic secret holds the generated username and password.
    fn post(
        &self,
        url: &str,
        token: &str,
        body: serde_json::Value,
        what: &str,
    ) -> Result<serde_json::Value, Error> {
        let _permit = self.concurrency.admit().ok_or_else(|| {
            Error::remote(format!(
                "{}: {} requests already in flight",
                self.describe(),
                self.concurrency.limit()
            ))
        })?;

        let mut request = self.agent()?.post(url).header("X-Vault-Token", token);

        if let Some(namespace) = &self.namespace {
            request = request.header("X-Vault-Namespace", namespace);
        }

        let text = request
            .send_json(body)
            .map_err(|error| Error::remote(format!("{}: {what} failed: {error}", self.describe())))?
            .body_mut()
            .read_to_string()
            .map_err(|error| {
                Error::remote(format!(
                    "{}: the {what} response could not be read: {error}",
                    self.describe()
                ))
            })?;

        // A revocation answers 204 with no body at all, and "it worked" is
        // not a parse failure. Read as text first so an empty answer is the
        // success it is.
        if text.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }

        serde_json::from_str(&text).map_err(|error| {
            Error::remote(format!(
                "{}: the {what} response was not JSON: {error}",
                self.describe()
            ))
        })
    }

    /// Reads a token, its lease and whether it renews out of an `auth` block.
    fn token_from(
        &self,
        response: &serde_json::Value,
        field: &str,
    ) -> Result<Issued<Token>, Error> {
        let auth = response.get(field).ok_or_else(|| {
            Error::remote(format!(
                "{}: the response has no `{field}` block",
                self.describe()
            ))
        })?;

        let secret = auth
            .get("client_token")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();

        // Zero means "does not expire" in Vault's own vocabulary, not "expired".
        let lease = auth
            .get("lease_duration")
            .and_then(serde_json::Value::as_u64)
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs);

        let renewable = auth
            .get("renewable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        Ok(Issued {
            value: Token::new(secret, renewable),
            ttl: lease,
        })
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

    fn url(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');

        match self.mode {
            // KV v2 nests reads under `data/`; every other engine is read at
            // the path itself.
            Mode::Kv2 => format!("{}/v1/{}/data/{path}", self.address, self.mount),
            Mode::Dynamic => format!("{}/v1/{}/{path}", self.address, self.mount),
        }
    }

    fn metadata_url(&self, path: &str) -> String {
        format!(
            "{}/v1/{}/metadata/{}",
            self.address,
            self.mount,
            path.trim_start_matches('/')
        )
    }
}

impl Vault {
    /// The one secret a watch follows, and the version it was read at.
    ///
    /// The version comes from the same response as the values, so the two
    /// cannot disagree — which is the whole point of not asking twice.
    fn read(&self) -> Result<(Fetched, u64), Error> {
        let path = self.single_path()?;
        let read = self.read_one(path)?;

        let mut document = Fetched::new(read.document, Format::Json);

        // KV v2 versions count up, so that one is orderable — which is what
        // lets a late read be recognised as old rather than installed over a
        // newer one. A dynamic engine has no counter to offer.
        if self.mode == Mode::Kv2 {
            document = document.with_revision(Revision::Counter(read.version));
        }

        if let Some(lease) = read.lease.clone() {
            document = document.with_lease(lease);
        }

        Ok((document, read.version))
    }

    /// One secret, wrapped under the section key, and its version.
    ///
    /// The wrapping happens per path rather than after the merge because that
    /// is what makes the merge mean something: two secrets supplying `host`
    /// collide at `db.host`, which is the path a reader of the configuration
    /// would name, not at a bare `host` that belongs to nothing.
    fn read_one(&self, path: &str) -> Result<Reading, Error> {
        let body = self.get(&self.url(path), "secret")?;

        match self.mode {
            Mode::Kv2 => self.read_kv2(path, &body),
            Mode::Dynamic => self.read_dynamic(path, &body),
        }
    }

    /// A KV v2 read: values one level down, a version beside them.
    fn read_kv2(&self, path: &str, body: &serde_json::Value) -> Result<Reading, Error> {
        // KV v2 nests the values one level down; anything else is a v1 mount or
        // an error page, and either way not what was asked for.
        let values = body
            .get("data")
            .and_then(|data| data.get("data"))
            .ok_or_else(|| {
                Error::remote(format!(
                    "{}: `{path}` answered without `data.data`; is this a KV v2 \
                     mount? A dynamic-secret engine is read with \
                     `with_dynamic()`",
                    self.describe()
                ))
            })?;

        // A KV v2 soft delete answers 200 with `data.data` set to null: the
        // version is still there, its values are not. That is a document
        // that is gone, not a mount that is the wrong kind.
        if values.is_null() {
            return Err(Error::absent(format!(
                "{}: `{path}` holds no values; the version is deleted",
                self.describe()
            )));
        }

        // Absent on a v1 mount, which `data.data` has already ruled out; zero
        // is then a version that never matches a real one, so a watch keeps
        // reading rather than deciding nothing ever changes.
        let version = body
            .get("data")
            .and_then(|data| data.get("metadata"))
            .and_then(|metadata| metadata.get("version"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);

        Ok(Reading {
            document: serde_json::json!({ &self.key: values }).to_string(),
            version,
            lease: None,
        })
    }

    /// A dynamic-secret read: values at `data`, and the lease they were
    /// issued under.
    fn read_dynamic(&self, path: &str, body: &serde_json::Value) -> Result<Reading, Error> {
        let values = body.get("data").ok_or_else(|| {
            Error::remote(format!(
                "{}: `{path}` answered without `data`; is this a secrets \
                 engine? A KV v2 mount is read without `with_dynamic()`",
                self.describe()
            ))
        })?;

        if values.is_null() {
            return Err(Error::absent(format!(
                "{}: `{path}` issued no credential",
                self.describe()
            )));
        }

        Ok(Reading {
            document: serde_json::json!({ &self.key: values }).to_string(),
            // No version counter on a dynamic engine: every read mints a new
            // credential, so there is nothing to compare and nothing to poll.
            version: 0,
            lease: lease_of(body),
        })
    }

    /// The `(path, document)` pairs this source reads, in merge order.
    ///
    /// A named list is one request per path and **every one of them must
    /// answer**: merging the two that did would leave a process running a
    /// section with half of itself quietly missing.
    fn documents(&self) -> Result<Vec<(String, String)>, Error> {
        let paths = self.keys.named();
        let mut documents = Vec::with_capacity(paths.len());

        for path in paths {
            documents.push((path.clone(), self.read_one(path)?.document));
        }

        Ok(documents)
    }
}

// Hand-written, never derived: a derive would print every field, and the
// fields include credentials. `{:?}` reaching a log is an ordinary accident —
// a `dbg!`, a `tracing::debug!(?source)` — and an accident must not disclose
// a secret. The other store crates follow the same rule.
impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("address", &self.address)
            .field("mount", &self.mount)
            .field("keys", &self.keys)
            .field("key", &self.key)
            .field("namespace", &self.namespace)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl RenewableSource for Vault {
    /// Extends a lease through `sys/leases/renew`.
    ///
    /// The answer is authoritative: Vault may grant less than was asked for
    /// — a role's `max_ttl` is a ceiling the caller cannot see — so the next
    /// renewal is scheduled from what came back, never from what was
    /// requested. Asking for the lease's own TTL again is the honest
    /// request: it says *as long as you gave me last time*.
    fn renew(&self, lease: &Lease) -> Result<Lease, Error> {
        let url = format!("{}/v1/sys/leases/renew", self.address);
        let token = self.token()?;

        let response = self.post(
            &url,
            &token,
            serde_json::json!({
                "lease_id": lease.id,
                "increment": lease.ttl.as_secs(),
            }),
            "the lease renewal",
        )?;

        // A renewal answers with the lease at the top level, exactly as the
        // issuing read did — so the same parser reads it.
        lease_of(&response).ok_or_else(|| {
            Error::remote(format!(
                "{}: the renewal answered without a lease",
                self.describe()
            ))
        })
    }

    /// Hands a lease back through `sys/leases/revoke`.
    ///
    /// The credential stops working now rather than at expiry. Best-effort
    /// by nature — a caller doing this on the way out has somewhere else to
    /// be — and the lease expires on its own regardless; revoking only
    /// shortens the window in which a leaked credential is worth anything.
    fn revoke(&self, lease: &Lease) -> Result<(), Error> {
        let url = format!("{}/v1/sys/leases/revoke", self.address);
        let token = self.token()?;

        self.post(
            &url,
            &token,
            serde_json::json!({ "lease_id": lease.id }),
            "the lease revocation",
        )
        .map(|_| ())
    }
}

impl RemoteSource for Vault {
    fn fetch(&self) -> Result<Fetched, Error> {
        // Straight through `read`, which keeps the lease. The merge below
        // builds a fresh document and would drop it — and a credential
        // nobody can renew or revoke is worse than one that was never read.
        if self.mode == Mode::Dynamic {
            self.single_dynamic_path()?;

            return self.read().map(|(document, _)| document);
        }

        let documents = self.documents()?;

        // Read in call order, which is the order the rule wants — so nothing
        // is sorted here.
        documents::merged(&documents, Format::Json, self.overlap(), &self.describe())
    }

    fn describe(&self) -> String {
        // The address too: a program with a staging Vault and a production
        // Vault should never have to guess which one refused it.
        format!(
            "vault {} {}/{}",
            self.address,
            self.mount,
            self.keys.describe()
        )
    }

    /// Conditional: the KV metadata endpoint answers with a version counter,
    /// so asking costs a small JSON body rather than the secret itself.
    fn watch_capability(&self) -> WatchCapability {
        match self.mode {
            // A KV v2 version counter is cheap to poll and says whether the
            // document moved without sending it.
            Mode::Kv2 => WatchCapability::Conditional,
            // A dynamic engine has no counter and no metadata endpoint:
            // every read mints a credential, so "has it changed?" is not a
            // question that can be asked separately from "give me one".
            Mode::Dynamic => WatchCapability::Interval,
        }
    }

    fn watch(
        &self,
        watching: &Watching,
        interval: Duration,
        on_change: &mut dyn FnMut(Fetched) -> Result<(), Error>,
    ) -> Result<(), Error> {
        Vault::watch(self, watching, interval, on_change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_a_credential() {
        let source = Vault::new("http://vault:8200", "secret", "myapp/db")
            .with_auth(Auth::app_role("hunter2-role-id", "hunter2-secret-id"));

        let printed = format!(
            "{source:?} {:?} {:?}",
            Auth::token("hunter2-token"),
            Auth::userpass("admin", "hunter2-password"),
        );

        assert!(!printed.contains("hunter2-secret-id"), "{printed}");
        assert!(!printed.contains("hunter2-token"), "{printed}");
        assert!(!printed.contains("hunter2-password"), "{printed}");
        // The non-secret halves stay printable — that is what makes the
        // redaction usable rather than a black hole.
        assert!(printed.contains("hunter2-role-id"), "{printed}");
        assert!(printed.contains("admin"), "{printed}");
    }
}
