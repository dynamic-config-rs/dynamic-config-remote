//! The served sections, and the server that owns them.
//!
//! Each served section is a [`Dynamic<Document>`] — this crate is a *user*
//! of the library, not a reimplementation of it. That is the constraint the
//! design is built around: the same loader resolves the section, the same
//! watcher notices the file change, the same last-known-good behaviour keeps
//! a bad edit from taking the section down, and the same
//! [`ConfigStatus`](dynamic_config::ConfigStatus) answers for it. If this
//! server ever needs something the library cannot do, that is a library
//! change, not a server one.
//!
//! Nothing here polls. A section reloads because the file watcher said so,
//! and `/status` is a handful of atomic loads — so an idle server with a
//! thousand sections costs a thousand idle inotify registrations and no CPU.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dynamic_config::{Builder, Changes, ConfigStatus, Dynamic};

use crate::audit::{AuditEntry, AuditSink, StderrAudit};
use crate::auth::{Authenticator, Principal, Token};
use crate::config::{Refusal, SectionConfig, ServerConfig};
use crate::document::Document;

/// One served application-and-profile pair.
pub struct Section {
    application: String,
    profile: String,
    config: Dynamic<Document>,
    /// Dropping this stops the watch, so the handle lives as long as the
    /// section does and is never read.
    _watch: Option<dynamic_config::watch::WatchHandle>,
}

impl Section {
    /// The application this serves.
    #[must_use]
    pub fn application(&self) -> &str {
        &self.application
    }

    /// The profile this serves.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The document currently serving, or `None` before the first install.
    ///
    /// One atomic load. A handler takes it once and reuses the `Arc`, so a
    /// reload landing mid-request cannot show one response two generations.
    #[must_use]
    pub fn current(&self) -> Option<Arc<Document>> {
        self.config.current()
    }

    /// Installs since this section was created.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.config.generation()
    }

    /// The serving document together with a generation that is never ahead
    /// of it.
    ///
    /// Two atomic loads, and their order is the whole point.
    /// [`ConfigCell`](dynamic_config::ConfigCell) publishes a snapshot's
    /// metadata *after* the snapshot itself — deliberately, so that reading
    /// configuration stays one load with nothing to project out of it — so
    /// a generation read first may lag the document and can never lead it.
    ///
    /// Reading the document first inverts that, and the inversion is the
    /// harmful direction: a reload landing between the two loads would send
    /// the *previous* document under the *new* number, and a client that
    /// records it — or resumes its change stream with it — has been told it
    /// consumed an update whose contents it never received. This way round,
    /// the worst case is a response labelled one install behind its own
    /// contents: the client is told about that install again and fetches
    /// once more, which costs a round trip and loses nothing.
    #[must_use]
    pub fn installed(&self) -> Option<(u64, Arc<Document>)> {
        let generation = self.generation();

        self.current().map(|document| (generation, document))
    }

    /// What is true of this section right now. No I/O.
    #[must_use]
    pub fn status(&self) -> ConfigStatus {
        self.config.status()
    }

    /// Whether this section can be served.
    ///
    /// A section is ready when it has a document *and* the last reload
    /// installed one. The second half is the point of fronting a store: a
    /// bad edit upstream leaves the previous document serving — callers see
    /// no outage — and says so here, so a deployment pipeline notices
    /// before the next restart turns "stale but working" into "will not
    /// start".
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.current().is_some() && self.status().is_healthy()
    }

    /// A handle woken by every later install of this section.
    ///
    /// What the change stream awaits. It costs one `Arc` clone and a `u64`
    /// — no document, no diff, no queue — which is what lets a connection
    /// per pod be an ordinary number rather than a memory bound.
    #[must_use]
    pub fn changes(&self) -> Changes<Document> {
        self.config.changes()
    }

    /// One reload: read the sources, install if they are good.
    ///
    /// # Errors
    ///
    /// Whatever the load reports. A failure installs nothing and is counted
    /// in [`status`](Self::status).
    pub fn reload(&self) -> Result<(), dynamic_config::Error> {
        self.config.reload()
    }

    /// This section's sources, for the diagnostics that re-read them.
    ///
    /// Cloned rather than borrowed because `check` and `explain` run on the
    /// blocking pool, which needs to own what it reads.
    #[must_use]
    pub fn sources(&self) -> Builder<Document> {
        self.config.builder().clone()
    }
}

/// Keys and shape, never the document: a section holds resolved
/// configuration and this type is the obvious thing to `{:?}` in a handler.
impl fmt::Debug for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Section")
            .field("application", &self.application)
            .field("profile", &self.profile)
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

/// Everything the HTTP layer serves from.
///
/// Built by [`Server::start`], shared behind an `Arc` by the router, and
/// immutable afterwards: sections are established at startup and the set
/// does not change while the process runs. Adding one is a restart, which is
/// the same answer the rest of this workspace gives to "where do the sources
/// live".
pub struct Server {
    sections: BTreeMap<(String, String), Arc<Section>>,
    authenticator: Authenticator,
    audit: Arc<dyn AuditSink>,
    address: SocketAddr,
    /// The loaded certificate, key and client verifier, when this server
    /// terminates TLS itself. Loaded at startup so that a key that cannot be
    /// read is a refusal rather than the first connection's problem.
    #[cfg(feature = "tls")]
    tls: Option<crate::tls::Tls>,
    /// The ceiling on open change streams, and how many are open. Zero as a
    /// ceiling means the endpoint is not served at all.
    max_streams: usize,
    streams: Arc<AtomicUsize>,
}

