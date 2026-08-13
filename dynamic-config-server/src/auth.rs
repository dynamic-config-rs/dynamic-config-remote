//! Who is calling, and what they may read.
//!
//! Authorisation here is **per application**, not per server: a token that
//! reads `billing` reads `billing` and nothing else. That is the decision the
//! threat model turns on — a config server holds every service's
//! configuration, so a credential scoped to the server is every secret at
//! once, and the blast radius of a leaked pod token has to be the pod's own
//! section.
//!
//! There is exactly one credential shape: a bearer token, presented in
//! `Authorization`. A client certificate is **not** a second one — with
//! `[server.tls]` and a `client_ca` it is a gate the connection passes
//! before a request exists, and nothing in this module knows or cares that
//! it happened. JWT validation is deliberately absent rather than
//! half-present; see the crate documentation, and [`crate::tls`] for why a
//! certificate names no principal here.

use std::fmt;
use std::sync::Arc;

use serde::Deserialize;

/// The shortest token this server will accept in its configuration.
///
/// Long enough that guessing is not a strategy, and stated as a number
/// rather than as advice because a config server with a four-character token
/// is a config server with no authentication at all.
pub const MIN_TOKEN_LEN: usize = 32;

/// A bearer token, as configured.
///
/// Deserialises from a plain string. It has no accessor: the only thing
/// anything may do with a configured token is ask whether a presented one
/// equals it, and that comparison lives here so it cannot be written a
/// second, sloppier time somewhere else.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    /// A token from a string, for constructing a server in code.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The configured length, for the minimum-length refusal.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the token is empty — `len() == 0`, spelled for clippy.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `presented` is this token.
    ///
    /// The byte comparison does not stop at the first difference, so the
    /// time it takes does not reveal how much of a guess was right. What it
    /// does still reveal is the *length* — a presented token of a different
    /// length is rejected after fewer XORs — and that is accepted
    /// deliberately: a token's length is fixed by whoever issued it, is not
    /// the secret, and is bounded below by [`MIN_TOKEN_LEN`] anyway.
    #[must_use]
    pub fn matches(&self, presented: &str) -> bool {
        let (configured, presented) = (self.0.as_bytes(), presented.as_bytes());
        let mut difference = u8::from(configured.len() != presented.len());

        for (left, right) in configured.iter().zip(presented) {
            difference |= left ^ right;
        }

        difference == 0
    }

    /// Whether two *configured* tokens are the same, for the duplicate-token
    /// refusal. Neither side is attacker-supplied, so this is the one
    /// comparison here that has nothing to hide.
    pub(crate) fn same_as(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

/// Redacted, and the mistake AGENTS.md names: a derived `Debug` over a
/// credential is how three store crates shipped printing their tokens. Not
/// even the length, which would narrow a guess for nothing in return.
impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(***)")
    }
}

/// An authenticated caller and the applications it may read.
///
/// Cheap to clone — a request handler carries one — because the grants are
/// behind an `Arc` rather than copied per request.
#[derive(Clone, Debug)]
pub struct Principal(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    name: String,
    applications: Vec<String>,
}

impl Principal {
    /// A principal named `name`, granted `applications`.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        applications: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self(Arc::new(Inner {
            name: name.into(),
            applications: applications.into_iter().map(Into::into).collect(),
        }))
    }

    /// The configured client name. Safe to log: it comes from the server's
    /// own configuration, never from the request.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0.name
    }

    /// Whether this caller may read `application`.
    ///
    /// Exact match, no wildcards and no prefixes. A grant language is a
    /// place to make a mistake that reads as a working deployment, and
    /// nothing here needs one.
    #[must_use]
    pub fn may_read(&self, application: &str) -> bool {
        self.0
            .applications
            .iter()
            .any(|granted| granted == application)
    }

    /// The applications this caller may read.
    #[must_use]
    pub fn applications(&self) -> &[String] {
        &self.0.applications
    }
}

/// Turns an `Authorization` header into a [`Principal`], or into nothing.
///
/// Nothing is the default: a caller with no credential is a principal only
/// when the deployment configured an anonymous client *and* opted in with
/// `allow_anonymous`. Two switches, because one of them is the kind that
/// gets flipped in a hurry.
#[derive(Debug)]
pub struct Authenticator {
    clients: Vec<(Token, Principal)>,
    anonymous: Option<Principal>,
}

impl Authenticator {
    /// An authenticator over `clients`, with an optional anonymous
    /// principal for callers that present no credential.
    #[must_use]
    pub fn new(
        clients: impl IntoIterator<Item = (Token, Principal)>,
        anonymous: Option<Principal>,
    ) -> Self {
        Self {
            clients: clients.into_iter().collect(),
            anonymous,
        }
    }

