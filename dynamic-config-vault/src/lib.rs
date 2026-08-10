//! Read [`dynamic-config`] configuration from HashiCorp Vault.
//!
//! Vault's KV v2 store speaks plain HTTP, so this implements the **blocking**
//! [`RemoteSource`] trait: nothing here needs an async runtime, and neither
//! does using it.
//!
//! ```no_run
//! use dynamic_config_vault::Vault;
//!
//! # struct DbConfig;
//! # impl DbConfig {
//! #     fn set_remote(_: Vault) {}
//! #     fn refresh_remote() -> Result<(), dynamic_config::Error> { Ok(()) }
//! #     fn init() -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! DbConfig::set_remote(
//!     Vault::new("https://vault.internal:8200", "secret", "myapp/db")
//!         .with_token(std::env::var("VAULT_TOKEN")?),
//! );
//!
//! // Fetching is explicit; the load that follows touches no network.
//! DbConfig::refresh_remote()?;
//! DbConfig::init()?;
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
//! ```no_run
//! # use dynamic_config::RemoteWatch;
//! # use dynamic_config_vault::Vault;
//! # use std::time::Duration;
//! # struct DbConfig;
//! # impl DbConfig {
//! #     fn apply_remote(_: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! # fn example(vault: Vault) {
//! let watch = RemoteWatch::new();
//! let watching = watch.watching();
//!
//! std::thread::spawn(move || {
//!     vault.watch(&watching, Duration::from_secs(30), DbConfig::apply_remote)
//! });
//!
//! // Dropping `watch` — or calling `watch.stop()` — ends the loop.
//! # }
//! ```
//!
//! [`dynamic-config`]: https://docs.rs/dynamic-config

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::time::Duration;

use dynamic_config::{Error, Fetched, Format, RemoteSource, Watching};

pub mod auth;

pub use auth::Auth;
use auth::{Session, Token};

/// How long to wait for Vault before giving up.
///
/// A configuration fetch that hangs is worse than one that fails: the caller
/// can retry a failure.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// A failed call, sorted by what a caller can do about it.
///
/// Sorted on `ureq`'s *typed* status, before anything becomes a string: an
/// error message mentioning a path like `myapp/403` must not read as a
/// refused token.
enum CallError {
    /// Vault said 403: the token is the problem, and a fresh login might be
    /// the cure.
    Forbidden(Error),
    /// Everything else — network, timeouts, a sealed Vault. A new token fixes
    /// none of it.
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
    /// The next tick may well succeed.
    #[allow(dead_code, reason = "carried for symmetry; only `NotKv2` is read")]
    Transient(Error),
}

/// A secret in Vault's KV v2 store, as a configuration source.
///
/// Not `Clone`: the session holds the current token, and two clones sharing a
/// path while logging in separately would double the login traffic and halve
/// the usefulness of the cache. Wrap it in an `Arc` if two places need one.
#[derive(Debug)]
pub struct Vault {
    address: String,
    mount: String,
    path: String,
    key: String,
    auth: Auth,
    session: Session,
    namespace: Option<String>,
    timeout: Duration,
    agent: Option<ureq::Agent>,
    /// The fallback client, built once. A fresh agent per request would mean
    /// a fresh connection pool per request — a TLS handshake per poll tick.
    default_agent: std::sync::OnceLock<ureq::Agent>,
}

