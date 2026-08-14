//! Why a configuration will not start a server.
//!
//! One enum and its rendering. Every variant names the key that fixes it,
//! and none of them carries a token: a refusal is printed to a terminal and
//! scraped into a log, which is the last place a credential should turn up.
//! That property is tested in `tests/security.rs`, not asserted here.

use std::fmt;

use crate::auth::MIN_TOKEN_LEN;

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
