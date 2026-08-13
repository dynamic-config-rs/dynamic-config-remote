//! Getting an access token, and getting another one before it expires.
//!
//! *When* to fetch another one is [`Cached`]'s decision, shared with the
//! Consul and Vault crates; what a token is worth fetching from, and how, is
//! this module's. Firestore is the simplest of the three: the metadata server
//! mints a token and cannot extend one, so there is no renewal path to choose
//! between.

use std::time::Duration;

use dynamic_config::Error;
use dynamic_config_store_core::credential::{Cached, Issued};

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

/// The current token for one source.
#[derive(Debug, Default)]
pub(crate) struct Session {
    token: Cached<String>,
}

impl Session {
    pub(crate) const fn new() -> Self {
        Self {
            token: Cached::new(),
        }
    }

    /// The token to present, fetching one if it is time.
    ///
    /// `Ok(None)` when there is nothing to present, which is the right answer
    /// for the emulator.
    ///
    /// Only the metadata server's token is cached: the emulator presents
    /// nothing, and a token handed in from outside is the same string every
    /// time, so a cache in front of either would be a lock around a constant.
    pub(crate) fn token(&self, auth: &Auth, agent: &ureq::Agent) -> Result<Option<String>, Error> {
        match auth {
            Auth::Emulator => Ok(None),
            Auth::AccessToken(token) => Ok(Some(token.clone())),
            Auth::MetadataServer { url } => self
                // The previous token is ignored: the metadata server mints,
                // it does not extend.
                .token
                .get(|_previous| Self::mint(url, agent))
                .map(Some),
        }
    }

    /// One trip to the metadata server.
    fn mint(url: &str, agent: &ureq::Agent) -> Result<Issued<String>, Error> {
        let response: serde_json::Value = agent
            .get(url)
            // Without this header the metadata server refuses, which is what
            // stops a confused browser or a proxied request from reading a
            // workload's credentials.
            .header("Metadata-Flavor", "Google")
            .call()
            .map_err(|error| {
                let described = format!("firestore: the metadata server: {error}");

                // A token that could not be obtained is an auth failure, but
                // only when the metadata server *answered* and refused —
                // 403 is what it says when the `Metadata-Flavor` header is
                // missing or the workload has no identity attached. Being
                // unable to reach it at all is `Remote`: it comes back.
                match error {
                    ureq::Error::StatusCode(401 | 403) => Error::auth(described),
                    _ => Error::remote(described),
                }
            })?
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

        // Zero seconds is a lifetime nothing can be done with, so it is read
        // as "the server said nothing" rather than "already expired".
        let ttl = response
            .get("expires_in")
            .and_then(serde_json::Value::as_u64)
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs);

        Ok(Issued { value: secret, ttl })
    }

    /// Throws the current token away, so the next request fetches one.
    pub(crate) fn invalidate(&self) {
        self.token.invalidate();
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

    // When a token is stale, what a lifetime too large to represent means,
    // and that a fetch happens once rather than per request, are
    // `dynamic-config-store-core`'s tests now: they were the same three
    // assertions here, in the Consul crate and in the Vault crate, over the
    // same code.
}
