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
//! ```no_run
//! # use dynamic_config::RemoteWatch;
//! # use dynamic_config_consul::Consul;
//! # struct DbConfig;
//! # impl DbConfig {
//! #     fn apply_remote(_: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
//! # }
//! # fn example(consul: Consul) {
//! let watch = RemoteWatch::new();
//! let watching = watch.watching();
//!
//! std::thread::spawn(move || consul.watch(&watching, DbConfig::apply_remote));
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
use dynamic_config::{Error, Fetched, Format, RemoteSource, Watching};

pub mod auth;

pub use auth::{Auth, Bearer};
use auth::{Session, Token};

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

/// A key in Consul's KV store, as a configuration source.
///
/// Not `Clone`: the session holds the current token, and two clones logging in
/// separately would double the login traffic. Wrap it in an `Arc` if two places
/// need one.
pub struct Consul {
    address: String,
    key: String,
    format: Option<Format>,
    auth: Auth,
    session: Session,
    datacenter: Option<String>,
    timeout: Duration,
    wait: Duration,
    agent: Option<ureq::Agent>,
}

impl Consul {
    /// The key `key`, served by the Consul agent at `address`.
    ///
    /// The format is taken from the key's extension — `myapp/db.json` is JSON.
    /// A key without one needs [`with_format`](Self::with_format).
    pub fn new(address: impl Into<String>, key: impl Into<String>) -> Self {
        let key = key.into();

        let format = Format::from_key(&key);

        Self {
            address: address.into().trim_end_matches('/').to_owned(),
            key,
            format,
            auth: Auth::Anonymous,
            session: Session::new(),
            datacenter: None,
            timeout: DEFAULT_TIMEOUT,
            wait: DEFAULT_WAIT,
            agent: None,
        }
    }

