//! The server's own configuration, and every reason it refuses to start.
//!
//! A config server is the one program whose misconfiguration is not its own
//! problem: it hands other services their secrets. So the checks here are
//! refusals rather than warnings, and each one names the key that would fix
//! it. The list is deliberately long and deliberately loud — the failure
//! mode this exists to prevent is a server that starts, looks healthy, and
//! is serving `billing` to anyone who asks.
//!
//! Three files, because the module answers three questions: **what a
//! configuration is** (here), **why one is refused**
//! ([`refusal`](refusal)), and **which refusal applies**
//! ([`validate`](validate)). The split is a file boundary and nothing more
//! — every type below is re-exported from `crate` exactly where it was.

mod refusal;
mod validate;

pub use refusal::Refusal;

use serde::Deserialize;

use crate::auth::Token;

/// The default bind address: loopback, so a server started with no `bind`
/// at all is reachable from nowhere but its own host.
fn default_bind() -> String {
    "127.0.0.1:8080".to_owned()
}

/// The default debounce for the file watcher, in milliseconds.
fn default_debounce_ms() -> u64 {
    250
}

/// The default ceiling on concurrent change-stream connections.
///
/// A thousand-pod fleet reconnecting at once is the shape this number is
/// chosen against: each connection costs one `Changes` handle and one
/// registered waker and holds no document, so a thousand is nothing — and a
/// ceiling that a fleet does not reach in normal operation is a backstop
/// against a client that reconnects in a loop rather than a rate limit.
fn default_max_streams() -> usize {
    4096
}

/// One served application-and-profile pair.
///
/// The section key inside the files **is** the application name: a document
/// served as `billing` is the `[billing]` table of the configured files.
/// That is one fact rather than two, and it keeps a URL and a file readable
/// against each other.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionConfig {
    /// The application, which is both the first path segment and the
    /// section key inside the files.
    pub application: String,
    /// The profile, which is the second path segment.
    ///
    /// A profile here is a *different set of files*, chosen by the
    /// operator, rather than the library's `profile_env` — that one is a
    /// process-wide environment variable, and a server serving two profiles
    /// cannot have two of those at once.
    pub profile: String,
    /// The files to merge, in order; later files win.
    pub files: Vec<String>,
    /// An environment-variable prefix layered above the files, as in
    /// `APP_` reading `APP_BILLING_*`.
    #[serde(default)]
    pub env_prefix: Option<String>,
    /// Whether these files carry a section header at all.
    ///
    /// `false` — the default — reads the application as a top-level key
    /// inside each file, so one file can hold several applications.
    ///
    /// `true` says each file *is* this section: `{"host": …, "port": …}`
    /// with nothing above it. A config server is routinely pointed at
    /// files somebody else's tool writes, and those files have no reason
    /// to carry a header this server invented.
    #[serde(default)]
    pub whole_document: bool,
}

/// Where the server's own certificate, key and client CA live.
///
/// Its presence is what turns TLS on; there is no `enabled` key, because a
/// block that names a certificate and does nothing is a deployment that
/// believes it is encrypted and is not.
///
/// ```toml
/// [server.tls]
/// certificate = "/etc/dynamic-config/server.pem"
/// key = "/etc/dynamic-config/server.key"
/// client_ca = "/etc/dynamic-config/clients-ca.pem"   # optional; see below
/// ```
///
/// Only paths live here. The key's *bytes* are read once, at startup, by
/// [`Tls::load`](crate::tls::Tls::load), and never reach a diagnostic — see
/// [`TlsError`](crate::tls::TlsError).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM holding the server's certificate, then any intermediates, leaf
    /// first.
    pub certificate: String,
    /// PEM holding that certificate's private key: PKCS#8, PKCS#1 or SEC1.
    ///
    /// On Unix the server **refuses to start** if this file is readable by
    /// anything but its owner, for the same reason it refuses a token under
    /// 32 characters.
    pub key: String,
    /// PEM holding the certificate authority every client certificate must
    /// chain to.
    ///
    /// Present means **mutual TLS is required**: a caller that presents no
    /// certificate, or one signed by anything else, does not complete the
    /// handshake and never becomes a request. Absent means the server
    /// authenticates itself to callers and asks for nothing back.
    ///
    /// A certificate is a second gate, never a second identity: it is not an
    /// alternative to the bearer token and it does not name a caller. See
    /// the [`tls`](crate::tls) module.
    ///
    /// **No revocation is checked.** A certificate that chains here is good
    /// until it expires; see [`crl`](Self::crl).
    #[serde(default)]
    pub client_ca: Option<String>,
    /// A certificate revocation list — **a startup refusal**, never a file
    /// this server reads.
    ///
    /// The key exists so that an operator who reaches for revocation is told
    /// that this server does not check it, rather than being told `unknown
    /// field 'crl'` and going looking for a different spelling. It is the
    /// same reason [`tls`](ServerConfig::tls) itself is parsed in a build
    /// without the feature: a security-relevant key that reads as a typo is
    /// worse than one that reads as a decision.
    ///
    /// The decision, and it was measured rather than assumed
    /// (`RevocationUnsupported`'s message is the short form): rustls will
    /// accept a CRL whose `nextUpdate` passed years ago without a word,
    /// because `ExpirationPolicy::Ignore` is the default — so the twenty
    /// lines that look like revocation are a check that stops being true the
    /// moment the file stops being refreshed, with nothing anywhere
    /// reporting it. The one switch that refuses a stale list,
    /// `enforce_revocation_expiration`, refuses **every** client while it is
    /// stale, which turns a CRL publishing hiccup into a fleet-wide
    /// configuration outage. Neither is a posture this crate will ship, and
    /// a file watcher does not rescue it: the failure to catch is the
    /// *absence* of a write, and no filesystem event fires for that.
    ///
    /// What to do instead is in [`tls`](crate::tls): short-lived client
    /// certificates, and revoke the bearer token — the credential that
    /// actually authorises, and the one this server can withdraw by removing
    /// a line.
    #[serde(default)]
    pub crl: Option<String>,
}

