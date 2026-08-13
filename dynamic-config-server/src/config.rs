//! The server's own configuration, and every reason it refuses to start.
//!
//! A config server is the one program whose misconfiguration is not its own
//! problem: it hands other services their secrets. So the checks here are
//! refusals rather than warnings, and each one names the key that would fix
//! it. The list is deliberately long and deliberately loud — the failure
//! mode this exists to prevent is a server that starts, looks healthy, and
//! is serving `billing` to anyone who asks.

use std::fmt;
use std::net::SocketAddr;

use serde::Deserialize;

use crate::auth::{Token, MIN_TOKEN_LEN};

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

impl ServerConfig {
    /// Every reason this configuration will not start a server.
    ///
    /// Pure, and separate from starting, so the whole refusal surface is
    /// testable without a socket. [`Server::start`](crate::Server::start)
    /// calls it first and does nothing else if it says no.
    ///
    /// # Errors
    ///
    /// The first [`Refusal`] that applies, in the order this checks them:
    /// the shape of the roster before the shape of the network, because an
    /// operator fixing two problems would rather be told about the one that
    /// is about who can read what.
    pub fn validate(&self) -> Result<(), Refusal> {
        if self.sections.is_empty() {
            return Err(Refusal::NoSections);
        }
        if self.clients.is_empty() {
            return Err(Refusal::NoClients);
        }

        let mut seen = Vec::new();

        for section in &self.sections {
            // The predicate the handlers apply to the path segments, applied
            // where the mistake was made. Without this a section named with
            // a space in it — or one 65 characters long — loads, starts, and
            // reports ready, while every request for it is refused by
            // `is_name` before the section map is even consulted: a section
            // that exists and can never be reached.
            for (part, value) in [
                ("application", &section.application),
                ("profile", &section.profile),
            ] {
                if !crate::routes::is_name(value) {
                    return Err(Refusal::UnroutableSection {
                        application: section.application.clone(),
                        profile: section.profile.clone(),
                        part,
                    });
                }
            }

            let pair = (section.application.as_str(), section.profile.as_str());

            if seen.contains(&pair) {
                return Err(Refusal::DuplicateSection {
                    application: section.application.clone(),
                    profile: section.profile.clone(),
                });
            }

            seen.push(pair);
        }

        let mut names: Vec<&str> = Vec::new();
        let mut anonymous = 0;

        for client in &self.clients {
            if names.contains(&client.name.as_str()) {
                return Err(Refusal::DuplicateClient {
                    name: client.name.clone(),
                });
            }

            names.push(&client.name);

            match &client.token {
                None => {
                    anonymous += 1;

                    if !self.allow_anonymous {
                        return Err(Refusal::AnonymousNotAllowed {
                            client: client.name.clone(),
                        });
                    }
                    if anonymous > 1 {
                        return Err(Refusal::SeveralAnonymousClients);
                    }
                }
                Some(token) if token.len() < MIN_TOKEN_LEN => {
                    return Err(Refusal::WeakToken {
                        client: client.name.clone(),
                    });
                }
                Some(_) => {}
            }

            // A grant naming an application nothing serves is a typo that
            // reads as a working deployment right up to the first 404. Two
            // sections can share an application (one per profile), so the
            // grant is checked against the application names, not the pairs.
            for application in &client.applications {
                if !self
                    .sections
                    .iter()
                    .any(|section| &section.application == application)
                {
                    return Err(Refusal::UnservedGrant {
                        client: client.name.clone(),
                        application: application.clone(),
                    });
                }
            }
        }

        // Two clients sharing a token is not a smaller version of one client
        // with two grants: whichever is listed first silently wins, and the
        // audit log then names the wrong caller for every request.
        for (index, client) in self.clients.iter().enumerate() {
            let Some(token) = &client.token else { continue };

            for other in self.clients.iter().skip(index + 1) {
                if other.token.as_ref().is_some_and(|it| token.same_as(it)) {
                    return Err(Refusal::DuplicateToken);
                }
            }
        }

        let address = self
            .bind
            .parse::<SocketAddr>()
            .map_err(|_| Refusal::UnparsableBind {
                bind: self.bind.clone(),
            })?;

        // The whole matrix of TLS against the bind, in one place. Four
        // starting shapes and three refusals:
        //
        // | tls     | bind         | insecure | outcome                    |
        // |---------|--------------|----------|----------------------------|
        // | absent  | loopback     | either   | starts, in the clear       |
        // | absent  | non-loopback | false    | `ExposedBind`              |
        // | absent  | non-loopback | true     | starts; a terminator is in front |
        // | present | anything     | false    | starts, terminating TLS    |
        // | present | anything     | true     | `InsecureWithTls`          |
        // | present | anything     | —        | `TlsUnsupported` if the feature is off |
        match &self.tls {
            Some(tls) => {
                // A build without the feature has no rustls in it at all. The
                // block is still *parsed* — a key that only exists in some
                // builds would be an unknown field in the others, and this
                // crate refuses unknown fields — so the refusal is here,
                // where it can name the feature.
                if !cfg!(feature = "tls") {
                    return Err(Refusal::TlsUnsupported);
                }
                // Before the path checks, because this one is about who can
                // read what and they are about which file: an operator with
                // both problems would rather hear that revocation is not
                // checked than that a path is blank.
                if tls.crl.is_some() {
                    return Err(Refusal::RevocationUnsupported);
                }
                if tls.certificate.trim().is_empty() {
                    return Err(Refusal::TlsPathMissing { key: "certificate" });
                }
                if tls.key.trim().is_empty() {
                    return Err(Refusal::TlsPathMissing { key: "key" });
                }
                if tls
                    .client_ca
                    .as_ref()
                    .is_some_and(|it| it.trim().is_empty())
                {
                    return Err(Refusal::TlsPathMissing { key: "client_ca" });
                }
                if self.insecure {
                    return Err(Refusal::InsecureWithTls);
                }
            }
            None => {
                if !address.ip().is_loopback() && !self.insecure {
                    return Err(Refusal::ExposedBind {
                        bind: self.bind.clone(),
                    });
                }
            }
        }

        Ok(())
    }

