//! Getting an ACL token, and getting another one when it stops working.
//!
//! Consul's story is shorter than Vault's. A token is either handed to the
//! process — the usual case, through `CONSUL_HTTP_TOKEN` — or obtained by
//! presenting a bearer token to an *auth method*, which is how a workload in
//! Kubernetes proves who it is without a secret to distribute.
//!
//! There is no renewal. Consul issues login tokens with an expiry and expects
//! you to log in again, so that is what happens: a token close to expiry is
//! replaced, and a `403` replaces one early. Both paths exist for the same
//! reason they do in the Vault crate — the proactive one should normally fire,
//! and the reactive one covers clock skew and tokens revoked out from under a
//! running process.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use dynamic_config::Error;

/// How close to expiry a token is replaced rather than used.
const REPLACE_WITHIN: Duration = Duration::from_secs(30);

/// Where a Kubernetes service-account token is mounted, by convention.
pub const SERVICE_ACCOUNT_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// How to obtain a Consul ACL token.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Auth {
    /// No token at all.
    ///
    /// Correct for a Consul with ACLs disabled, and for a `default` policy that
    /// allows reads — both of which are ordinary in development.
    Anonymous,

    /// A token somebody already obtained, usually `CONSUL_HTTP_TOKEN`.
    ///
    /// The only variant that cannot recover on its own: there are no
    /// credentials here to log in again with.
    Token(String),

    /// A bearer token presented to an auth method.
    ///
    /// Consul calls the endpoint `/v1/acl/login`; the method decides what a
    /// valid bearer token looks like — a Kubernetes service-account JWT, an
    /// OIDC id token, a JWT signed by something Consul trusts.
    Login {
        /// The auth method's name, as configured in Consul.
        method: String,
        /// Where the bearer token comes from.
        bearer: Bearer,
        /// Consul's `Meta`, attached to the issued token for auditing.
        meta: Vec<(String, String)>,
    },
}

/// Where a bearer token comes from.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Bearer {
    /// A literal token.
    Literal(String),
    /// A file, re-read at every login.
    ///
    /// This is what a Kubernetes projected service-account token needs: the
    /// kubelet rotates it, and a copy taken at startup expires with the pod
    /// still running.
    File(String),
}

impl Auth {
    /// A token somebody already obtained.
    pub fn token(token: impl Into<String>) -> Self {
        Self::Token(token.into())
    }

    /// `CONSUL_HTTP_TOKEN`, if it is set and not empty.
    ///
    /// Returns [`Auth::Anonymous`] when it is not, because a Consul with ACLs
    /// disabled is a perfectly ordinary thing to point this at, and failing
    /// would make the convenience useless in exactly that case.
    #[must_use]
    pub fn from_environment() -> Self {
        match std::env::var("CONSUL_HTTP_TOKEN") {
            Ok(token) if !token.is_empty() => Self::Token(token),
            _ => Self::Anonymous,
        }
    }

    /// Kubernetes: the pod's service-account token, presented to `method`.
    pub fn kubernetes(method: impl Into<String>) -> Self {
        Self::Login {
            method: method.into(),
            bearer: Bearer::File(SERVICE_ACCOUNT_TOKEN.to_owned()),
            meta: Vec::new(),
        }
    }

    /// A JWT or OIDC token presented to `method`.
    pub fn jwt(method: impl Into<String>, token: impl Into<String>) -> Self {
        Self::Login {
            method: method.into(),
            bearer: Bearer::Literal(token.into()),
            meta: Vec::new(),
        }
    }

    /// Reads the bearer token from somewhere other than the conventional path.
    #[must_use]
    pub fn with_bearer_file(mut self, path: impl Into<String>) -> Self {
        if let Self::Login { bearer, .. } = &mut self {
            *bearer = Bearer::File(path.into());
        }

        self
    }

    /// Adds a `Meta` entry, which Consul attaches to the issued token.
    #[must_use]
    pub fn with_meta(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        if let Self::Login { meta, .. } = &mut self {
            meta.push((name.into(), value.into()));
        }

        self
    }

    /// The body to POST to `/v1/acl/login`, or `None` when there is no login.
    ///
    /// # Errors
    ///
    /// If a bearer token file cannot be read.
    pub(crate) fn login_body(&self) -> Result<Option<serde_json::Value>, Error> {
        let Self::Login {
            method,
            bearer,
            meta,
        } = self
        else {
            return Ok(None);
        };

        let token = match bearer {
            Bearer::Literal(token) => token.clone(),
            Bearer::File(path) => std::fs::read_to_string(path)
                .map_err(|error| {
                    Error::remote(format!(
                        "consul: cannot read the bearer token at {path}: {error}"
                    ))
                })?
                .trim()
                .to_owned(),
        };

        let meta: serde_json::Map<String, serde_json::Value> = meta
            .iter()
            .map(|(name, value)| (name.clone(), serde_json::Value::from(value.clone())))
            .collect();

        Ok(Some(serde_json::json!({
            "AuthMethod": method,
            "BearerToken": token,
            "Meta": meta,
        })))
    }

