//! Kubernetes authentication: the caller's bearer token is a projected
//! service-account token, and the API server's TokenReview says whose.
//!
//! This is the server-side half of the organisation's identity-first
//! policy: a pod presents the identity Kubernetes already gave it, the
//! server asks the API server "is this real, and who is it", and the
//! grants map `namespace:serviceaccount` names to applications. **No
//! distributed client tokens at all** — nothing to mint, rotate, or
//! leak; revoking access is deleting the ServiceAccount or the grant.
//!
//! One review per unseen token, then a short cache: projected tokens
//! rotate on the kubelet's schedule (minutes to hours), so a sixty-
//! second cache absorbs the request rate without ever holding a
//! verdict long after the token could have been revoked.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::auth::Principal;

/// How long a TokenReview verdict is reused before the API server is
/// asked again.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// The reviewer: where the API server is, how this server authenticates
/// to it, and who is granted what.
pub struct KubernetesVerifier {
    /// `https://host:port`, from the in-cluster environment (or a test).
    api: String,
    /// This server's OWN service-account token, presented to the API
    /// server. Read per call: it is projected too, and rotates.
    own_token_path: std::path::PathBuf,
    /// The cluster CA, absent only under `#[cfg(test)]` plumbing.
    agent: ureq::Agent,
    /// Audience the token must carry, when the deployment pins one.
    audience: Option<String>,
    /// `namespace:serviceaccount` → the principal it becomes.
    grants: Vec<(String, Principal)>,
    /// Verdicts by token hash — the token itself is never stored.
    cache: Mutex<HashMap<u64, (Option<Principal>, Instant)>>,
}

impl std::fmt::Debug for KubernetesVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KubernetesVerifier")
            .field("api", &self.api)
            .field("grants", &self.grants.len())
            .finish_non_exhaustive()
    }
}