    /// The validated bind address.
    ///
    /// # Errors
    ///
    /// If `bind` is not a literal `address:port`. A hostname is refused
    /// rather than resolved: which of a name's addresses a server ends up
    /// on is not a thing to discover at startup.
    pub fn address(&self) -> Result<SocketAddr, Refusal> {
        self.bind
            .parse::<SocketAddr>()
            .map_err(|_| Refusal::UnparsableBind {
                bind: self.bind.clone(),
            })
    }
}

/// Why a configuration will not start a server.
///
/// Every variant's `Display` names the key that fixes it, and none of them
/// carries a token: a refusal is printed to a terminal and scraped into a
/// log, which is the last place a credential should turn up.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// No `sections` — the server would serve nothing.
    NoSections,
    /// No `clients` — nobody could ever read anything.
    NoClients,
    /// Two sections claim the same application and profile.
    DuplicateSection {
        /// The application both claim.
        application: String,
        /// The profile both claim.
        profile: String,
    },
    /// A section names an application or profile no route can carry, so
    /// nothing could ever reach it.
    UnroutableSection {
        /// The application, as configured.
        application: String,
        /// The profile, as configured.
        profile: String,
        /// Which of the two was refused: `application` or `profile`.
        part: &'static str,
    },
    /// Two clients share a name.
    DuplicateClient {
        /// The name.
        name: String,
    },
    /// Two clients share a token.
    DuplicateToken,
    /// A configured token is shorter than [`MIN_TOKEN_LEN`].
    WeakToken {
        /// The client whose token is too short.
        client: String,
    },
    /// A client has no token and `allow_anonymous` is not set.
    AnonymousNotAllowed {
        /// The client with no token.
        client: String,
    },
    /// More than one client has no token, so "the anonymous caller" names
    /// two different grants.
    SeveralAnonymousClients,
    /// A client is granted an application no section serves.
    UnservedGrant {
        /// The client.
        client: String,
        /// The application it was granted.
        application: String,
    },
    /// A non-loopback `bind` with neither `tls` nor `insecure`.
    ExposedBind {
        /// The address.
        bind: String,
    },
    /// `bind` is not a literal `address:port`.
    UnparsableBind {
        /// What was written.
        bind: String,
    },
    /// `[server.tls]` in a build compiled without the `tls` feature.
    TlsUnsupported,
    /// `insecure` is set and `[server.tls]` is configured: an
    /// acknowledgement of something that is not true.
    InsecureWithTls,
    /// A `[server.tls]` key that must name a file names an empty string.
    TlsPathMissing {
        /// Which key.
        key: &'static str,
    },
    /// `tls.crl` is configured. This server checks no revocation and says so
    /// rather than accepting a key it would ignore.
    RevocationUnsupported,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSections => {
                f.write_str("no `sections` are configured: this server would serve nothing at all")
            }
            Self::NoClients => f.write_str(
                "no `clients` are configured: nothing could ever be read. Add a client with \
                 a `token` and the `applications` it may read, or an anonymous one with \
                 `allow_anonymous = true`",
            ),
            Self::DuplicateSection {
                application,
                profile,
            } => write!(
                f,
                "two `sections` claim `{application}`/`{profile}`; one application and \
                 profile is served by exactly one section"
            ),
            Self::UnroutableSection {
                application,
                profile,
                part,
            } => write!(
                f,
                "the section `{application}`/`{profile}` has a `{part}` no request can \
                 name: a path segment is up to 64 characters, starts with a letter or a \
                 digit, and carries only letters, digits, `.`, `_` and `-`. The server \
                 would start, report ready and answer `404` for that section forever"
            ),
            Self::DuplicateClient { name } => {
                write!(f, "two `clients` are named `{name}`; names identify a caller in the audit log and must be unique")
            }
            Self::DuplicateToken => f.write_str(
                "two `clients` share a `token`; the first listed would silently win every \
                 request and the audit log would name the wrong caller",
            ),
            Self::WeakToken { client } => write!(
                f,
                "the `token` for client `{client}` is shorter than {MIN_TOKEN_LEN} characters"
            ),
            Self::AnonymousNotAllowed { client } => write!(
                f,
                "client `{client}` has no `token`, which makes it the anonymous caller; set \
                 `allow_anonymous = true` to say that is intended, or give it a token"
            ),
            Self::SeveralAnonymousClients => f.write_str(
                "more than one client has no `token`; there is one anonymous caller, so it \
                 can have only one set of grants",
            ),
            Self::UnservedGrant {
                client,
                application,
            } => write!(
                f,
                "client `{client}` is granted `{application}`, which no section serves; a \
                 grant that matches nothing is a typo that reads as a working deployment"
            ),
            Self::ExposedBind { bind } => write!(
                f,
                "`bind` is `{bind}`, which is not loopback, and this server is terminating no \
                 TLS: that would put configuration — secrets included — on the network in the \
                 clear. Terminate TLS here with a `[server.tls]` section, or put a terminator \
                 in front of it and set `insecure = true` to say so, or bind loopback"
            ),
            Self::UnparsableBind { bind } => write!(
                f,
                "`bind` is `{bind}`, which is not a literal `address:port`; a hostname is \
                 refused rather than resolved"
            ),
            Self::TlsUnsupported => f.write_str(
                "`[server.tls]` is configured, but this binary was built without the `tls` \
                 feature and contains no TLS at all. Rebuild it with `--features tls`, or \
                 remove `[server.tls]` and put a terminator in front",
            ),
            Self::InsecureWithTls => f.write_str(
                "`insecure = true` is set and `[server.tls]` is configured. `insecure` \
                 acknowledges that this server's own socket is unencrypted, which is no longer \
                 true — remove it, so that removing the TLS section later refuses again \
                 instead of quietly serving in the clear",
            ),
            Self::TlsPathMissing { key } => {
                write!(f, "`tls.{key}` is empty; it has to name a PEM file")
            }
            Self::RevocationUnsupported => f.write_str(
                "`tls.crl` is configured, but this server checks no certificate revocation and \
                 will not pretend to. A CRL whose `nextUpdate` has passed is accepted silently \
                 by default, so the list would stop being true the moment it stopped being \
                 refreshed and nothing would report it; the one setting that refuses a stale \
                 list refuses every client along with it, which turns a publishing hiccup into \
                 an outage for every service at once. Remove the key. Issue short-lived client \
                 certificates, and revoke the `token` — delete the client's line and restart — \
                 which is the credential that actually authorises here",
            ),
        }
    }
}

