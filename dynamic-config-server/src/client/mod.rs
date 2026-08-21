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
//! ## Watching
//!
//! It subscribes. `GET /{application}/{profile}/stream` carries a generation
//! number, and [`ConfigServer::watch`] follows it: connect, read events,
//! re-fetch the document when the number moves, reconnect with the
//! `Last-Event-ID` the server left off at. The reconnect is a comparison
//! rather than a replay — a generation subsumes every one before it — so
//! there is no window in which a change can be missed by being reconnected
//! past.
//!
//! ## What it does not do
//!
//! **It does not verify provenance.** The document arrives as JSON with no
//! signature, so a client trusts the server exactly as far as TLS and the
//! bearer token take it. A deployment that needs more should read from the
//! store the server reads from.

mod http;

use std::sync::Arc;
use std::time::Duration;

use dynamic_config::{Error, Fetched, Format, Pace, RemoteSource, WatchCapability, Watching};
use dynamic_config_store_core::attempts::Attempts;
use dynamic_config_store_core::tls::TlsConfig;
use dynamic_config_store_core::{guarded, redacted, LoneAuthority};

use http::{Budget, Connection, Endpoint, Events, Get};

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
    token_file: Option<std::path::PathBuf>,
    tls: TlsConfig,
    timeout: Duration,
    /// Built once from `tls`, on the first fetch: assembling a rustls
    /// configuration reads files, and a fetch path is not where that belongs.
    client: std::sync::OnceLock<Arc<rustls::ClientConfig>>,
    described: String,
    /// Where a watch reports an attempt that came back with nothing.
    ///
    /// Nobody, unless [`reporting_to`](Self::reporting_to) says otherwise —
    /// the same door the eight store crates carry, and for the same reason:
    /// a watch swallows transport failures by design, so without this the
    /// only thing that knew the server had been unreachable for an hour was
    /// the loop, and `status().reachable()` went on answering `true`.
    attempts: Attempts,
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
            token_file: None,
            tls: TlsConfig::new(),
            timeout: DEFAULT_TIMEOUT,
            client: std::sync::OnceLock::new(),
            described,
            attempts: Attempts::default(),
        }
    }

    /// Report failed attempts to `sink`, so an outage is visible.
    ///
    /// A watch swallows transport failures on purpose — outliving one is
    /// what a watch is for — and the cost of that is a store that has been
    /// unreachable for an hour while `status().reachable()` says otherwise.
    /// This is the door the eight store crates carry, and the same
    /// discipline: take the sink where the watch is wired, once, because a
    /// sink captures the generation of the source installed at that moment
    /// and that is what fences a winding-down loop's failures away from its
    /// replacement.
    ///
    /// **A failure moves the failure streak and nothing else.** The fetch
    /// count and the clock are left alone, so a dashboard keeps ageing
    /// `last_fetch` while `up` goes to zero — the pair an alert wants. It
    /// changes nothing about what [`watch`](Self::watch) returns.
    #[must_use]
    pub fn reporting_to(mut self, sink: dynamic_config::RemoteSink) -> Self {
        self.attempts = Attempts::to(sink);
        self
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

    /// The bearer token read from a file, **re-read at every fetch** —
    /// for credentials something else rotates underneath this client,
    /// first among them a pod's projected service-account token (the
    /// server's `[kubernetes]` auth reviews exactly that). Wins over
    /// [`with_token`](Self::with_token) when both are set.
    #[must_use]
    pub fn with_token_file(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.token_file = Some(path.into());
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

        // The file wins, and is read per fetch: a projected token that
        // rotated between two fetches must present its NEW self. One
        // reader, shared with the watch — two copies of "which credential
        // do we present" is one more than a credential should have.
        let bearer = self.bearer()?;

        let response = connection
            .get(
                &endpoint,
                Get {
                    path: &path,
                    token: bearer.as_deref(),
                    accept: "application/json",
                    resume: None,
                },
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

impl ConfigServer {
    /// How long a stream may be silent before it is treated as dead.
    ///
    /// The server sends a comment every fifteen seconds precisely so that
    /// silence means something; three of those is a connection a proxy has
    /// dropped without telling either end.
    const IDLE: Duration = Duration::from_secs(50);

    /// Follows the change stream, fetching whenever the generation moves.
    ///
    /// Blocks until `watching` is stopped, so it belongs on a thread of its
    /// own. `interval` is the reconnect pace rather than a poll: the stream
    /// pushes, and this is how long to wait before trying again when it
    /// ends. The waits are spread across a fleet and grow after a failure,
    /// so a server coming back up is not met by every pod at once.
    ///
    /// Each document is delivered only when it differs from the last one:
    /// a generation moves for every install, and an install that changed
    /// nothing this caller can see should wake nothing.
    ///
    /// # Errors
    ///
    /// If `on_change` refuses a document. A connection failing is not an
    /// error — reconnecting through an outage is what this is for.
    pub fn watch<F>(
        &self,
        watching: &Watching,
        interval: Duration,
        mut on_change: F,
    ) -> Result<(), Error>
    where
        F: FnMut(Fetched) -> Result<(), Error>,
    {
        // One runtime for the whole watch, unlike `fetch`'s per-call one: a
        // watch is a long-lived thing by definition, so the argument that
        // makes a per-call runtime free does not apply to it.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                Error::remote(format!(
                    "{}: no runtime for the watch: {error}",
                    self.described
                ))
            })?;

        // Settled once, before the loop, because neither can come right by
        // being retried: a URL this crate cannot parse and a TLS
        // configuration it cannot build are the caller's to fix, and a loop
        // that swallowed them reconnected forever, delivered nothing and
        // said nothing. The eight stores validate what is deterministic up
        // front for the same reason.
        let endpoint = Endpoint::parse(&self.url, &self.described)?;
        self.tls_client(endpoint.secure)?;

        runtime.block_on(async {
            let mut pace = Pace::new(interval);
            let mut resume: Option<String> = None;
            let mut last: Option<Fetched> = None;

            while watching.keep_going() {
                match self
                    .subscribed(watching, &mut resume, &mut last, &mut on_change)
                    .await
                {
                    Ok(()) => pace.succeeded(),
                    // The caller refusing a document is the one failure this
                    // loop does not own: it is a decision, not an outage.
                    Err(Ended::Refused(error)) => return Err(error),
                    // Everything else is swallowed on purpose, credentials
                    // included: a token file rotating between two
                    // connections looks exactly like a token that is wrong,
                    // and a watch that stopped on the first would be a pod
                    // that never recovered from a routine rotation.
                    Err(Ended::Disconnected) => pace.failed(),
                }

                sleep_while(watching, pace.next_wait()).await;
            }

            Ok(())
        })
    }

    /// One connection's worth of stream, from subscribe to close.
    async fn subscribed<F>(
        &self,
        watching: &Watching,
        resume: &mut Option<String>,
        last: &mut Option<Fetched>,
        on_change: &mut F,
    ) -> Result<(), Ended>
    where
        F: FnMut(Fetched) -> Result<(), Error>,
    {
        let endpoint = Endpoint::parse(&self.url, &self.described)
            .map_err(|error| self.disconnected(&error))?;
        let path = endpoint.path(&format!("/{}/{}/stream", self.application, self.profile));

        // The budget covers getting the stream open — connect, handshake,
        // request — and stops there. A deadline on the stream itself would
        // be a deadline on the configuration not changing.
        let budget = Budget::starting(self.timeout);
        let secure = endpoint.secure;
        let tls = self
            .tls_client(secure)
            .map_err(|error| self.disconnected(&error))?;
        let mut connection = Connection::open(&endpoint, tls, budget, &self.described)
            .await
            .map_err(|error| self.disconnected(&error))?;

        let bearer = self.bearer().map_err(|error| self.disconnected(&error))?;
        let response = connection
            .get(
                &endpoint,
                Get {
                    path: &path,
                    token: bearer.as_deref(),
                    accept: "text/event-stream",
                    resume: resume.as_deref(),
                },
                budget,
                &self.described,
            )
            .await
            .map_err(|error| self.disconnected(&error))?;

        if !response.status().is_success() {
            let status = response.status();
            let refusal = http::refused(status, &self.described);

            // **A 404 is an answer, not an outage.** The stream path is
            // absent when a deployment sets `max_stream_connections = 0`,
            // and when a URL names a prefix this server does not mount —
            // neither comes right by reconnecting, and a loop that retried
            // them forever was a watch that delivered nothing and said
            // nothing. Everything else is waited out, credentials included:
            // a token file rotating between two connections looks exactly
            // like a token that is wrong.
            if status == 404 {
                return Err(Ended::Refused(refusal));
            }

            return Err(self.disconnected(&refusal));
        }

        let mut events = Events::new(response);

        // Whether the *first* event of this connection is the server saying
        // where the document stands rather than that it moved. It is,
        // exactly when this subscription sent no `Last-Event-ID`.
        let mut opening = resume.is_none();

        while watching.keep_going() {
            let next = events
                .next(watching, Self::IDLE, &self.described)
                .await
                .map_err(|error| self.disconnected(&error))?;

            let Some(event) = next else {
                // The server closed it. Ordinary — a rolling restart does
                // exactly this — and the loop above reconnects.
                return Ok(());
            };

            // A keep-alive says the connection is there and nothing else.
            // Round the loop rather than through the fetch: re-reading the
            // whole document every fifteen seconds of quiet is the poll this
            // client exists to replace, and coming back here is also what
            // notices a watch that has been stopped.
            if !event.carried {
                continue;
            }

            // The event says *something landed*; the document is fetched
            // from the endpoint that serves documents. Reading the number
            // out of the payload is not needed for that and is not done:
            // an install is an install.
            let _ = event.data;

            // **The opening event is not a change.** A first subscription
            // sends no `Last-Event-ID`, so the server opens with where the
            // document stands — which is the current value, and
            // "the current value is not delivered at startup" is the
            // contract all nine sources keep. Its id is still worth having:
            // a reconnect resumes from it.
            if opening {
                opening = false;
                *resume = event.id.or_else(|| resume.take());

                continue;
            }

            let fetched = self
                .read()
                .await
                .map_err(|error| self.disconnected(&error))?;

            if last.as_ref() != Some(&fetched) {
                *last = Some(fetched.clone());

                // Through `guarded`, as every other store delivers: a
                // callback that panics ends the watch with an error rather
                // than unwinding through this loop and killing the caller's
                // thread with the `RemoteWatch` handle still looking alive.
                guarded(on_change, fetched, &self.described).map_err(Ended::Refused)?;
            }

            // **Advanced last, and only on the way out.** Moving it before
            // the fetch meant a fetch that failed still counted: the
            // reconnect carried a `Last-Event-ID` for a generation this
            // client never read, the server saw nothing newer, and the
            // change was lost until the next install — the one window the
            // module documentation says cannot exist.
            if let Some(id) = event.id {
                *resume = Some(id);
            }
        }

        Ok(())
    }

    /// An attempt that came back with nothing, reported and then forgotten.
    ///
    /// Reporting happens here rather than at each call site so that a
    /// failure branch added later cannot be the one that forgets — the
    /// same shape the eight store crates use.
    fn disconnected(&self, error: &Error) -> Ended {
        self.attempts.failed(error);

        Ended::Disconnected
    }

    /// The bearer token to present, file first.
    fn bearer(&self) -> Result<Option<String>, Error> {
        match &self.token_file {
            Some(file) => std::fs::read_to_string(file)
                .map(|token| Some(token.trim().to_owned()))
                .map_err(|error| {
                    Error::auth(format!(
                        "{}: reading the bearer token file: {error}",
                        self.described
                    ))
                }),
            None => Ok(self.token.clone()),
        }
    }
}