/// One open change stream's place in the ceiling.
///
/// Held by the stream and released when it is dropped — which is when the
/// connection ends, however it ends: a client that goes away, a proxy that
/// times out, or a process that shuts down all drop the response body, and
/// the body owns this.
#[derive(Debug)]
pub struct StreamPermit {
    open: Arc<AtomicUsize>,
}

impl Drop for StreamPermit {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::Release);
    }
}

impl Server {
    /// Loads every section and refuses to start if anything is wrong.
    ///
    /// The order matters: [`ServerConfig::validate`] runs first and nothing
    /// is opened if it refuses, so a server that would have been
    /// world-readable never gets as far as reading a secret off disk.
    ///
    /// A section that will not load at startup is fatal. A config server
    /// that comes up serving nothing for one application is a silent outage
    /// for whoever needed it — better to fail the deployment.
    ///
    /// # Errors
    ///
    /// A [`Refusal`], a section that will not load, or a watch that will not
    /// start.
    pub fn start(config: &ServerConfig) -> Result<Self, StartupError> {
        Self::start_with(config, StderrAudit)
    }

    /// [`start`](Self::start), with somewhere else for the audit log to go.
    ///
    /// # Errors
    ///
    /// As [`start`](Self::start).
    pub fn start_with(config: &ServerConfig, audit: impl AuditSink) -> Result<Self, StartupError> {
        config.validate()?;

        let address = config.address()?;
        // Before any section is opened: a private key that cannot be read,
        // or that anybody on the host can read, is the same class of problem
        // as a roster that would serve `billing` to everyone, and it is
        // answered in the same place — at startup, with nothing yet open.
        #[cfg(feature = "tls")]
        let tls = config
            .tls
            .as_ref()
            .map(crate::tls::Tls::load)
            .transpose()
            .map_err(StartupError::Tls)?;
        let debounce = Duration::from_millis(config.watch_debounce_ms);
        let mut sections = BTreeMap::new();

        for described in &config.sections {
            let section = load_section(described, debounce)?;

            sections.insert(
                (described.application.clone(), described.profile.clone()),
                Arc::new(section),
            );
        }

        let mut clients: Vec<(Token, Principal)> = Vec::new();
        let mut anonymous = None;

        for client in &config.clients {
            let principal = Principal::new(&client.name, client.applications.clone());

            match &client.token {
                Some(token) => clients.push((token.clone(), principal)),
                None => anonymous = Some(principal),
            }
        }

        Ok(Self {
            sections,
            authenticator: Authenticator::new(clients, anonymous),
            audit: Arc::new(audit),
            address,
            #[cfg(feature = "tls")]
            tls,
            max_streams: config.max_stream_connections,
            streams: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The address the binary listens on.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The loaded TLS configuration, or `None` for a server that expects a
    /// terminator in front of it.
    ///
    /// What [`serve_tls`](crate::serve_tls) needs, and what tells an
    /// embedder which of the two serving paths to take.
    #[cfg(feature = "tls")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
    #[must_use]
    pub fn tls(&self) -> Option<&crate::tls::Tls> {
        self.tls.as_ref()
    }

    /// Where this server's audit lines go, for the parts of the serving path
    /// that are outside a request — a TLS handshake that was refused has no
    /// request to be recorded against, and is still something an operator
    /// looks for in the audit trail.
    #[cfg(feature = "tls")]
    pub(crate) fn audit_sink(&self) -> Arc<dyn AuditSink> {
        Arc::clone(&self.audit)
    }

    /// Who is calling, given the raw `Authorization` header.
    #[must_use]
    pub fn authenticate(&self, authorization: Option<&str>) -> Option<Principal> {
        self.authenticator.authenticate(authorization)
    }

    /// The section serving `application` at `profile`, if one does.
    ///
    /// **Call this only after authorising.** It is the lookup that would
    /// otherwise tell a caller whether a section exists, and the whole
    /// not-an-oracle property rests on nothing reaching it that has not
    /// already been granted the application.
    #[must_use]
    pub fn section(&self, application: &str, profile: &str) -> Option<&Arc<Section>> {
        // Owned keys because `BTreeMap<(String, String), _>` cannot be
        // probed with a pair of `&str` without a `Borrow` impl that does not
        // exist. The map is small — one entry per served pair — and this is
        // not the read path; the document itself comes from an atomic load.
        self.sections
            .get(&(application.to_owned(), profile.to_owned()))
    }

    /// Every served section.
    pub fn sections(&self) -> impl Iterator<Item = &Arc<Section>> {
        self.sections.values()
    }

    /// Whether every section is ready. What `/readyz` answers.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.sections().all(|section| section.is_ready())
    }

    /// Records one request.
    pub fn record(&self, entry: &AuditEntry) {
        self.audit.record(entry);
    }

    /// Whether this server serves change streams at all.
    #[must_use]
    pub fn streams_enabled(&self) -> bool {
        self.max_streams > 0
    }

    /// A place in the change-stream ceiling, if there is one free.
    ///
    /// A compare-and-swap loop rather than a fetch-add and a refund: an
    /// increment that overshoots is briefly visible to another request,
    /// which would let two callers each see the ceiling exceeded and both
    /// back off.
    #[must_use]
    pub fn open_stream(&self) -> Option<StreamPermit> {
        let mut open = self.streams.load(Ordering::Acquire);

        loop {
            if open >= self.max_streams {
                return None;
            }

            match self.streams.compare_exchange_weak(
                open,
                open + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(StreamPermit {
                        open: Arc::clone(&self.streams),
                    })
                }
                Err(current) => open = current,
            }
        }
    }

    /// How many change streams are open. For the tests and for a deployment
    /// that wants the number in its own metrics.
    #[must_use]
    pub fn open_streams(&self) -> usize {
        self.streams.load(Ordering::Acquire)
    }
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("address", &self.address)
            .field("sections", &self.sections.len())
            .field("anonymous", &self.authenticator.allows_anonymous())
            // The posture, never the material: `Tls`'s own `Debug` is
            // hand-written for the same reason this one is.
            .field("tls", &self.posture())
            .finish_non_exhaustive()
    }
}

