//! Reading configuration *from* a config server.
//!
//! The other half of this crate, behind the `client` feature: a
//! [`RemoteSource`] that fetches `GET /{application}/{profile}` and hands the
//! document to the engine, exactly as an etcd or a Vault source does. The two
//! halves live in one crate so that they are tested against each other —
//! every test in `tests/client.rs` drives this against the real router rather
//! than against a fixture of what the router is believed to return.
//!
//! ```no_run
//! use std::time::Duration;
//! use dynamic_config_server::client::ConfigServer;
//!
//! # fn main() -> Result<(), dynamic_config::Error> {
//! let source = ConfigServer::new("https://config.internal", "billing", "prod")
//!     .with_token(std::env::var("CONFIG_TOKEN").unwrap_or_default())
//!     .with_timeout(Duration::from_secs(5));
//! # let _ = source;
//! # Ok(())
//! # }
//! ```
//!
//! ## What it does not do
//!
//! **It does not subscribe.** `GET /{application}/{profile}/stream` carries a
//! generation, and a client that follows it calls
//! `refresh_remote()` when the number
//! moves — a loop of a dozen lines that belongs to whoever owns the reload
//! cadence. Building it in would mean this crate owning a task, a backoff and
//! a reconnect policy that the application is better placed to choose; what
//! this crate owes is the half that is fiddly to get right, which is the
//! bounded, deadline-covered, credential-carrying fetch below.
//!
//! **It does not verify provenance.** The document arrives as JSON with no
//! signature, so a client trusts the server exactly as far as TLS and the
//! bearer token take it. A deployment that needs more should read from the
//! store the server reads from.

mod http;

use std::sync::Arc;
use std::time::Duration;

use dynamic_config::{Error, Fetched, Format, RemoteSource};
use dynamic_config_store_core::tls::TlsConfig;
use dynamic_config_store_core::{redacted, LoneAuthority};

use http::{Budget, Connection, Endpoint};

/// How much of a response body is read before it is refused.
///
/// A configuration document that does not fit in a megabyte is not a
/// configuration document, and a client that trusts a server to send
/// something finite is a client that can be made to allocate until it dies.
const MOST_BYTES: usize = 1024 * 1024;

/// The default deadline for one fetch — connect, handshake, request and body.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// A [`RemoteSource`] reading one application-and-profile from a config
/// server.
///
/// The credential is a bearer token, scoped by the server to the applications
/// it may read; TLS with a private authority and a client certificate is
/// [`TlsConfig`], the same type every store crate in this workspace takes.
pub struct ConfigServer {
    url: String,
    application: String,
    profile: String,
    token: Option<String>,
    tls: TlsConfig,
    timeout: Duration,
    /// Built once from `tls`, on the first fetch: assembling a rustls
    /// configuration reads files, and a fetch path is not where that belongs.
    client: std::sync::OnceLock<Arc<rustls::ClientConfig>>,
    described: String,
}

impl ConfigServer {
    /// A source reading `{url}/{application}/{profile}`.
    ///
    /// `url` may carry a path prefix — `https://config.internal/config` — for
    /// a server mounted behind one. A userinfo component is refused rather
    /// than dropped: this server's credential is a bearer token, and a
    /// password in a url is a password in every log that url reaches.
    #[must_use]
    pub fn new(
        url: impl Into<String>,
        application: impl Into<String>,
        profile: impl Into<String>,
    ) -> Self {
        let (url, application, profile) = (url.into(), application.into(), profile.into());

        // Redacted as the description is built, rather than where each
        // message is written: this string is quoted into every error this
        // source raises and is what `describe()` returns — and one of those
        // errors is the *refusal* of a `user:password@` authority. Printing
        // the password while saying it is refused would be a leak with a
        // note attached.
        let described = format!(
            "config server {} {application}/{profile}",
            redacted(&url, LoneAuthority::Username)
        );

        Self {
            // Parsing is deferred to the first fetch so that `new` cannot
            // fail: a source that refuses to be *built* is awkward to place
            // in a builder chain, and the url is checked before it is used.
            url,
            application,
            profile,
            token: None,
            tls: TlsConfig::new(),
            timeout: DEFAULT_TIMEOUT,
            client: std::sync::OnceLock::new(),
            described,
        }
    }