/// Why one connection's worth of stream ended.
///
/// The distinction the loop above acts on, and the only one it needs: a
/// connection that failed is waited out and tried again, and a caller that
/// refused a document has made a decision the loop has no business
/// overriding.
enum Ended {
    /// The connection failed, or the server refused the subscription. The
    /// error is not carried past here: the loop waits and tries again, and a
    /// message per reconnect through an outage is a log nobody can read. It
    /// *is* reported first — see `ConfigServer::disconnected`.
    Disconnected,
    Refused(Error),
}

/// Sleeps for `total`, waking early once the watch is stopped.
async fn sleep_while(watching: &Watching, total: Duration) {
    const SLICE: Duration = Duration::from_millis(250);

    let mut left = total;

    while left > Duration::ZERO && watching.keep_going() {
        let slice = left.min(SLICE);

        tokio::time::sleep(slice).await;
        left -= slice;
    }
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

    /// Native: the server pushes a generation down a `text/event-stream`.
    fn watch_capability(&self) -> WatchCapability {
        WatchCapability::Native
    }

    fn watch(
        &self,
        watching: &Watching,
        interval: Duration,
        on_change: &mut dyn FnMut(Fetched) -> Result<(), Error>,
    ) -> Result<(), Error> {
        ConfigServer::watch(self, watching, interval, on_change)
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
            .field("token_file", &self.token_file)
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
