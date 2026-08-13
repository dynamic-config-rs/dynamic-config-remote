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
//!
//! *When* a token is close enough to expiry to replace is [`Cached`]'s
//! decision, shared
//! with the Consul and Firestore crates. *Whether to renew or log in again* is
//! Vault's alone — it is the only one of the three that can extend a lease —
//! and so it stays here, in this module's `Session`.

use dynamic_config::Error;
use dynamic_config_store_core::credential::{Cached, Issued};

/// Where a Kubernetes service-account token is mounted, by convention.
pub const SERVICE_ACCOUNT_TOKEN: &str =
    dynamic_config_store_core::credential::SERVICE_ACCOUNT_TOKEN;

/// How to obtain a Vault token.
///
/// The variants that take a `mount` take the auth method's mount path, which is
/// its type by default — `approle` for AppRole, `kubernetes` for Kubernetes.
/// Mounting the same method twice under different paths is ordinary Vault
/// practice, which is why it is a parameter rather than a constant.
#[derive(Clone)]
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

// Debug is hand-written for every type on this page that can hold a secret:
// a derive prints payloads, and the payloads here are Vault tokens, AppRole
// secret ids, passwords and JWTs. What IS printed — variant, mount, role,
// username — is what a person debugging auth actually needs.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token(_) => f.write_str("Token(***)"),
            Self::AppRole { mount, role_id, .. } => f
                .debug_struct("AppRole")
                .field("mount", mount)
                .field("role_id", role_id)
                .finish_non_exhaustive(),
            Self::Kubernetes {
                mount,
                role,
                token_path,
            } => f
                .debug_struct("Kubernetes")
                .field("mount", mount)
                .field("role", role)
                .field("token_path", token_path)
                .finish(),
            Self::Jwt { mount, role, .. } => f
                .debug_struct("Jwt")
                .field("mount", mount)
                .field("role", role)
                .finish_non_exhaustive(),
            Self::Userpass {
                mount, username, ..
            } => f
                .debug_struct("Userpass")
                .field("mount", mount)
                .field("username", username)
                .finish_non_exhaustive(),
            Self::Ldap {
                mount, username, ..
            } => f
                .debug_struct("Ldap")
                .field("mount", mount)
                .field("username", username)
                .finish_non_exhaustive(),
            Self::Certificate { mount, name } => f
                .debug_struct("Certificate")
                .field("mount", mount)
                .field("name", name)
                .finish(),
        }
    }
}

/// A token and whether it can be renewed.
///
/// When it expires is not here: that is the one thing every token-caching
/// store in this family says the same way, so [`Cached`] keeps it.
#[derive(Clone)]
pub(crate) struct Token {
    pub(crate) secret: String,
    renewable: bool,
}

impl Token {
    pub(crate) fn new(secret: String, renewable: bool) -> Self {
        Self { secret, renewable }
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("secret", &"***")
            .field("renewable", &self.renewable)
            .finish()
    }
}

/// The current token for one source, and Vault's rule for replacing it.
///
/// The rule is Vault's alone: Consul issues login tokens and expects another
/// login, and Firestore's metadata server cannot extend anything, so those two
/// hand [`Cached`] a closure that simply obtains. This one first tries to
/// extend the lease it already has.
#[derive(Debug, Default)]
pub(crate) struct Session {
    held: Cached<Token>,
}

impl Session {
    pub(crate) const fn new() -> Self {
        Self {
            held: Cached::new(),
        }
    }

    /// The token to use, logging in or renewing if it is time.
    ///
    /// `login` and `renew` are closures rather than methods so this module
    /// stays free of HTTP: what it decides is *whether*, not *how*.
    ///
    /// # Errors
    ///
    /// Whatever logging in reports. A failed *renewal* is not an error: the
    /// credentials are still here, so falling through to a fresh login is
    /// strictly better than reporting something the caller can do nothing
    /// about.
    pub(crate) fn token(
        &self,
        login: impl Fn() -> Result<Issued<Token>, Error>,
        renew: impl Fn(&str) -> Result<Issued<Token>, Error>,
    ) -> Result<String, Error> {
        self.held
            .get(|current| match current {
                Some(token) if token.renewable => renew(&token.secret).or_else(|_| login()),
                // Nothing held, nothing renewable, or a token thrown away by
                // `invalidate` — all of them mean a fresh login.
                _ => login(),
            })
            .map(|token| token.secret)
    }

    /// Throws the current token away, so the next request logs in again.
    pub(crate) fn invalidate(&self) {
        self.held.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dynamic_config_store_core::credential::REFRESH_WITHIN;

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

    /// A token issued with `lease`, the way `token_from` builds one.
    fn issued(secret: &str, lease: Option<Duration>, renewable: bool) -> Issued<Token> {
        Issued {
            value: Token::new(secret.to_owned(), renewable),
            ttl: lease,
        }
    }

    // When a token is stale, what a lease too large to represent means, that
    // a login happens once rather than per request, and that `invalidate`
    // forces another are `dynamic-config-store-core`'s tests now: they were
    // the same assertions here, in the Consul crate and in the Firestore
    // crate, over the same code. What stays is what Vault does and the other
    // two cannot — renew.

    #[test]
    fn a_stale_renewable_token_is_renewed_rather_than_replaced() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let logins = AtomicUsize::new(0);
        let session = Session::new();

        let expiring = || {
            logins.fetch_add(1, Ordering::SeqCst);

            Ok(issued("first", Some(REFRESH_WITHIN / 2), true))
        };
        let renew = |secret: &str| {
            assert_eq!(secret, "first", "renewal presents the token it is renewing");

            Ok(issued("renewed", Some(Duration::from_secs(3600)), true))
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

        let login = || Ok(issued("fresh", Some(REFRESH_WITHIN / 2), true));
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

        let login = || Ok(issued("fresh", Some(REFRESH_WITHIN / 2), false));
        let renew = |_: &str| panic!("a non-renewable token must not be renewed");

        assert_eq!(session.token(login, renew).unwrap(), "fresh");
        assert_eq!(session.token(login, renew).unwrap(), "fresh");
    }

    /// A renewal that fails must not be able to lose the token: the fresh
    /// login that follows is what the caller ends up presenting, and if
    /// *that* fails too the previous token has to still be there.
    #[test]
    fn a_login_that_fails_after_a_failed_renewal_keeps_the_token_it_had() {
        let session = Session::new();

        assert_eq!(
            session
                .token(
                    || Ok(issued("first", Some(REFRESH_WITHIN / 2), true)),
                    |_: &str| panic!("nothing to renew yet"),
                )
                .unwrap(),
            "first"
        );

        let error = session
            .token(
                || Err(Error::auth("the role is gone")),
                |_: &str| Err(Error::remote("the lease is gone")),
            )
            .expect_err("neither renewing nor logging in worked");

        assert!(error.to_string().contains("the role is gone"), "{error}");

        session
            .token(
                || panic!("the token that is still held is renewed, not replaced"),
                |secret: &str| {
                    assert_eq!(secret, "first");

                    Ok(issued("renewed", Some(Duration::from_secs(3600)), true))
                },
            )
            .unwrap();
    }
}