    /// The bearer token this server issued for these applications.
    ///
    /// Without one the server answers `401` unless it was started with
    /// anonymous access explicitly enabled.
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// A private certificate authority, a client certificate, or both.
    ///
    /// The same [`TlsConfig`] the store crates take, so a deployment spells
    /// its trust once and uses it everywhere.
    #[must_use]
    pub fn with_tls(mut self, tls: TlsConfig) -> Self {
        self.tls = tls;
        self
    }

    /// The deadline for one fetch: connect, TLS handshake, request and body.
    ///
    /// Ten seconds by default. A fetch that hangs is a reload that never
    /// happens, and the loop above has no other way to notice.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The rustls configuration, built once.
    ///
    /// Building it reads files, so it is done on the first fetch and kept —
    /// not per fetch, and not at construction, where it would make `new`
    /// fallible for a source that may never be used.
    fn tls_client(&self, secure: bool) -> Result<Option<&Arc<rustls::ClientConfig>>, Error> {
        if !secure {
            return Ok(None);
        }

        if let Some(built) = self.client.get() {
            return Ok(Some(built));
        }

        let built = self.build_tls_client()?;

        Ok(Some(self.client.get_or_init(|| built)))
    }

    fn build_tls_client(&self) -> Result<Arc<rustls::ClientConfig>, Error> {
        use rustls::pki_types::pem::PemObject as _;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};

        let mut roots = rustls::RootCertStore::empty();

        // The platform store first, then the caller's authority on top: a
        // private CA is one *more* certificate to trust, which is the whole
        // reason this crate offers no way to turn verification off.
        for certificate in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(certificate);
        }

        if let Some(pem) = self.tls.ca_certificate_pem(&self.described)? {
            let mut added = 0;

            for certificate in CertificateDer::pem_slice_iter(&pem) {
                let certificate = certificate.map_err(|_| {
                    Error::remote(format!(
                        "{}: the certificate authority is not readable as PEM",
                        self.described
                    ))
                })?;

                roots
                    .add(certificate)
                    .map_err(|error| Error::remote(format!("{}: {error}", self.described)))?;
                added += 1;
            }

            if added == 0 {
                return Err(Error::remote(format!(
                    "{}: the certificate authority holds no certificate",
                    self.described
                )));
            }
        }

        let builder = rustls::ClientConfig::builder().with_root_certificates(roots);

        let Some((certificate, key)) = self.tls.client_certificate_pem(&self.described)? else {
            return Ok(Arc::new(builder.with_no_client_auth()));
        };

        let chain = CertificateDer::pem_slice_iter(&certificate)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                Error::remote(format!(
                    "{}: the client certificate is not readable as PEM",
                    self.described
                ))
            })?;

        // The key's own parse error is deliberately dropped: the one thing
        // such an error has to say is the line it choked on, and in a key
        // file that line is key material.
        let key = PrivateKeyDer::from_pem_slice(&key).map_err(|_| {
            Error::remote(format!(
                "{}: the client private key is not readable as PEM",
                self.described
            ))
        })?;

        builder
            .with_client_auth_cert(chain, key)
            .map(Arc::new)
            .map_err(|error| Error::remote(format!("{}: {error}", self.described)))
    }

    /// One fetch, on the current thread's runtime.
    async fn read(&self) -> Result<Fetched, Error> {
        let endpoint = Endpoint::parse(&self.url, &self.described)?;
        let path = endpoint.path(&format!("/{}/{}", self.application, self.profile));

        // One budget for the whole attempt, started here: the deadline
        // `with_timeout` documents is for a fetch, and a fetch is the
        // connect, the handshake, the request and the body together.
        let budget = Budget::starting(self.timeout);

        let secure = endpoint.secure;
        let mut connection =
            Connection::open(&endpoint, self.tls_client(secure)?, budget, &self.described).await?;

        let response = connection
            .get(
                &endpoint,
                &path,
                self.token.as_deref(),
                "application/json",
                budget,
                &self.described,
            )
            .await?;

        if !response.status().is_success() {
            return Err(http::refused(response.status(), &self.described));
        }

        let body = http::body(response, MOST_BYTES, budget, &self.described).await?;
        let text = String::from_utf8(body)
            .map_err(|_| Error::remote(format!("{}: the document is not UTF-8", self.described)))?;

        // The server answers `{application, profile, generation, config}`;
        // the engine wants the document, which is `config`. Reaching for it
        // by name rather than deserializing the envelope keeps this working
        // when the envelope grows a field.
        let document = extract(&text, &self.described)?;

        Ok(Fetched::new(document, Format::Json))
    }
}