/// One caller, and what it may read.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// The client's name. Appears in the audit log and nowhere else.
    pub name: String,
    /// The bearer token this client presents.
    ///
    /// Absent means **anonymous**: this client is whoever calls without a
    /// credential. That needs [`allow_anonymous`](ServerConfig::allow_anonymous)
    /// as well, so an omitted token can never be the accident that opens a
    /// server up.
    #[serde(default)]
    pub token: Option<Token>,
    /// The applications this client may read, by name. Exact, no wildcards.
    pub applications: Vec<String>,
}

impl ClientConfig {
    /// Whether this client is the anonymous one.
    #[must_use]
    pub fn is_anonymous(&self) -> bool {
        self.token.is_none()
    }
}

/// Everything the server needs to start.
///
/// `deny_unknown_fields` on purpose: a misspelled `allow_anonymous` that
/// silently stayed `false` would be a harmless surprise, and a misspelled
/// `applications` that silently granted nothing would be a confusing one —
/// but a key this struct does not know is, in a security-relevant file, a
/// key the operator believes is doing something. Refuse it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// The address to listen on. Loopback unless said otherwise.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Permits a bind address that is not loopback **when this server
    /// terminates no TLS**.
    ///
    /// Without [`tls`](Self::tls), a non-loopback bind means configuration —
    /// secrets included — crossing a network in the clear unless something
    /// in front of it is doing the encryption. Setting this is the operator
    /// saying that something is.
    ///
    /// With [`tls`](Self::tls) it is a **refusal**, not a no-op. The word
    /// acknowledges an unencrypted socket, and there is not one; leaving it
    /// set while TLS is on would make it stop meaning anything, so that
    /// removing the TLS block later would quietly reopen the port instead of
    /// refusing.
    #[serde(default)]
    pub insecure: bool,
    /// TLS termination, and the client certificate that goes with it.
    ///
    /// Absent — the default — is a server that speaks plain HTTP and expects
    /// a terminator in front of it, exactly as before. Present is this
    /// process terminating TLS itself, and needs the `tls` Cargo feature: a
    /// build without it **refuses to start** rather than ignoring the block.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// Permits a client with no token.
    #[serde(default)]
    pub allow_anonymous: bool,
    /// The file watcher's debounce, in milliseconds. Zero disables
    /// watching, which is what an operator who reloads by other means
    /// wants.
    #[serde(default = "default_debounce_ms")]
    pub watch_debounce_ms: u64,
    /// How many change-stream connections may be open at once, across every
    /// caller and every section.
    ///
    /// **Zero turns the endpoint off**, and a server with it off answers
    /// `/stream` with the same 404 as everything else it does not serve — a
    /// deployment that does not want long-lived connections says so once
    /// here rather than in whatever is in front of it.
    ///
    /// It is a backstop, not a rate limit. Per-*caller* limiting belongs to
    /// the thing in front, which is the only place that sees every replica's
    /// share of a caller; what this bounds is the total number of sockets one
    /// process will hold open on this endpoint, so a client reconnecting in
    /// a loop cannot take the process with it.
    #[serde(default = "default_max_streams")]
    pub max_stream_connections: usize,
    /// The served applications and profiles.
    pub sections: Vec<SectionConfig>,
    /// The callers.
    pub clients: Vec<ClientConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            insecure: false,
            tls: None,
            allow_anonymous: false,
            watch_debounce_ms: default_debounce_ms(),
            max_stream_connections: default_max_streams(),
            sections: Vec::new(),
            clients: Vec::new(),
        }
    }
}
