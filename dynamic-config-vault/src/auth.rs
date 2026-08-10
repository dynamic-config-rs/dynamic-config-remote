//! Getting a token, and getting another one when it stops working.
//!
//! Every Vault auth method ends in the same place: a client token with a lease.
//! What differs is the credentials handed over and the endpoint they go to, so
//! that is all [`Auth`] models. The rest — when to log in, when to renew, what
//! to do about a token that expired mid-request — is the same for all of them
//! and is handled once, inside the crate.
//!
//! # Logging in is lazy
//!
//! Building a [`Vault`](crate::Vault) reaches nothing. The first read logs in,
//! and every read after that reuses the token until it is close to expiry. This
//! matches the rest of the crate: constructing a source is not I/O, and
//! configuration that reaches the network on a call nobody expected to block is
//! how a startup ends up mysteriously slow.
//!
//! # Expiry is handled twice, on purpose
//!
//! **Before the request**, a token within thirty seconds of its expiry is
//! renewed — or, if it cannot be, replaced by a fresh login. This is the path
//! that should normally fire.
//!
//! **After the request**, a `403` is treated as *the token stopped working* and
//! triggers exactly one fresh login and retry. Clocks skew, Vault revokes, a
//! lease is shorter than it said; the proactive path cannot catch all of that,
//! and a configuration reader that gives up on the first `403` will eventually
//! do so at three in the morning.
//!
//! Once, not in a loop: if a fresh token also gets `403`, the problem is the
//! policy rather than the lease, and retrying would only turn a clear failure
//! into a hang.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use dynamic_config::Error;

/// How close to expiry a token is refreshed rather than used.
///
/// Long enough to cover a slow round trip and a little clock skew, short enough
/// that a five-minute lease still gets used for most of its life.
const RENEW_WITHIN: Duration = Duration::from_secs(30);

/// Where a Kubernetes service-account token is mounted, by convention.
pub const SERVICE_ACCOUNT_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// How to obtain a Vault token.
///
/// The variants that take a `mount` take the auth method's mount path, which is
/// its type by default — `approle` for AppRole, `kubernetes` for Kubernetes.
/// Mounting the same method twice under different paths is ordinary Vault
/// practice, which is why it is a parameter rather than a constant.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Auth {
    /// A token somebody already obtained.
    ///
    /// The simplest thing that works, and the only one that cannot recover on
    /// its own: there are no credentials here to log in again with. A renewable
    /// token is still renewed.
    Token(String),

    /// AppRole: a role id and a secret id.
    ///
    /// The usual choice for a service outside Kubernetes.
    AppRole {
        /// Mount path, `"approle"` by default.
        mount: String,
        /// The role's public half.
        role_id: String,
        /// The role's secret half.
        secret_id: String,
    },

    /// Kubernetes: the pod's service-account token, plus a Vault role.
    ///
    /// The JWT is read from disk at every login rather than once, because the
    /// kubelet rotates projected service-account tokens and a copy taken at
    /// startup expires with the pod still running.
    Kubernetes {
        /// Mount path, `"kubernetes"` by default.
        mount: String,
        /// The Vault role to assume.
        role: String,
        /// Where the service-account token is mounted.
        token_path: String,
    },

    /// A JWT or OIDC token, with an optional role.
    Jwt {
        /// Mount path, `"jwt"` by default.
        mount: String,
        /// The Vault role, when the mount does not have a default.
        role: Option<String>,
        /// The token to present.
        jwt: String,
    },

    /// Username and password against the `userpass` method.
    Userpass {
        /// Mount path, `"userpass"` by default.
        mount: String,
        /// The user to log in as.
        username: String,
        /// Their password.
        password: String,
    },

    /// Username and password against an LDAP directory.
    Ldap {
        /// Mount path, `"ldap"` by default.
        mount: String,
        /// The user to log in as.
        username: String,
        /// Their password.
        password: String,
    },

    /// A TLS client certificate, with an optional role.
    ///
    /// The certificate itself is configured on the HTTP client, not here: build
    /// a `ureq::Agent` that presents it and hand it over with
    /// [`Vault::with_agent`](crate::Vault::with_agent). This variant only says
    /// *log in with it*, which is the part Vault needs told.
    Certificate {
        /// Mount path, `"cert"` by default.
        mount: String,
        /// The certificate role, when the mount does not pick one by subject.
        name: Option<String>,
    },
}

impl Auth {
    /// A token somebody already obtained.
    pub fn token(token: impl Into<String>) -> Self {
        Self::Token(token.into())
    }