    /// Whether an unauthenticated caller is somebody here.
    #[must_use]
    pub fn allows_anonymous(&self) -> bool {
        self.anonymous.is_some()
    }

    /// Who is calling, given the raw `Authorization` header.
    ///
    /// Three answers, and the middle one is the one worth stating: a header
    /// that is *present* but unusable — a wrong scheme, an unknown token —
    /// is **not** downgraded to anonymous. A caller that presented a
    /// credential meant to present that credential, and silently serving it
    /// the anonymous grants instead is how an expired token becomes a
    /// deployment that appears to work.
    #[must_use]
    pub fn authenticate(&self, authorization: Option<&str>) -> Option<Principal> {
        let Some(header) = authorization else {
            return self.anonymous.clone();
        };

        let presented = bearer(header)?;

        // Every configured token is compared, whether or not one has already
        // matched: stopping early would make the time taken depend on which
        // client is calling, which is a smaller oracle than the byte
        // comparison's but the same kind.
        let mut found = None;

        for (token, principal) in &self.clients {
            let hit = token.matches(presented);

            if hit && found.is_none() {
                found = Some(principal.clone());
            }
        }

        found
    }
}

/// The token out of `Bearer <token>`, case-insensitively on the scheme.
fn bearer(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;

    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }

    let token = token.trim_start();

    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authenticator() -> Authenticator {
        Authenticator::new(
            [(
                Token::new("0123456789abcdef0123456789abcdef"),
                Principal::new("billing-pod", ["billing"]),
            )],
            None,
        )
    }

    #[test]
    fn a_configured_token_authenticates_its_client() {
        let principal = authenticator()
            .authenticate(Some("Bearer 0123456789abcdef0123456789abcdef"))
            .expect("the configured token");

        assert_eq!(principal.name(), "billing-pod");
        assert!(principal.may_read("billing"));
    }

    #[test]
    fn the_scheme_is_case_insensitive_and_the_token_is_not() {
        let authenticator = authenticator();

        assert!(authenticator
            .authenticate(Some("bearer 0123456789abcdef0123456789abcdef"))
            .is_some());
        assert!(authenticator
            .authenticate(Some("Bearer 0123456789ABCDEF0123456789ABCDEF"))
            .is_none());
    }

    #[test]
    fn an_unusable_header_is_nobody_even_when_anonymous_is_configured() {
        let authenticator = Authenticator::new(
            [(
                Token::new("0123456789abcdef0123456789abcdef"),
                Principal::new("billing-pod", ["billing"]),
            )],
            Some(Principal::new("anonymous", ["demo"])),
        );

        // No credential at all is the anonymous principal...
        assert_eq!(
            authenticator
                .authenticate(None)
                .map(|who| who.name().to_owned()),
            Some("anonymous".to_owned())
        );
        // ...but a credential that does not work is not silently downgraded.
        assert!(authenticator.authenticate(Some("Bearer wrong")).is_none());
        assert!(authenticator.authenticate(Some("Basic abc")).is_none());
        assert!(authenticator.authenticate(Some("Bearer ")).is_none());
        assert!(authenticator.authenticate(Some("garbage")).is_none());
    }

    #[test]
    fn a_grant_is_exact() {
        let principal = Principal::new("who", ["billing"]);

        assert!(principal.may_read("billing"));
        assert!(!principal.may_read("bill"));
        assert!(!principal.may_read("billing-staging"));
        assert!(!principal.may_read("*"));
    }

    #[test]
    fn token_comparison_is_by_bytes_and_length() {
        let token = Token::new("0123456789abcdef0123456789abcdef");

        assert!(token.matches("0123456789abcdef0123456789abcdef"));
        assert!(!token.matches("0123456789abcdef0123456789abcdeg"));
        assert!(!token.matches("0123456789abcdef0123456789abcde"));
        assert!(!token.matches("0123456789abcdef0123456789abcdef0"));
        assert!(!token.matches(""));
    }

    /// The mistake AGENTS.md records, asserted rather than reviewed: a
    /// planted token must not survive a `{:?}` of anything holding it.
    #[test]
    fn debug_never_prints_a_token() {
        let token = Token::new("planted-token-value-0123456789ab");
        let authenticator =
            Authenticator::new([(token.clone(), Principal::new("who", ["billing"]))], None);

        for rendered in [format!("{token:?}"), format!("{authenticator:?}")] {
            assert!(
                !rendered.contains("planted-token-value"),
                "a credential escaped through Debug: {rendered}"
            );
        }
    }
}
