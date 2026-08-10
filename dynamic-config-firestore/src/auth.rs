//! Getting an access token, and getting another one before it expires.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use dynamic_config::Error;

/// How close to expiry a token may get before it is refreshed.
///
/// One name and one value across the three token-caching store crates, on
/// purpose. The margin is also the only cushion against clock skew: expiry
/// is computed from a *local* `Instant` plus a *server-reported* TTL, so any
/// disagreement between the server's issue time and our receipt time eats
/// into it. A minute absorbs the skew a real fleet actually has.
const REFRESH_WITHIN: Duration = Duration::from_secs(60);

/// Where a Google workload asks for its own token. Reachable from GKE, Cloud
/// Run, GCE and Cloud Functions, and from nowhere else — which is the security
/// property that makes it the right default.
const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

/// How to obtain an access token for the Firestore API.
#[derive(Clone)]
#[non_exhaustive]
pub enum Auth {
    /// No token at all, for the Firestore emulator.
    Emulator,

    /// A token somebody already obtained.
    ///
    /// `gcloud auth print-access-token` produces one; so does any library that
    /// already handles Google credentials. It expires, and this cannot renew
    /// it — install a fresh source when it does, or use
    /// [`metadata_server`](Self::metadata_server), which can.
    AccessToken(String),

    /// The workload's own identity, from the metadata server.
    ///
    /// The right answer on GKE, Cloud Run, GCE and Cloud Functions: no secret
    /// is distributed, the token is short-lived, and it is renewed here as it
    /// approaches expiry.
    MetadataServer {
        /// Where to ask. The conventional address unless a sidecar proxies it.
        url: String,
    },
}

impl Auth {
    /// A token somebody already obtained.
    pub fn access_token(token: impl Into<String>) -> Self {
        Self::AccessToken(token.into())
    }

    /// The workload's own identity, from the conventional metadata address.
    #[must_use]
    pub fn metadata_server() -> Self {
        Self::MetadataServer {
            url: METADATA_TOKEN_URL.to_owned(),
        }
    }

    /// Asks somewhere other than the conventional address.
    #[must_use]
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        if let Self::MetadataServer { url: existing } = &mut self {
            *existing = url.into();
        }

        self
    }
}

// Debug is hand-written for every type on this page that can hold a secret:
// a derive prints payloads, and the payload here is a live GCP access token.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Emulator => f.write_str("Emulator"),
            Self::AccessToken(_) => f.write_str("AccessToken(***)"),
            Self::MetadataServer { url } => {
                f.debug_struct("MetadataServer").field("url", url).finish()
            }
        }
    }
}

/// A token and when it expires.
struct Token {
    secret: String,
    /// `None` for a token nothing said an expiry for.
    expires: Option<Instant>,
}

impl Token {
    fn is_stale(&self) -> bool {
        self.expires.is_some_and(|expires| {
            expires.saturating_duration_since(Instant::now()) < REFRESH_WITHIN
        })
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("secret", &"***")
            .field("expires", &self.expires)
            .finish()
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

    /// The token to present, fetching one if it is time.
    ///
    /// `Ok(None)` when there is nothing to present, which is the right answer
    /// for the emulator.
    pub(crate) fn token(&self, auth: &Auth, agent: &ureq::Agent) -> Result<Option<String>, Error> {
        match auth {
            Auth::Emulator => Ok(None),
            Auth::AccessToken(token) => Ok(Some(token.clone())),
            Auth::MetadataServer { url } => self.metadata_token(url, agent).map(Some),
        }
    }

    fn metadata_token(&self, url: &str, agent: &ureq::Agent) -> Result<String, Error> {
        let mut slot = self.lock();

        if let Some(token) = slot.as_ref() {
            if !token.is_stale() {
                return Ok(token.secret.clone());
            }
        }

        let response: serde_json::Value = agent
            .get(url)
            // Without this header the metadata server refuses, which is what
            // stops a confused browser or a proxied request from reading a
            // workload's credentials.
            .header("Metadata-Flavor", "Google")
            .call()
            .map_err(|error| Error::remote(format!("firestore: the metadata server: {error}")))?
            .body_mut()
            .read_json()
            .map_err(|error| {
                Error::remote(format!(
                    "firestore: the metadata server's response was not JSON: {error}"
                ))
            })?;

        let secret = response
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                Error::remote("firestore: the metadata server returned no `access_token`")
            })?
            .to_owned();

        let expires = response
            .get("expires_in")
            .and_then(serde_json::Value::as_u64)
            .filter(|seconds| *seconds > 0)
            // `checked_add` because the value comes from the server: a nonsense
            // number would otherwise panic the process on the arithmetic.
            .and_then(|seconds| Instant::now().checked_add(Duration::from_secs(seconds)));

        *slot = Some(Token {
            secret: secret.clone(),
            expires,
        });

        Ok(secret)
    }

    /// Throws the current token away, so the next request fetches one.
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
    fn the_emulator_presents_nothing() {
        let agent = ureq::Agent::new_with_defaults();

        assert!(Session::new()
            .token(&Auth::Emulator, &agent)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_supplied_token_is_presented_as_it_is() {
        let agent = ureq::Agent::new_with_defaults();

        assert_eq!(
            Session::new()
                .token(&Auth::access_token("ya29.abc"), &agent)
                .unwrap()
                .as_deref(),
            Some("ya29.abc")
        );
    }

    #[test]
    fn the_metadata_url_can_be_moved_for_a_sidecar() {
        let auth = Auth::metadata_server().with_url("http://127.0.0.1:8081/token");

        let Auth::MetadataServer { url } = auth else {
            panic!("still a metadata auth");
        };

        assert_eq!(url, "http://127.0.0.1:8081/token");
    }

    #[test]
    fn a_token_near_its_expiry_is_stale() {
        let fresh = Token {
            secret: "t".to_owned(),
            expires: Instant::now().checked_add(Duration::from_secs(3600)),
        };
        let expiring = Token {
            secret: "t".to_owned(),
            expires: Instant::now().checked_add(REFRESH_WITHIN / 2),
        };

        assert!(!fresh.is_stale());
        assert!(expiring.is_stale());
    }

    #[test]
    fn a_lifetime_too_large_to_represent_is_treated_as_no_expiry() {
        // A server answering with nonsense must not panic the process.
        let token = Token {
            secret: "t".to_owned(),
            expires: Instant::now().checked_add(Duration::from_secs(u64::MAX)),
        };

        assert!(!token.is_stale());
    }
}