    /// States the format, for a key whose name does not.
    #[must_use]
    pub fn with_format(mut self, format: Format) -> Self {
        self.format = Some(format);
        self
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
    #[must_use]
    pub fn with_agent(mut self, agent: ureq::Agent) -> Self {
        self.agent = Some(agent);
        self
    }

    /// The datacenter to read from, when it is not the agent's own.
    #[must_use]
    pub fn with_datacenter(mut self, datacenter: impl Into<String>) -> Self {
        self.datacenter = Some(datacenter.into());
        self
    }

    /// How long to wait before giving up. Ten seconds by default.
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

    /// Calls `on_change` whenever the key's value changes.
    ///
    /// Uses Consul's blocking queries: each request carries the index the last
    /// one returned, and the agent holds it open until that index moves or
    /// [`with_wait`](Self::with_wait) expires. So this is change-driven, not a
    /// poll — the callback runs when the value actually moves.
    ///
    /// The current value is **not** delivered at startup, for the same reason a
    /// file watcher does not report an edit when it starts. Fetch first if the
    /// starting value matters, which it usually does:
    ///
    /// ```no_run
    /// # use dynamic_config::{RemoteSource, RemoteWatch};
    /// # use dynamic_config_consul::Consul;
    /// # struct DbConfig;
    /// # impl DbConfig {
    /// #     fn apply_remote(_: dynamic_config::Fetched) -> Result<(), dynamic_config::Error> { Ok(()) }
    /// # }
    /// # fn example(consul: Consul, watching: dynamic_config::Watching) -> Result<(), dynamic_config::Error> {
    /// DbConfig::apply_remote(consul.fetch()?)?;
    /// consul.watch(&watching, DbConfig::apply_remote)
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
    /// # Errors
    ///
    /// If `on_change` returns an error, which ends the watch — so a caller that
    /// wants to survive a bad document should log it and return `Ok`. Transport
    /// failures do not surface here; they are retried.
    pub fn watch<F>(&self, watching: &Watching, mut on_change: F) -> Result<(), Error>
    where
        F: FnMut(Fetched) -> Result<(), Error>,
    {
        let format = self.format.ok_or_else(|| {
            Error::remote(format!(
                "{}: the key names no format; call `with_format`",
                self.describe()
            ))
        })?;

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
        );

        let mut index = 0;
        let mut last: Option<String> = None;
        // The first query carries index 0, which Consul answers immediately
        // with whatever is stored. That is the value the caller already has —
        // it primes the index and the comparison, and reports nothing, the same
        // way a file watcher does not announce an edit when it starts.
        let mut priming = true;

        while watching.keep_going() {
            let answered = match self.blocking_read(&agent, index) {
                Ok(answered) => answered,
                Err(_) => {
                    // Retried rather than reported: the loop's whole job is to
                    // survive the store going away for a while. Sleeping in
                    // slices keeps a stop from waiting out the whole pause.
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

            let Some(text) = answered.text else {
                // The key holds nothing. Consul will still block on the next
                // query, but only while its index is meaningful — a pause here
                // is what stops a degenerate index from becoming a hot loop.
                watching.sleep_for(RETRY_AFTER);

                continue;
            };

            let unchanged = last.as_ref() == Some(&text);

            last = Some(text.clone());

            if std::mem::take(&mut priming) || unchanged {
                continue;
            }

            guarded(&mut on_change, Fetched::new(text, format), &self.describe())?;
        }

        Ok(())
    }

    /// The HTTP client: the caller's if they supplied one, otherwise ours.
    fn agent(&self, timeout: Duration) -> ureq::Agent {
        self.agent.clone().unwrap_or_else(|| {
            ureq::Agent::config_builder()
                .timeout_global(Some(timeout))
                .build()
                .new_agent()
        })
    }

    /// The token to present, logging in if it is time.
    ///
    /// `Ok(None)` when there is nothing to present, which is the right answer
    /// for a Consul with ACLs disabled.
    fn token(&self) -> Result<Option<String>, Error> {
        match &self.auth {
            Auth::Anonymous => Ok(None),
            Auth::Token(supplied) => Ok(Some(supplied.clone())),
            Auth::Login { .. } => self.session.token(|| self.login()).map(Some),
        }
    }

    /// Exchanges a bearer token for an ACL token.
    fn login(&self) -> Result<Token, Error> {
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
            .agent(self.timeout)
            .post(&url)
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

        Ok(Token::new(secret, ttl))
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

    /// One blocking query. `index` of zero returns immediately.
    fn blocking_read(&self, agent: &ureq::Agent, index: u64) -> Result<Answered, Error> {
        let mut url = self.url();

        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&format!(
            "index={index}&wait={}s",
            self.wait.as_secs().max(1)
        ));

        let mut response = match self.call(agent.get(&url)) {
            Err(CallError::Forbidden(_)) if self.can_relogin() => {
                // The token stopped working. One fresh login and one retry, not
                // a loop: if a new token is also refused, the policy is wrong
                // and retrying would turn a clear failure into a hang.
                self.session.invalidate();

                self.call(agent.get(&url)).map_err(CallError::into_error)?
            }
            outcome => outcome.map_err(CallError::into_error)?,
        };

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

        Ok(Answered {
            index,
            text: self.decode(&entries).ok(),
        })
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
                let rendered = Error::remote(format!("{}: {error}", self.describe()));

                match error {
                    ureq::Error::StatusCode(403) => CallError::Forbidden(rendered),
                    _ => CallError::Other(rendered),
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

    /// Turns Consul's one-element array into the document it holds.
    fn decode(&self, entries: &[serde_json::Value]) -> Result<String, Error> {
        // Consul answers a single-key read with a one-element array. An empty
        // one means the key is not there, which is a missing configuration
        // rather than a transport failure — but still nothing to load.
        let encoded = entries
            .first()
            .and_then(|entry| entry.get("Value"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::remote(format!("{}: the key holds no value", self.describe())))?;

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| {
                Error::remote(format!(
                    "{}: the value is not valid base64: {error}",
                    self.describe()
                ))
            })?;

        String::from_utf8(decoded).map_err(|error| {
            Error::remote(format!(
                "{}: the value is not UTF-8: {error}",
                self.describe()
            ))
        })
    }

    fn url(&self) -> String {
        let mut url = format!(
            "{}/v1/kv/{}",
            self.address,
            self.key.trim_start_matches('/')
        );

        if let Some(datacenter) = &self.datacenter {
            url.push_str("?dc=");
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
    /// be the cure.
    Forbidden(Error),
    /// Everything else — network, timeouts, 500s. A new token fixes none of
    /// it.
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
    /// `None` when the key holds nothing — a deleted key is not a change worth
    /// reporting, because no configuration is not a configuration.
    text: Option<String>,
}

// Hand-written, never derived: a derive would print every field, and the
// fields include credentials. `{:?}` reaching a log is an ordinary accident —
// a `dbg!`, a `tracing::debug!(?source)` — and an accident must not disclose
// a secret. The other store crates follow the same rule.
impl std::fmt::Debug for Consul {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Consul")
            .field("address", &self.address)
            .field("key", &self.key)
            .field("format", &self.format)
            .field("datacenter", &self.datacenter)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl RemoteSource for Consul {
    fn fetch(&self) -> Result<Fetched, Error> {
        let format = self.format.ok_or_else(|| {
            Error::remote(format!(
                "{}: the key names no format; call `with_format`",
                self.describe()
            ))
        })?;

        let agent = self.agent(self.timeout);

        let mut response = match self.call(agent.get(&self.url())) {
            Err(CallError::Forbidden(_)) if self.can_relogin() => {
                self.session.invalidate();

                self.call(agent.get(&self.url()))
                    .map_err(CallError::into_error)?
            }
            outcome => outcome.map_err(CallError::into_error)?,
        };

        let entries: Vec<serde_json::Value> = response.body_mut().read_json().map_err(|error| {
            Error::remote(format!(
                "{}: the response was not JSON: {error}",
                self.describe()
            ))
        })?;

        Ok(Fetched::new(self.decode(&entries)?, format))
    }

    fn describe(&self) -> String {
        // The address too: "the key holds no value" helps nobody who has a
        // staging Consul and a production Consul and a wrong environment
        // variable.
        format!("consul {} kv/{}", self.address, self.key)
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
