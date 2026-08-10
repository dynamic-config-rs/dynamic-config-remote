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
//! [`dynamic-config`]: https://docs.rs/dynamic-config
//! [`dynamic-config-vault`]: https://docs.rs/dynamic-config-vault

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod auth;
mod value;

use std::time::Duration;

use dynamic_config::{Error, Fetched, Format, RemoteSource, Watching};

pub use auth::Auth;

/// How long to wait for Firestore before giving up.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// A failed call, sorted by what a caller can do about it.
///
/// Sorted on `ureq`'s *typed* status, before anything becomes a string: an
/// error message mentioning a path like `config/401` must not read as an
/// expired token.
enum CallError {
    /// Firestore said 401: the token is the problem, and a fresh one might be
    /// the cure.
    Unauthorized(Error),
    /// Everything else — network, timeouts, 500s. A new token fixes none of
    /// it.
    Other(Error),
}

impl CallError {
    fn into_error(self) -> Error {
        match self {
            Self::Unauthorized(error) | Self::Other(error) => error,
        }
    }
}

/// A document in Firestore, as a configuration source.
///
/// Not `Clone`: it holds the session that caches an access token, and two
/// clones fetching tokens separately would double the traffic.
#[derive(Debug)]
pub struct Firestore {
    project: String,
    database: String,
    path: String,
    key: String,
    auth: Auth,
    session: auth::Session,
    endpoint: Option<String>,
    timeout: Duration,
    agent: Option<ureq::Agent>,
    /// The fallback client, built once. A fresh agent per request would mean
    /// a fresh connection pool per request — a TLS handshake per poll tick.
    default_agent: std::sync::OnceLock<ureq::Agent>,
}

impl Firestore {
    /// The document at `path` in `project`'s default database.
    ///
    /// `path` is collection-then-document — `config/db`, or
    /// `environments/prod/config/db` for a nested one.
    ///
    /// The document is wrapped under the section key the configuration type
    /// uses, `"db"` by default; change it with [`with_key`](Self::with_key).
    #[must_use]
    pub fn new(project: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            database: "(default)".to_owned(),
            path: path.into().trim_matches('/').to_owned(),
            key: "db".to_owned(),
            auth: Auth::Emulator,
            session: auth::Session::new(),
            endpoint: None,
            timeout: DEFAULT_TIMEOUT,
            agent: None,
            default_agent: std::sync::OnceLock::new(),
        }
    }

    /// The section key to wrap the document under.
    ///
    /// Must match the `key` in the `#[dynamic_config]` attribute.
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

    /// How long to wait before giving up. Ten seconds by default.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        // The cached fallback client baked in the old timeout.
        self.default_agent = std::sync::OnceLock::new();
        self
    }

    /// Uses an HTTP client the program already has.
    #[must_use]
    pub fn with_agent(mut self, agent: ureq::Agent) -> Self {
        self.agent = Some(agent);
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
    /// quarter second whatever `interval` is.
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
        let mut seen: Option<String> = None;

        while watching.keep_going() {
            // A failed read — a blip, an expired token, a document briefly
            // unreachable — is skipped rather than reported: that is what a
            // watch exists to survive.
            if let Ok((document, updated)) = self.read() {
                // No `updateTime` means no way to ever detect a change: every
                // tick would compare nothing to nothing and find "no change",
                // and the watch would sit silent forever. A server answering
                // like that is misconfigured, and that is reported, not
                // waited out.
                let Some(updated) = updated else {
                    return Err(Error::remote(format!(
                        "{}: the document has no `updateTime`, so changes                          cannot be detected; is this a real Firestore?",
                        self.describe()
                    )));
                };

                // The first read records the time without firing: the document
                // it names is the one the caller already has.
                if seen.is_none() {
                    seen = Some(updated);
                } else if seen.as_deref() != Some(&*updated) {
                    seen = Some(updated);

                    on_change(document)?;
                }
            }

            watching.sleep_for(interval);
        }

        Ok(())
    }

    /// The document, and the `updateTime` it was read at.
    fn read(&self) -> Result<(Fetched, Option<String>), Error> {
        let body = self.get()?;

        let fields = body.get("fields").ok_or_else(|| {
            Error::remote(format!(
                "{}: the response has no `fields`; is that a document?",
                self.describe()
            ))
        })?;

        let values = value::to_json(fields);
        let document = serde_json::json!({ &self.key: values });

        let updated = body
            .get("updateTime")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);

        Ok((Fetched::new(document.to_string(), Format::Json), updated))
    }

    /// One GET, retried once if the token turned out to be dead.
    fn get(&self) -> Result<serde_json::Value, Error> {
        match self.get_once() {
            Err(CallError::Unauthorized(_)) if self.can_refresh() => {
                // The proactive refresh should have caught an expiring token,
                // but clocks skew. One fresh token and one retry — not a loop.
                self.session.invalidate();

                self.get_once().map_err(CallError::into_error)
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

    fn get_once(&self) -> Result<serde_json::Value, CallError> {
        let mut request = self.agent().get(&self.url());

        if let Some(token) = self
            .session
            .token(&self.auth, self.agent())
            .map_err(CallError::Other)?
        {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }

        request
            .call()
            .map_err(|error| {
                let rendered = Error::remote(format!("{}: {error}", self.describe()));

                match error {
                    ureq::Error::StatusCode(401) => CallError::Unauthorized(rendered),
                    _ => CallError::Other(rendered),
                }
            })?
            .body_mut()
            .read_json()
            .map_err(|error| {
                CallError::Other(Error::remote(format!(
                    "{}: the response was not JSON: {error}",
                    self.describe()
                )))
            })
    }

    fn url(&self) -> String {
        let host = self
            .endpoint
            .clone()
            .unwrap_or_else(|| "https://firestore.googleapis.com".to_owned());

        format!(
            "{host}/v1/projects/{}/databases/{}/documents/{}",
            self.project, self.database, self.path
        )
    }

    /// The HTTP client: the caller's if they supplied one, otherwise ours.
    ///
    /// Ours is built once and kept: an agent owns a connection pool and a TLS
    /// session cache, and rebuilding it per request would pay a handshake per
    /// poll tick.
    fn agent(&self) -> &ureq::Agent {
        self.agent.as_ref().unwrap_or_else(|| {
            self.default_agent.get_or_init(|| {
                ureq::Agent::config_builder()
                    .timeout_global(Some(self.timeout))
                    .build()
                    .new_agent()
            })
        })
    }
}

impl RemoteSource for Firestore {
    fn fetch(&self) -> Result<Fetched, Error> {
        self.read().map(|(document, _updated)| document)
    }

    fn describe(&self) -> String {
        // The endpoint tells the emulator apart from the real service — the
        // question an error actually raises. The auth method is not part of
        // *where*, so it no longer rides along.
        match &self.endpoint {
            Some(endpoint) => format!("firestore {endpoint} {}/{}", self.project, self.path),
            None => format!("firestore {}/{}", self.project, self.path),
        }
    }
}