    /// How to name this method in an error.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Anonymous => "no token".to_owned(),
            Self::Token(_) => "a supplied token".to_owned(),
            Self::Login { method, .. } => format!("auth method `{method}`"),
        }
    }
}

/// A token and when it expires.
#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub(crate) secret: String,
    /// `None` for a token Consul did not put an expiry on.
    expires: Option<Instant>,
}

impl Token {
    pub(crate) fn new(secret: String, ttl: Option<Duration>) -> Self {
        Self {
            secret,
            // `checked_add` because the TTL comes from the agent: one answering
            // with a nonsense `ExpirationTTL` would otherwise panic the process
            // on the arithmetic. Too large to represent is treated as no
            // expiry, which is what a number that large means anyway.
            expires: ttl.and_then(|ttl| Instant::now().checked_add(ttl)),
        }
    }

    fn is_stale(&self) -> bool {
        self.expires.is_some_and(|expires| {
            expires.saturating_duration_since(Instant::now()) < REPLACE_WITHIN
        })
    }
}

/// The current token for one source.
#[derive(Debug, Default)]
pub(crate) struct Session {
    token: Mutex<Option<Token>>,
}

impl Session {
    pub(crate) const fn new() -> Self {
        Self {
            token: Mutex::new(None),
        }
    }

    /// The token to present, logging in again if it is time.
    ///
    /// # Errors
    ///
    /// Whatever logging in reports.
    pub(crate) fn token(&self, login: impl Fn() -> Result<Token, Error>) -> Result<String, Error> {
        let mut slot = self.lock();

        if let Some(token) = slot.as_ref() {
            if !token.is_stale() {
                return Ok(token.secret.clone());
            }
        }

        let fresh = login()?;
        let secret = fresh.secret.clone();
        *slot = Some(fresh);

        Ok(secret)
    }

    /// Throws the current token away, so the next request logs in again.
    pub(crate) fn invalidate(&self) {
        *self.lock() = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Token>> {
        self.token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_supplied_token_needs_no_login() {
        assert!(Auth::token("t").login_body().unwrap().is_none());
        assert!(Auth::Anonymous.login_body().unwrap().is_none());
    }

    #[test]
    fn a_login_presents_its_bearer_token_to_a_named_method() {
        let body = Auth::jwt("kubernetes", "a.b.c")
            .login_body()
            .unwrap()
            .expect("this one logs in");

        assert_eq!(body["AuthMethod"], "kubernetes");
        assert_eq!(body["BearerToken"], "a.b.c");
    }

    #[test]
    fn meta_is_carried_through_for_the_audit_log() {
        let body = Auth::jwt("kubernetes", "a.b.c")
            .with_meta("pod", "myapp-7f9")
            .login_body()
            .unwrap()
            .unwrap();

        assert_eq!(body["Meta"]["pod"], "myapp-7f9");
    }

    #[test]
    fn a_missing_bearer_file_says_where_it_looked() {
        let error = Auth::kubernetes("kubernetes")
            .with_bearer_file("/no/such/token")
            .login_body()
            .expect_err("there is no token there");

        assert!(error.to_string().contains("/no/such/token"), "{error}");
    }

    #[test]
    fn an_unset_environment_variable_is_anonymous_rather_than_an_error() {
        // Consul with ACLs off is an ordinary thing to point this at, so the
        // convenience has to work there.
        std::env::remove_var("CONSUL_HTTP_TOKEN");

        assert!(matches!(Auth::from_environment(), Auth::Anonymous));
    }

    #[test]
    fn a_ttl_too_large_to_represent_is_treated_as_no_expiry() {
        // An agent answering with nonsense must not be able to panic the
        // process on `Instant + Duration`.
        assert!(!Token::new("t".to_owned(), Some(Duration::from_nanos(u64::MAX))).is_stale());
    }

    #[test]
    fn a_token_with_no_expiry_is_never_stale() {
        assert!(!Token::new("t".to_owned(), None).is_stale());
    }

    #[test]
    fn a_token_near_its_expiry_is_stale() {
        assert!(!Token::new("t".to_owned(), Some(Duration::from_secs(3600))).is_stale());
        assert!(Token::new("t".to_owned(), Some(REPLACE_WITHIN / 2)).is_stale());
    }

    #[test]
    fn a_session_logs_in_once_and_then_reuses_the_token() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let logins = AtomicUsize::new(0);
        let session = Session::new();

        let login = || {
            logins.fetch_add(1, Ordering::SeqCst);

            Ok(Token::new(
                "token".to_owned(),
                Some(Duration::from_secs(3600)),
            ))
        };

        assert_eq!(session.token(login).unwrap(), "token");
        assert_eq!(session.token(login).unwrap(), "token");
        assert_eq!(logins.load(Ordering::SeqCst), 1);

        session.invalidate();

        assert_eq!(session.token(login).unwrap(), "token");
        assert_eq!(
            logins.load(Ordering::SeqCst),
            2,
            "a 403 must be able to force a fresh login"
        );
    }
}