impl Vault {
    /// A secret at `{mount}/{path}`, served by the Vault at `address`.
    ///
    /// The document is wrapped under the section key the configuration type
    /// uses — `"db"` by default, changed with [`with_key`](Self::with_key) —
    /// because Vault stores a section's contents, not a whole file.
    pub fn new(
        address: impl Into<String>,
        mount: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            address: address.into().trim_end_matches('/').to_owned(),
            mount: mount.into(),
            path: path.into(),
            key: "db".to_owned(),
            // No credentials until one is supplied; the first read then says so
            // rather than sending an unauthenticated request and reporting
            // Vault's answer to it.
            auth: Auth::Token(String::new()),
            session: Session::new(),
            namespace: None,
            timeout: DEFAULT_TIMEOUT,
            agent: None,
            default_agent: std::sync::OnceLock::new(),
        }
    }

    /// The section key to wrap the secret under.
    ///
    /// Must match the `key` in the `#[dynamic_config]` attribute.
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
    #[must_use]
    pub fn with_agent(mut self, agent: ureq::Agent) -> Self {
        self.agent = Some(agent);
        self
    }

    /// The Vault Enterprise namespace, if there is one.
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
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
    /// # struct DbConfig;
    /// # impl DbConfig {
    /// #     fn apply_remote(_: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
    /// # }
    /// # fn example(vault: Vault, watching: dynamic_config::Watching) -> Result<(), dynamic_config::Error> {
    /// DbConfig::apply_remote(vault.fetch()?)?;
    /// vault.watch(&watching, Duration::from_secs(30), DbConfig::apply_remote)
    /// # }
    /// ```
    ///
    /// A failed check does not end the watch — an expired token, a sealed
    /// Vault, a network blip — it waits out the interval and tries again. `stop`
    /// is noticed within a quarter second regardless of how long `interval` is.
    ///
    /// # Errors
    ///
    /// If the mount turns out not to be KV v2: a v1 mount has no version
    /// counter, so every tick would find "no change" and the watch would
    /// silently never fire — a misconfiguration, reported as one. Or if
    /// `on_change` returns an error, which ends the watch — so a caller that
    /// wants to survive a bad document should log it and return `Ok`.
    /// Transport failures do not surface here; they are retried.
    pub fn watch<F>(
        &self,
        watching: &Watching,
        interval: Duration,
        mut on_change: F,
    ) -> Result<(), Error>
    where
        F: FnMut(Fetched) -> Result<(), Error>,
    {
        let mut seen: Option<u64> = None;

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
                    if let Ok((document, version)) = self.read() {
                        seen = Some(version);

                        on_change(document)?;
                    }
                }

                // Not a KV v2 mount: there is no version counter to poll, so
                // "retry next tick" would run forever and deliver nothing.
                // A misconfiguration is reported, not waited out.
                Err(CheckError::NotKv2(error)) => return Err(error),

                // Unchanged, or a transient failure. Either way, wait.
                _ => {}
            }

            watching.sleep_for(interval);
        }

        Ok(())
    }

    /// The version counter KV v2 keeps beside the secret.
    fn current_version(&self) -> Result<u64, CheckError> {
        let body = self
            .get(&self.metadata_url(), "metadata")
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
        let token = self.token().map_err(CallError::Other)?;

        let mut request = self.agent().get(url).header("X-Vault-Token", &token);

        if let Some(namespace) = &self.namespace {
            request = request.header("X-Vault-Namespace", namespace);
        }

        request
            .call()
            .map_err(|error| {
                let rendered = Error::remote(format!("{}: {error}", self.describe()));

                match error {
                    ureq::Error::StatusCode(403) => CallError::Forbidden(rendered),
                    _ => CallError::Other(rendered),
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
                return Err(Error::remote(format!(
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
    fn login(&self) -> Result<Token, Error> {
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

        let mut request = self.agent().post(&url);

        if let Some(namespace) = &self.namespace {
            request = request.header("X-Vault-Namespace", namespace);
        }

        let response: serde_json::Value = request
            .send_json(&body)
            .map_err(|error| {
                Error::remote(format!(
                    "{}: logging in with {} failed: {error}",
                    self.describe(),
                    self.auth.describe()
                ))
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
    fn renew(&self, token: &str) -> Result<Token, Error> {
        let url = format!("{}/v1/auth/token/renew-self", self.address);

        let mut request = self.agent().post(&url).header("X-Vault-Token", token);

        if let Some(namespace) = &self.namespace {
            request = request.header("X-Vault-Namespace", namespace);
        }

        let response: serde_json::Value = request
            .send_json(serde_json::json!({}))
            .map_err(|error| {
                Error::remote(format!("{}: renewal failed: {error}", self.describe()))
            })?
            .body_mut()
            .read_json()
            .map_err(|error| {
                Error::remote(format!(
                    "{}: the renewal response was not JSON: {error}",
                    self.describe()
                ))
            })?;

        // Renewal answers with the lease but not the token: it is the same one.
        let mut renewed = self.token_from(&response, "auth")?;
        renewed.secret = token.to_owned();

        Ok(renewed)
    }

    /// Reads a token, its lease and whether it renews out of an `auth` block.
    fn token_from(&self, response: &serde_json::Value, field: &str) -> Result<Token, Error> {
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

        Ok(Token::new(secret, lease, renewable))
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

    fn url(&self) -> String {
        format!(
            "{}/v1/{}/data/{}",
            self.address,
            self.mount,
            self.path.trim_start_matches('/')
        )
    }

    fn metadata_url(&self) -> String {
        format!(
            "{}/v1/{}/metadata/{}",
            self.address,
            self.mount,
            self.path.trim_start_matches('/')
        )
    }
}

impl Vault {
    /// The secret, and the version it was read at.
    ///
    /// The version comes from the same response as the values, so the two
    /// cannot disagree — which is the whole point of not asking twice.
    fn read(&self) -> Result<(Fetched, u64), Error> {
        let body = self.get(&self.url(), "secret")?;

        // KV v2 nests the values one level down; anything else is a v1 mount or
        // an error page, and either way not what was asked for.
        let values = body
            .get("data")
            .and_then(|data| data.get("data"))
            .ok_or_else(|| {
                Error::remote(format!(
                    "{}: the response has no `data.data`; is this a KV v2 mount?",
                    self.describe()
                ))
            })?;

        let document = serde_json::json!({ &self.key: values });

        // Absent on a v1 mount, which `data.data` has already ruled out; zero
        // is then a version that never matches a real one, so a watch keeps
        // reading rather than deciding nothing ever changes.
        let version = body
            .get("data")
            .and_then(|data| data.get("metadata"))
            .and_then(|metadata| metadata.get("version"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);

        Ok((Fetched::new(document.to_string(), Format::Json), version))
    }
}

impl RemoteSource for Vault {
    fn fetch(&self) -> Result<Fetched, Error> {
        self.read().map(|(document, _version)| document)
    }

    fn describe(&self) -> String {
        // The address too: a program with a staging Vault and a production
        // Vault should never have to guess which one refused it.
        format!("vault {} {}/{}", self.address, self.mount, self.path)
    }
}