impl std::error::Error for Refusal {}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(application: &str, profile: &str) -> SectionConfig {
        SectionConfig {
            application: application.to_owned(),
            profile: profile.to_owned(),
            files: vec!["config.toml".to_owned()],
            env_prefix: None,
            whole_document: false,
        }
    }

    fn client(name: &str, token: Option<&str>, applications: &[&str]) -> ClientConfig {
        ClientConfig {
            name: name.to_owned(),
            token: token.map(Token::new),
            applications: applications.iter().map(|it| (*it).to_owned()).collect(),
        }
    }

    const GOOD: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210";

    fn valid() -> ServerConfig {
        ServerConfig {
            sections: vec![section("billing", "prod")],
            clients: vec![client("billing-pod", Some(GOOD), &["billing"])],
            ..ServerConfig::default()
        }
    }

    #[test]
    fn a_complete_configuration_starts() {
        assert_eq!(valid().validate(), Ok(()));
    }

    #[test]
    fn an_empty_roster_is_refused_at_both_ends() {
        let mut config = valid();
        config.sections.clear();
        assert_eq!(config.validate(), Err(Refusal::NoSections));

        let mut config = valid();
        config.clients.clear();
        assert_eq!(config.validate(), Err(Refusal::NoClients));
    }

    #[test]
    fn a_duplicate_section_is_refused_but_two_profiles_are_not() {
        let mut config = valid();
        config.sections.push(section("billing", "prod"));

        assert_eq!(
            config.validate(),
            Err(Refusal::DuplicateSection {
                application: "billing".to_owned(),
                profile: "prod".to_owned(),
            })
        );

        let mut config = valid();
        config.sections.push(section("billing", "staging"));
        assert_eq!(config.validate(), Ok(()));
    }

    /// A section whose name no path segment can carry is refused where it
    /// was written, not answered `404` forever. The predicate is the
    /// handlers' own, so the two cannot drift.
    #[test]
    fn a_section_no_route_could_name_is_refused_at_startup() {
        for (part, application, profile) in [
            ("application", "billing api", "prod"),
            ("profile", "billing", ".hidden"),
            ("application", "", "prod"),
            ("profile", "billing", "../etc"),
        ] {
            let mut config = valid();
            config.sections = vec![section(application, profile)];
            config.clients = vec![client("pod", Some(GOOD), &[application])];

            assert_eq!(
                config.validate(),
                Err(Refusal::UnroutableSection {
                    application: application.to_owned(),
                    profile: profile.to_owned(),
                    part,
                }),
                "`{application}`/`{profile}` must be refused"
            );
        }

        // And the shapes a deployment actually uses still pass.
        let mut config = valid();
        config.sections = vec![section("billing-api.v2", "prod_1")];
        config.clients = vec![client("pod", Some(GOOD), &["billing-api.v2"])];
        assert_eq!(config.validate(), Ok(()));

        // Sixty-four characters is the ceiling, and it is inclusive.
        let mut config = valid();
        let long = "a".repeat(65);
        config.sections = vec![section(&long, "prod")];
        config.clients = vec![client("pod", Some(GOOD), &[&long])];
        assert!(matches!(
            config.validate(),
            Err(Refusal::UnroutableSection { .. })
        ));
    }

    #[test]
    fn duplicate_client_names_and_tokens_are_refused() {
        let mut config = valid();
        config
            .clients
            .push(client("billing-pod", Some(OTHER), &["billing"]));

        assert_eq!(
            config.validate(),
            Err(Refusal::DuplicateClient {
                name: "billing-pod".to_owned()
            })
        );

        let mut config = valid();
        config
            .clients
            .push(client("other", Some(GOOD), &["billing"]));

        assert_eq!(config.validate(), Err(Refusal::DuplicateToken));
    }

    #[test]
    fn a_short_token_is_refused() {
        let mut config = valid();
        config.clients = vec![client("billing-pod", Some("short"), &["billing"])];

        assert_eq!(
            config.validate(),
            Err(Refusal::WeakToken {
                client: "billing-pod".to_owned()
            })
        );
    }

    /// The switch the threat model turns on: no credential is nobody unless
    /// the deployment says otherwise, in as many words.
    #[test]
    fn anonymous_access_needs_an_explicit_opt_in() {
        let mut config = valid();
        config.clients = vec![client("anonymous", None, &["billing"])];

        assert_eq!(
            config.validate(),
            Err(Refusal::AnonymousNotAllowed {
                client: "anonymous".to_owned()
            })
        );

        config.allow_anonymous = true;
        assert_eq!(config.validate(), Ok(()));

        config.clients.push(client("also", None, &["billing"]));
        assert_eq!(config.validate(), Err(Refusal::SeveralAnonymousClients));
    }

    #[test]
    fn a_grant_nothing_serves_is_refused() {
        let mut config = valid();
        config.clients = vec![client("billing-pod", Some(GOOD), &["biling"])];

        assert_eq!(
            config.validate(),
            Err(Refusal::UnservedGrant {
                client: "billing-pod".to_owned(),
                application: "biling".to_owned(),
            })
        );
    }

    #[test]
    fn a_non_loopback_bind_is_refused_without_the_flag() {
        let mut config = valid();
        config.bind = "0.0.0.0:8080".to_owned();

        let refusal = config.validate().unwrap_err();
        assert_eq!(
            refusal,
            Refusal::ExposedBind {
                bind: "0.0.0.0:8080".to_owned()
            }
        );
        assert!(
            refusal.to_string().contains("insecure"),
            "the refusal has to name the key that fixes it: {refusal}"
        );

        config.insecure = true;
        assert_eq!(config.validate(), Ok(()));
    }

    fn tls(client_ca: Option<&str>) -> TlsConfig {
        TlsConfig {
            certificate: "/etc/tls/server.pem".to_owned(),
            key: "/etc/tls/server.key".to_owned(),
            client_ca: client_ca.map(ToOwned::to_owned),
            crl: None,
        }
    }

    /// The half of the matrix that only exists because TLS does: terminating
    /// it here is itself the answer to "that address is not loopback", so no
    /// acknowledgement is asked for.
    #[cfg(feature = "tls")]
    #[test]
    fn tls_is_the_acknowledgement_a_non_loopback_bind_needs() {
        let mut config = valid();
        config.bind = "0.0.0.0:8443".to_owned();
        config.tls = Some(tls(None));

        assert_eq!(config.validate(), Ok(()));
    }

    /// And the refusal that keeps `insecure` meaning one thing. Without it,
    /// a configuration that had both would keep starting after the TLS block
    /// was deleted — in the clear, on a public address, having been
    /// pre-approved months earlier.
    #[cfg(feature = "tls")]
    #[test]
    fn insecure_and_tls_together_are_a_contradiction_rather_than_a_no_op() {
        let mut config = valid();
        config.bind = "0.0.0.0:8443".to_owned();
        config.tls = Some(tls(Some("/etc/tls/ca.pem")));
        config.insecure = true;

        let refusal = config.validate().unwrap_err();

        assert_eq!(refusal, Refusal::InsecureWithTls);
        assert!(refusal.to_string().contains("insecure"), "{refusal}");
    }

    #[cfg(feature = "tls")]
    #[test]
    fn a_tls_section_that_names_no_file_is_refused_per_key() {
        for (key, mut broken) in [
            ("certificate", tls(None)),
            ("key", tls(None)),
            ("client_ca", tls(Some(""))),
        ] {
            match key {
                "certificate" => broken.certificate = String::new(),
                "key" => broken.key = "   ".to_owned(),
                _ => {}
            }

            let mut config = valid();
            config.tls = Some(broken);

            assert_eq!(config.validate(), Err(Refusal::TlsPathMissing { key }));
        }
    }

    /// Revocation is refused rather than half-implemented, and the refusal
    /// has to name what to do instead — an operator who configured a CRL is
    /// an operator who has a certificate to withdraw, and leaving them with
    /// "no" and no answer is how the key ends up back in the file next week.
    #[cfg(feature = "tls")]
    #[test]
    fn a_crl_is_refused_and_the_refusal_names_the_credential_that_can_be_revoked() {
        let mut config = valid();
        let mut with_crl = tls(Some("/etc/tls/ca.pem"));
        with_crl.crl = Some("/etc/tls/clients.crl".to_owned());
        config.tls = Some(with_crl);

        let refusal = config.validate().unwrap_err();

        assert_eq!(refusal, Refusal::RevocationUnsupported);

        let rendered = refusal.to_string();

        assert!(rendered.contains("`tls.crl`"), "{rendered}");
        assert!(rendered.contains("token"), "{rendered}");
        assert!(rendered.contains("short-lived"), "{rendered}");
    }

    /// The key is *understood* rather than unknown, which is the whole point
    /// of it existing: `deny_unknown_fields` would otherwise answer an
    /// operator asking for revocation with "unknown field", which reads as a
    /// misspelling and sends them looking for the right one.
    #[test]
    fn a_crl_key_parses_so_that_the_refusal_can_explain_rather_than_serde() {
        let config: ServerConfig = serde_json::from_str(
            r#"{"sections":[{"application":"a","profile":"p","files":["f"]}],
                "clients":[{"name":"c","token":"0123456789abcdef0123456789abcdef","applications":["a"]}],
                "tls":{"certificate":"c.pem","key":"k.pem","crl":"clients.crl"}}"#,
        )
        .expect("the key is understood, not unknown");

        assert_eq!(
            config.tls.expect("the block parsed").crl.as_deref(),
            Some("clients.crl")
        );
    }

    /// A build with no TLS in it says so, rather than serving in the clear
    /// on a port the operator believes is encrypted. This is the one refusal
    /// that is about the binary rather than the file.
    #[cfg(not(feature = "tls"))]
    #[test]
    fn a_build_without_the_feature_refuses_a_tls_section() {
        let mut config = valid();
        config.tls = Some(tls(None));

        let refusal = config.validate().unwrap_err();

        assert_eq!(refusal, Refusal::TlsUnsupported);
        assert!(refusal.to_string().contains("--features tls"), "{refusal}");
    }

    /// The block parses in *both* builds. It has to: `deny_unknown_fields`
    /// would otherwise turn a build without the feature into "unknown field
    /// `tls`", which reads as a typo rather than as a missing feature.
    #[test]
    fn a_tls_section_is_understood_whether_or_not_the_feature_is_on() {
        let config: ServerConfig = serde_json::from_str(
            r#"{"sections":[{"application":"a","profile":"p","files":["f"]}],
                "clients":[{"name":"c","token":"0123456789abcdef0123456789abcdef","applications":["a"]}],
                "tls":{"certificate":"c.pem","key":"k.pem","client_ca":"ca.pem"}}"#,
        )
        .expect("the shape is complete");

        let tls = config.tls.expect("the block is understood");

        assert_eq!(tls.certificate, "c.pem");
        assert_eq!(tls.client_ca.as_deref(), Some("ca.pem"));
    }

    /// Nothing above may have moved the plain-HTTP half of the matrix.
    #[test]
    fn without_tls_a_non_loopback_bind_still_needs_the_acknowledgement() {
        let mut config = valid();
        config.bind = "0.0.0.0:8080".to_owned();

        assert!(matches!(
            config.validate(),
            Err(Refusal::ExposedBind { .. })
        ));

        config.insecure = true;
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn ipv6_loopback_counts_as_loopback() {
        let mut config = valid();
        config.bind = "[::1]:8080".to_owned();

        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn a_hostname_is_refused_rather_than_resolved() {
        let mut config = valid();
        config.bind = "localhost:8080".to_owned();

        assert_eq!(
            config.validate(),
            Err(Refusal::UnparsableBind {
                bind: "localhost:8080".to_owned()
            })
        );
    }

    /// Refusals are printed at startup and end up in a log. None of them may
    /// carry a token there.
    #[test]
    fn no_refusal_prints_a_token() {
        let mut config = valid();
        config
            .clients
            .push(client("other", Some(GOOD), &["billing"]));

        let refusal = config.validate().unwrap_err();

        assert!(
            !refusal.to_string().contains(GOOD) && !format!("{refusal:?}").contains(GOOD),
            "a credential escaped through a refusal: {refusal}"
        );
    }

    /// The stream ceiling has a default a fleet does not reach, and zero is
    /// a legal value rather than a refusal: it is how a deployment says it
    /// does not want long-lived connections at all.
    #[test]
    fn the_stream_ceiling_defaults_high_and_zero_is_a_valid_answer() {
        let config: ServerConfig = serde_json::from_str(
            r#"{"sections":[{"application":"a","profile":"p","files":["f"]}],
                "clients":[{"name":"c","token":"0123456789abcdef0123456789abcdef","applications":["a"]}]}"#,
        )
        .expect("the shape is complete");

        assert_eq!(config.max_stream_connections, 4096);
        assert_eq!(config.validate(), Ok(()));

        let mut off = config;
        off.max_stream_connections = 0;
        assert_eq!(off.validate(), Ok(()));
    }

    #[test]
    fn a_key_the_server_does_not_know_is_refused() {
        let error = serde_json::from_str::<ServerConfig>(
            r#"{"sections":[],"clients":[],"allow_anonymou":true}"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field"), "{error}");
    }
}