    /// AppRole, on the default `approle` mount.
    pub fn app_role(role_id: impl Into<String>, secret_id: impl Into<String>) -> Self {
        Self::AppRole {
            mount: "approle".to_owned(),
            role_id: role_id.into(),
            secret_id: secret_id.into(),
        }
    }

    /// Kubernetes, on the default `kubernetes` mount, reading the pod's own
    /// service-account token.
    pub fn kubernetes(role: impl Into<String>) -> Self {
        Self::Kubernetes {
            mount: "kubernetes".to_owned(),
            role: role.into(),
            token_path: SERVICE_ACCOUNT_TOKEN.to_owned(),
        }
    }

    /// A JWT or OIDC token, on the default `jwt` mount.
    pub fn jwt(jwt: impl Into<String>) -> Self {
        Self::Jwt {
            mount: "jwt".to_owned(),
            role: None,
            jwt: jwt.into(),
        }
    }

    /// Username and password, on the default `userpass` mount.
    pub fn userpass(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self::Userpass {
            mount: "userpass".to_owned(),
            username: username.into(),
            password: password.into(),
        }
    }

    /// Username and password against LDAP, on the default `ldap` mount.
    pub fn ldap(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self::Ldap {
            mount: "ldap".to_owned(),
            username: username.into(),
            password: password.into(),
        }
    }

    /// A TLS client certificate, on the default `cert` mount.
    pub fn certificate() -> Self {
        Self::Certificate {
            mount: "cert".to_owned(),
            name: None,
        }
    }

    /// Puts this method on a different mount path.
    ///
    /// No effect on [`Auth::Token`], which has no mount.
    #[must_use]
    pub fn at_mount(mut self, path: impl Into<String>) -> Self {
        let path = path.into();

        match &mut self {
            Self::Token(_) => {}
            Self::AppRole { mount, .. }
            | Self::Kubernetes { mount, .. }
            | Self::Jwt { mount, .. }
            | Self::Userpass { mount, .. }
            | Self::Ldap { mount, .. }
            | Self::Certificate { mount, .. } => *mount = path,
        }

        self
    }

    /// Names the role, for the methods that take one.
    #[must_use]
    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        let named = role.into();

        match &mut self {
            Self::Kubernetes { role, .. } => *role = named,
            Self::Jwt { role, .. } => *role = Some(named),
            Self::Certificate { name, .. } => *name = Some(named),
            _ => {}
        }

        self
    }

    /// Reads the service-account token from somewhere other than the
    /// conventional path.
    #[must_use]
    pub fn with_token_path(mut self, path: impl Into<String>) -> Self {
        if let Self::Kubernetes { token_path, .. } = &mut self {
            *token_path = path.into();
        }

        self
    }

    /// The login endpoint, relative to `/v1`.
    pub(crate) fn path(&self) -> Option<String> {
        match self {
            Self::Token(_) => None,
            Self::AppRole { mount, .. } => Some(format!("auth/{mount}/login")),
            Self::Kubernetes { mount, .. } => Some(format!("auth/{mount}/login")),
            Self::Jwt { mount, .. } => Some(format!("auth/{mount}/login")),
            Self::Certificate { mount, .. } => Some(format!("auth/{mount}/login")),
            // These two put the user in the path rather than the body, which is
            // why they cannot share an arm with the others.
            Self::Userpass {
                mount, username, ..
            } => Some(format!("auth/{mount}/login/{username}")),
            Self::Ldap {
                mount, username, ..
            } => Some(format!("auth/{mount}/login/{username}")),
        }
    }

    /// The credentials to POST.
    ///
    /// # Errors
    ///
    /// If a Kubernetes service-account token cannot be read.
    pub(crate) fn body(&self) -> Result<serde_json::Value, Error> {
        Ok(match self {
            Self::Token(_) => serde_json::json!({}),

            Self::AppRole {
                role_id, secret_id, ..
            } => serde_json::json!({ "role_id": role_id, "secret_id": secret_id }),

            Self::Kubernetes {
                role, token_path, ..
            } => {
                // Read per login, not once: the kubelet rotates projected
                // tokens, and a copy taken at startup expires with the pod
                // still running.
                let jwt = std::fs::read_to_string(token_path).map_err(|error| {
                    Error::remote(format!(
                        "vault: cannot read the service-account token at {token_path}: {error}"
                    ))
                })?;

                serde_json::json!({ "role": role, "jwt": jwt.trim() })
            }

            Self::Jwt { role, jwt, .. } => match role {
                Some(role) => serde_json::json!({ "role": role, "jwt": jwt }),
                None => serde_json::json!({ "jwt": jwt }),
            },

            Self::Userpass { password, .. } | Self::Ldap { password, .. } => {
                serde_json::json!({ "password": password })
            }

            Self::Certificate { name, .. } => match name {
                Some(name) => serde_json::json!({ "name": name }),
                None => serde_json::json!({}),
            },
        })
    }

    /// How to name this method in an error.
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            Self::Token(_) => "a supplied token",
            Self::AppRole { .. } => "approle",
            Self::Kubernetes { .. } => "kubernetes",
            Self::Jwt { .. } => "jwt",
            Self::Userpass { .. } => "userpass",
            Self::Ldap { .. } => "ldap",
            Self::Certificate { .. } => "cert",
        }
    }
}