/// The `config` member of the server's envelope, re-rendered.
fn extract(text: &str, described: &str) -> Result<String, Error> {
    let envelope: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| Error::remote(format!("{described}: the answer is not JSON")))?;

    let document = envelope.get("config").ok_or_else(|| {
        Error::remote(format!(
            "{described}: the answer carries no `config` member; is this a \
             config server?"
        ))
    })?;

    serde_json::to_string(document)
        .map_err(|_| Error::remote(format!("{described}: the document will not re-render")))
}

impl RemoteSource for ConfigServer {
    fn fetch(&self) -> Result<Fetched, Error> {
        // A blocking `fetch` on a client built from an async stack: one
        // runtime, current-thread, for this call only. A source is fetched
        // when a caller asks, minutes or hours apart, so the cost of starting
        // one is not on any path that matters — and owning a long-lived
        // runtime here would put a second one inside applications that
        // already have theirs.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                Error::remote(format!(
                    "{}: no runtime for the fetch: {error}",
                    self.described
                ))
            })?;

        runtime.block_on(self.read())
    }

    fn describe(&self) -> String {
        self.described.clone()
    }
}

impl std::fmt::Debug for ConfigServer {
    /// Shape only. The token is the credential and never prints; `TlsConfig`
    /// redacts its own key material; and the URL is redacted too, because a
    /// `user:password@` authority is refused at fetch time rather than at
    /// construction — so a source carrying one can be printed.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigServer")
            .field("url", &redacted(&self.url, LoneAuthority::Username))
            .field("application", &self.application)
            .field("profile", &self.profile)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("tls", &self.tls)
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_is_lifted_out_of_the_servers_envelope() {
        let text = r#"{"application":"billing","profile":"prod","generation":7,
                       "config":{"port":8080}}"#;

        assert_eq!(extract(text, "a server").unwrap(), r#"{"port":8080}"#);
    }

    #[test]
    fn an_answer_from_something_that_is_not_a_config_server_says_so() {
        let error = extract(r#"{"hello":"world"}"#, "a server").unwrap_err();

        assert!(error.to_string().contains("no `config` member"), "{error}");
    }

    /// A password in the URL is refused rather than sent — and the refusal
    /// must not be where it gets printed. `new` cannot fail, so the source
    /// exists, is `Debug`-printed and describes itself long before the
    /// parser gets to say no.
    #[test]
    fn a_password_in_the_url_reaches_neither_debug_nor_a_message() {
        let source = ConfigServer::new(
            "https://user:hunter2-do-not-print@config.internal",
            "billing",
            "prod",
        );

        let rendered = format!("{source:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");

        let described = source.describe();
        assert!(!described.contains("hunter2"), "{described}");
        assert!(described.contains("user:***@"), "{described}");

        // And the refusal itself, which quotes the description.
        let error = Endpoint::parse(&source.url, &source.described)
            .expect_err("a `user:password@` authority is refused");
        assert!(!error.to_string().contains("hunter2"), "{error}");
    }

    #[test]
    fn a_token_never_reaches_debug() {
        let source = ConfigServer::new("https://config.internal", "billing", "prod")
            .with_token("hunter2-do-not-print");

        let rendered = format!("{source:?}");

        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
}