impl KubernetesVerifier {
    /// The in-cluster reviewer: API server address from the environment
    /// Kubernetes injects, trust from the mounted cluster CA, identity
    /// from the mounted service-account token.
    ///
    /// # Errors
    ///
    /// Outside a cluster (no `KUBERNETES_SERVICE_HOST`, no mounted CA) —
    /// at startup, where the refusal names what is missing, not at the
    /// first request.
    pub fn in_cluster(
        audience: Option<String>,
        grants: Vec<(String, Principal)>,
    ) -> Result<Self, String> {
        let host = std::env::var("KUBERNETES_SERVICE_HOST")
            .map_err(|_| "auth.kubernetes is enabled, but KUBERNETES_SERVICE_HOST is not set — this server is not running in a cluster".to_owned())?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".to_owned());

        let ca = std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/ca.crt")
            .map_err(|error| format!("auth.kubernetes: reading the cluster CA: {error}"))?;
        let mut roots = Vec::new();

        for item in ureq::tls::parse_pem(ca.as_bytes()) {
            match item {
                Ok(ureq::tls::PemItem::Certificate(certificate)) => roots.push(certificate),
                Ok(_) => {}
                Err(_) => {
                    return Err("auth.kubernetes: the cluster CA is not PEM".to_owned());
                }
            }
        }

        if roots.is_empty() {
            return Err("auth.kubernetes: the cluster CA held no certificate".to_owned());
        }

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::new_with_certs(&roots))
                    .build(),
            )
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();

        Ok(Self {
            api: format!("https://{host}:{port}"),
            own_token_path: "/var/run/secrets/kubernetes.io/serviceaccount/token".into(),
            agent,
            audience,
            grants,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// A reviewer pointed at an arbitrary endpoint with default trust —
    /// what the mock-backed tests use; never constructed in production.
    #[cfg(test)]
    #[must_use]
    pub fn for_tests(
        api: String,
        own_token_path: std::path::PathBuf,
        grants: Vec<(String, Principal)>,
    ) -> Self {
        Self {
            api,
            own_token_path,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(5)))
                .build()
                .into(),
            audience: None,
            grants,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Who `presented` is, if the API server vouches for it AND a grant
    /// names it. `None` is both "not a valid token" and "valid but not
    /// granted" — the caller's error stays 401 either way, and the
    /// distinction lives in this server's log, not the response.
    pub fn verify(&self, presented: &str) -> Option<Principal> {
        // The token never lands in the map; its keyed 64-bit hash does.
        // SipHash with a per-process random key: not forgeable from
        // outside, and a collision is lottery odds against yourself.
        let key = {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            presented.hash(&mut hasher);
            hasher.finish()
        };

        if let Some((verdict, at)) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        {
            if at.elapsed() < CACHE_TTL {
                return verdict.clone();
            }
        }

        let verdict = self.review(presented);

        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, (verdict.clone(), Instant::now()));

        verdict
    }

    fn review(&self, presented: &str) -> Option<Principal> {
        let own = std::fs::read_to_string(&self.own_token_path).ok()?;

        let mut spec = serde_json::json!({ "token": presented });

        if let Some(audience) = &self.audience {
            spec["audiences"] = serde_json::json!([audience]);
        }

        let body = serde_json::json!({
            "apiVersion": "authentication.k8s.io/v1",
            "kind": "TokenReview",
            "spec": spec,
        });

        let response: serde_json::Value = self
            .agent
            .post(format!(
                "{}/apis/authentication.k8s.io/v1/tokenreviews",
                self.api
            ))
            .header("authorization", format!("Bearer {}", own.trim()))
            .send_json(&body)
            .ok()?
            .body_mut()
            .read_json()
            .ok()?;

        if response["status"]["authenticated"] != serde_json::Value::Bool(true) {
            return None;
        }

        // "system:serviceaccount:<namespace>:<name>" — anything else
        // (a user, a node) is authenticated but not OUR vocabulary.
        let username = response["status"]["user"]["username"].as_str()?;
        let subject = username.strip_prefix("system:serviceaccount:")?;

        self.grants
            .iter()
            .find(|(granted, _)| granted == subject)
            .map(|(_, principal)| principal.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_granted_service_account_becomes_its_principal() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("binds");
        let api = format!("http://{}", server.server_addr());

        let handle = std::thread::spawn(move || {
            let request = server.recv().expect("a review arrives");

            assert_eq!(request.url(), "/apis/authentication.k8s.io/v1/tokenreviews");

            let response = serde_json::json!({
                "status": {
                    "authenticated": true,
                    "user": { "username": "system:serviceaccount:shop:billing" },
                },
            });

            request
                .respond(tiny_http::Response::from_string(response.to_string()))
                .expect("responds");
        });

        let token_file = tempfile::NamedTempFile::new().expect("a file");
        std::fs::write(token_file.path(), "own-token").expect("written");

        let verifier = KubernetesVerifier::for_tests(
            api,
            token_file.path().to_path_buf(),
            vec![(
                "shop:billing".to_owned(),
                Principal::new("shop/billing", ["shop"]),
            )],
        );

        let principal = verifier.verify("some-projected-token").expect("granted");

        assert_eq!(principal.name(), "shop/billing");
        assert!(principal.may_read("shop"));

        // The second ask is the cache, not a second review — the mock
        // accepted exactly one request and the thread has ended.
        handle.join().expect("one review");
        assert!(verifier.verify("some-projected-token").is_some());
    }

    #[test]
    fn an_ungranted_or_unauthenticated_token_is_nobody() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("binds");
        let api = format!("http://{}", server.server_addr());

        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let request = server.recv().expect("a review arrives");
                let response = serde_json::json!({
                    "status": {
                        "authenticated": true,
                        "user": { "username": "system:serviceaccount:other:nobody" },
                    },
                });

                request
                    .respond(tiny_http::Response::from_string(response.to_string()))
                    .expect("responds");
            }
        });

        let token_file = tempfile::NamedTempFile::new().expect("a file");
        std::fs::write(token_file.path(), "own-token").expect("written");

        let verifier = KubernetesVerifier::for_tests(
            api,
            token_file.path().to_path_buf(),
            vec![(
                "shop:billing".to_owned(),
                Principal::new("shop/billing", ["shop"]),
            )],
        );

        assert!(
            verifier.verify("token-one").is_none(),
            "valid but ungranted"
        );
        assert!(verifier.verify("token-two").is_none());
        handle.join().expect("two reviews");
    }
}