impl Server {
    /// How this server's own socket is protected, in a few words — for a
    /// startup line, a `Debug` and an operator's first question.
    ///
    /// One of `none`, `tls` or `tls, client certificate required`. Never a
    /// path, never a subject, never a key: it says which of the three shapes
    /// this process is in and nothing about the material it is in it with.
    #[cfg(feature = "tls")]
    #[must_use]
    pub fn posture(&self) -> &'static str {
        match self.tls.as_ref().map(crate::tls::Tls::is_mutual) {
            Some(true) => "tls, client certificate required",
            Some(false) => "tls",
            None => "none",
        }
    }

    /// The same, for a build with no TLS in it: there is one answer.
    #[cfg(not(feature = "tls"))]
    #[must_use]
    pub fn posture(&self) -> &'static str {
        "none"
    }
}

fn load_section(described: &SectionConfig, debounce: Duration) -> Result<Section, StartupError> {
    let mut builder = Builder::<Document>::new(described.application.as_str());

    if described.whole_document {
        builder = builder.whole_document();
    }

    for file in &described.files {
        builder = builder.file(file.as_str());
    }
    if let Some(prefix) = &described.env_prefix {
        builder = builder.env(prefix.as_str());
    }

    let config = Dynamic::new(builder);

    config.init().map_err(|source| StartupError::Section {
        application: described.application.clone(),
        profile: described.profile.clone(),
        source,
    })?;

    // Zero disables watching, for a deployment that reloads by restarting or
    // by a means of its own. Anything else is the library's watcher, which
    // is event-driven — this server has no polling loop of its own and does
    // not want one.
    let watch = if debounce.is_zero() {
        None
    } else {
        Some(
            config
                .watch(debounce)
                .map_err(|source| StartupError::Watch {
                    application: described.application.clone(),
                    profile: described.profile.clone(),
                    source,
                })?,
        )
    };

    Ok(Section {
        application: described.application.clone(),
        profile: described.profile.clone(),
        config,
        _watch: watch,
    })
}

/// Why a server did not start.
#[derive(Debug)]
#[non_exhaustive]
pub enum StartupError {
    /// The configuration was refused before anything was opened.
    Refused(Refusal),
    /// A section's sources would not load.
    Section {
        /// The application.
        application: String,
        /// The profile.
        profile: String,
        /// What the load reported. Value-free, like every error in the
        /// library.
        source: dynamic_config::Error,
    },
    /// A section's watch would not start.
    Watch {
        /// The application.
        application: String,
        /// The profile.
        profile: String,
        /// What the watcher reported.
        source: std::io::Error,
    },
    /// The TLS material would not load. Carries no key material: see
    /// [`TlsError`](crate::tls::TlsError).
    #[cfg(feature = "tls")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
    Tls(crate::tls::TlsError),
}

impl From<Refusal> for StartupError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => write!(f, "refusing to start: {refusal}"),
            Self::Section {
                application,
                profile,
                source,
            } => write!(
                f,
                "refusing to start: the section `{application}`/`{profile}` will not load: \
                 {source}"
            ),
            Self::Watch {
                application,
                profile,
                source,
            } => write!(
                f,
                "refusing to start: the section `{application}`/`{profile}` cannot be \
                 watched: {source}"
            ),
            #[cfg(feature = "tls")]
            Self::Tls(error) => write!(f, "refusing to start: {error}"),
        }
    }
}

impl std::error::Error for StartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            Self::Section { source, .. } => Some(source),
            Self::Watch { source, .. } => Some(source),
            #[cfg(feature = "tls")]
            Self::Tls(error) => Some(error),
        }
    }
}