/// A token, when it expires, and whether it can be renewed.
#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub(crate) secret: String,
    /// `None` for a token with no lease — a root token, or one Vault called
    /// non-expiring.
    expires: Option<Instant>,
    renewable: bool,
}

impl Token {
    pub(crate) fn new(secret: String, lease: Option<Duration>, renewable: bool) -> Self {
        Self {
            secret,
            // `checked_add` because the lease comes from the server: a `Vault`
            // that answers with a nonsense `lease_duration` would otherwise
            // panic the process on the arithmetic. A lease that does not fit in
            // an `Instant` is treated as no expiry, which is what a number that
            // large means anyway.
            expires: lease.and_then(|lease| Instant::now().checked_add(lease)),
            renewable,
        }
    }

    /// Whether this token should be refreshed before the next request.
    fn is_stale(&self) -> bool {
        self.expires
            .is_some_and(|expires| expires.saturating_duration_since(Instant::now()) < RENEW_WITHIN)
    }

    fn renewable(&self) -> bool {
        self.renewable
    }
}

/// The current token for one source.
///
/// A `Mutex` rather than a lock-free cell: logging in twice concurrently is
/// harmless but wasteful, and this is on the once-per-refresh path rather than
/// the once-per-request one.
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

    /// The token to use, logging in or renewing if it is time.
    ///
    /// `login` and `renew` are closures rather than methods so this module
    /// stays free of HTTP: what it decides is *when*, not *how*.
    ///
    /// # Errors
    ///
    /// Whatever logging in or renewing reports.
    pub(crate) fn token(
        &self,
        login: impl Fn() -> Result<Token, Error>,
        renew: impl Fn(&str) -> Result<Token, Error>,
    ) -> Result<String, Error> {
        let mut slot = self.lock();

        match slot.as_ref() {
            Some(token) if !token.is_stale() => return Ok(token.secret.clone()),

            // A renewal that fails is not a failure yet: the credentials are
            // still here, so falling through to a fresh login is strictly
            // better than reporting an error the caller can do nothing about.
            Some(token) if token.renewable() => {
                if let Ok(renewed) = renew(&token.secret) {
                    let secret = renewed.secret.clone();
                    *slot = Some(renewed);

                    return Ok(secret);
                }
            }

            _ => {}
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
    fn each_method_posts_to_its_own_endpoint() {
        assert_eq!(Auth::token("t").path(), None, "a token needs no login");
        assert_eq!(
            Auth::app_role("r", "s").path().as_deref(),
            Some("auth/approle/login")
        );
        assert_eq!(
            Auth::userpass("alice", "hunter2").path().as_deref(),
            Some("auth/userpass/login/alice"),
            "userpass puts the user in the path, not the body"
        );
        assert_eq!(
            Auth::ldap("alice", "hunter2").path().as_deref(),
            Some("auth/ldap/login/alice")
        );
    }

    #[test]
    fn a_method_can_be_mounted_anywhere() {
        assert_eq!(
            Auth::app_role("r", "s")
                .at_mount("approle-prod")
                .path()
                .as_deref(),
            Some("auth/approle-prod/login")
        );
        assert_eq!(
            Auth::token("t").at_mount("nowhere").path(),
            None,
            "a token has no mount to move"
        );
    }

    #[test]
    fn credentials_go_where_the_method_expects_them() {
        let body = Auth::app_role("role", "secret").body().unwrap();
        assert_eq!(body["role_id"], "role");
        assert_eq!(body["secret_id"], "secret");

        let body = Auth::userpass("alice", "hunter2").body().unwrap();
        assert_eq!(body["password"], "hunter2");
        assert!(
            body.get("username").is_none(),
            "the username is in the path"
        );

        let body = Auth::jwt("a.b.c").body().unwrap();
        assert_eq!(body["jwt"], "a.b.c");
        assert!(
            body.get("role").is_none(),
            "no role unless one was asked for"
        );

        let body = Auth::jwt("a.b.c").with_role("readers").body().unwrap();
        assert_eq!(body["role"], "readers");
    }

    #[test]
    fn a_missing_service_account_token_says_where_it_looked() {
        let error = Auth::kubernetes("app")
            .with_token_path("/no/such/token")
            .body()
            .expect_err("there is no token there");

        assert!(error.to_string().contains("/no/such/token"), "{error}");
    }

    #[test]
    fn a_lease_too_large_to_represent_is_treated_as_no_expiry() {
        // A server answering with nonsense must not be able to panic the
        // process on `Instant + Duration`.
        let token = Token::new("t".to_owned(), Some(Duration::from_secs(u64::MAX)), true);

        assert!(!token.is_stale());
    }

    #[test]
    fn a_token_with_no_lease_is_never_stale() {
        let token = Token::new("t".to_owned(), None, false);

        assert!(!token.is_stale(), "a root token does not expire");
    }

    #[test]
    fn a_token_near_its_expiry_is_stale() {
        let fresh = Token::new("t".to_owned(), Some(Duration::from_secs(3600)), true);
        let expiring = Token::new("t".to_owned(), Some(RENEW_WITHIN / 2), true);

        assert!(!fresh.is_stale());
        assert!(expiring.is_stale(), "it would expire mid-request");
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
                true,
            ))
        };
        let renew = |_: &str| panic!("a fresh token needs no renewal");

        assert_eq!(session.token(login, renew).unwrap(), "token");
        assert_eq!(session.token(login, renew).unwrap(), "token");
        assert_eq!(logins.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalidating_forces_another_login() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let logins = AtomicUsize::new(0);
        let session = Session::new();

        let login = || {
            let count = logins.fetch_add(1, Ordering::SeqCst);

            Ok(Token::new(
                format!("token-{count}"),
                Some(Duration::from_secs(3600)),
                true,
            ))
        };
        let renew = |_: &str| panic!("not reached");

        assert_eq!(session.token(login, renew).unwrap(), "token-0");

        session.invalidate();

        assert_eq!(
            session.token(login, renew).unwrap(),
            "token-1",
            "a 403 must be able to force a fresh login"
        );
    }

    #[test]
    fn a_stale_renewable_token_is_renewed_rather_than_replaced() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let logins = AtomicUsize::new(0);
        let session = Session::new();

        let expiring = || {
            logins.fetch_add(1, Ordering::SeqCst);

            Ok(Token::new("first".to_owned(), Some(RENEW_WITHIN / 2), true))
        };
        let renew = |secret: &str| {
            assert_eq!(secret, "first", "renewal presents the token it is renewing");

            Ok(Token::new(
                "renewed".to_owned(),
                Some(Duration::from_secs(3600)),
                true,
            ))
        };

        assert_eq!(session.token(expiring, renew).unwrap(), "first");
        assert_eq!(session.token(expiring, renew).unwrap(), "renewed");
        assert_eq!(
            logins.load(Ordering::SeqCst),
            1,
            "renewing must not cost a login"
        );
    }

    #[test]
    fn a_failed_renewal_falls_back_to_logging_in_again() {
        let session = Session::new();

        let login = || Ok(Token::new("fresh".to_owned(), Some(RENEW_WITHIN / 2), true));
        let refuse = |_: &str| Err(Error::remote("the lease is gone"));

        assert_eq!(session.token(login, refuse).unwrap(), "fresh");
        assert_eq!(
            session.token(login, refuse).unwrap(),
            "fresh",
            "a renewal Vault refuses is not a reason to fail; the credentials are still here"
        );
    }

    #[test]
    fn a_stale_non_renewable_token_goes_straight_to_a_fresh_login() {
        let session = Session::new();

        let login = || {
            Ok(Token::new(
                "fresh".to_owned(),
                Some(RENEW_WITHIN / 2),
                false,
            ))
        };
        let renew = |_: &str| panic!("a non-renewable token must not be renewed");

        assert_eq!(session.token(login, renew).unwrap(), "fresh");
        assert_eq!(session.token(login, renew).unwrap(), "fresh");
    }
}
