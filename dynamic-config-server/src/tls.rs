//! TLS termination, and the client certificate that goes with it.
//!
//! This crate used to terminate no TLS at all, on the grounds that a second
//! TLS stack doubles the CVE surface of a program whose job is holding other
//! people's secrets and that every target deployment already has a
//! terminator. The first half of that is still true and is why TLS is
//! **opt-in twice** — a Cargo feature and a `[server.tls]` block — but the
//! second half was never a rule about deployments, only about the ones that
//! had been looked at. A server on a machine with no ingress, and a
//! deployment that wants the config server's own socket to demand a client
//! certificate, are both real, and neither is served by an answer that lives
//! in somebody else's process.
//!
//! # What a client certificate is here
//!
//! **A second gate, not a second identity.** A caller that presents a
//! certificate signed by the configured CA gets a TCP connection and nothing
//! else: it is still nobody until it presents a bearer token, and the token
//! is still what names it in the audit log and what its grants hang off.
//!
//! The two rejected alternatives are worth stating, because both are
//! defensible and only one of these three can be in the code:
//!
//! - **A certificate *instead* of a token.** That makes the certificate a
//!   way to bypass the token, which is the opposite of what a second factor
//!   is for, and it moves authorisation onto a subject name — a string
//!   issued by whoever holds the CA key, which is frequently not whoever
//!   maintains this server's roster.
//! - **A certificate that *names* a client** (subject → principal). One
//!   identity, two spellings, and a second roster to keep in step with the
//!   first. Worse, it makes the CA an authorisation authority: anybody who
//!   can get a certificate with `CN=billing-pod` out of it reads `billing`,
//!   and CAs are asked for certificates by processes that have never heard
//!   of this server's grants.
//!
//! So the certificate says *this connection came from a machine the
//! deployment provisioned*, and the token says *this caller may read
//! `billing`*. Two independent facts, both required, neither able to stand
//! in for the other. Nothing in the router changes because of TLS,
//! and that is the property to preserve: a request that reached a handler
//! is authorised exactly as it was before.
//!
//! # Posture
//!
//! Not invented here. The protocol versions are rustls's
//! `with_safe_default_protocol_versions` and the cipher suites and key
//! exchange groups are the `ring` provider's defaults, in the provider's own
//! preference order. This module chooses no suite, disables no version and
//! reorders nothing — the whole reason to use rustls is that these decisions
//! are made by people who track them, and a hand-picked list here would be
//! this crate's opinion frozen at the day it was written.
//!
//! The one thing it does set is ALPN: `http/1.1`, and only that, because
//! axum is compiled here with `http1` alone. A client that negotiated `h2`
//! against a server that cannot speak it is a connection that fails after
//! the handshake instead of during it.
//!
//! # Revocation, and why there is none
//!
//! A certificate that chains to `client_ca` is good until it expires. This
//! module configures no CRL, and `[server.tls] crl` is a startup refusal
//! ([`Refusal::RevocationUnsupported`](crate::Refusal::RevocationUnsupported))
//! rather than a key that is read — because a decorative revocation check is
//! worse than an acknowledged absence, and this one would be decorative.
//!
//! rustls has the machinery, and it is about twenty lines:
//! `ClientCertVerifierBuilder::with_crls`, a revocation-check depth and an
//! unknown-status policy. What sank it is not the code but the freshness,
//! and both halves were measured rather than assumed (the measurement is
//! `tests/tls.rs::the_measurement_behind_refusing_revocation_still_holds`,
//! which fails if either default moves):
//!
//! - **By default a stale CRL is used, silently.** rustls's
//!   `ExpirationPolicy::Ignore` means a list whose `nextUpdate` passed in
//!   2020 still verifies a handshake in 2026 with no error, no warning and
//!   nothing in any log. So the naive build is a server that reports it
//!   checks revocation and, from whenever the file stopped being refreshed,
//!   does not. Worse, it *tests green*: the obvious test — revoke a
//!   certificate, assert the handshake fails — passes against a six-year-old
//!   list, because revocation itself keeps working. Only freshness rots, and
//!   nothing observes it.
//! - **The switch that fixes that breaks everything else.**
//!   `enforce_revocation_expiration` refuses a stale list — and refuses every
//!   *clean, unrevoked* client along with it, for as long as it is stale.
//!   That makes the CA's publishing cadence a liveness dependency of every
//!   service's configuration, in the one program a fleet cannot fetch
//!   configuration without.
//!
//! The obvious escape is to re-read the file on the same watcher the
//! sections use, and it does not work: a watcher fires on a **write**, and
//! the failure to catch is the *absence* of one. No filesystem event says
//! "this should have been rewritten an hour ago". Catching that needs a
//! clock — a periodic wake-up — which is the polling loop this crate does
//! not have and whose absence is a stated property of it. An HTTP
//! distribution point is a fetch loop, and OCSP is a second protocol and a
//! third dependency.
//!
//! What settles it is that the certificate is a **gate, not an identity**. A
//! stolen certificate on its own buys a TCP connection and a 401; reading
//! anything needs the bearer token. So a CRL here would revoke the credential
//! that does not authorise, on a schedule this server cannot verify, while
//! the credential that *does* authorise is a line in a file the operator
//! already controls — deleted and restarted in seconds, with no CA, no
//! cadence and no new way to fail. Issue short-lived certificates; revoke the
//! token.

use std::fmt;
use std::fs::{File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::RootCertStore;

use crate::config::TlsConfig;

/// A loaded TLS configuration: a certificate chain, a private key, and
/// either a client-certificate verifier or the absence of one.
///
/// Built by [`Tls::load`] during [`Server::start`](crate::Server::start), so
/// a key that cannot be read, a key with permissions that make it not a
/// secret, and a certificate that does not match it are all startup
/// refusals rather than the first connection's problem.
pub struct Tls {
    config: Arc<rustls::ServerConfig>,
    mutual: bool,
}

impl Tls {
    /// Reads the certificate, the key and — if one is configured — the
    /// client CA, and builds the rustls configuration from them.
    ///
    /// # Errors
    ///
    /// A [`TlsError`] naming the file and the fix. None of them carries a
    /// byte of the key: see [`TlsError`].
    pub fn load(config: &TlsConfig) -> Result<Self, TlsError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let chain = read_certificates(Path::new(&config.certificate), Role::Certificate)?;
        let key = read_private_key(Path::new(&config.key))?;

        let verifier = match &config.client_ca {
            Some(authority) => {
                let mut roots = RootCertStore::empty();

                for certificate in read_certificates(Path::new(authority), Role::ClientCa)? {
                    roots
                        .add(certificate)
                        .map_err(|source| TlsError::UnusableClientCa {
                            path: PathBuf::from(authority),
                            source,
                        })?;
                }

                WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                    .build()
                    .map_err(|_| TlsError::UnusableClientCa {
                        path: PathBuf::from(authority),
                        // `VerifierBuilderError` is "no anchors" or "a bad
                        // CRL", and this crate configures no CRLs, so the
                        // only reachable case is the first. Reported as our
                        // own sentence rather than the builder's, so the
                        // message names the key that fixes it.
                        source: rustls::Error::General(
                            "it contains no usable certificate authority".to_owned(),
                        ),
                    })?
            }
            // Not `allow_unauthenticated()`. A client certificate that the
            // server would accept the *absence* of is decorative: it can be
            // dropped by anything in the path and nothing notices. Either
            // `client_ca` is configured and a certificate is required, or it
            // is not configured and none is asked for.
            None => WebPkiClientVerifier::no_client_auth(),
        };

        let mutual = config.client_ca.is_some();
        let mut server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|_| TlsError::Provider)?
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key)
            .map_err(|_| TlsError::KeyDoesNotMatch {
                certificate: PathBuf::from(&config.certificate),
                key: PathBuf::from(&config.key),
            })?;

        server.alpn_protocols = vec![b"http/1.1".to_vec()];

        Ok(Self {
            config: Arc::new(server),
            mutual,
        })
    }

    /// The rustls configuration, for the acceptor.
    #[must_use]
    pub fn server_config(&self) -> Arc<rustls::ServerConfig> {
        Arc::clone(&self.config)
    }

    /// Whether a client certificate is required.
    ///
    /// True exactly when `client_ca` was configured: there is no third state
    /// in which a certificate is asked for and not required.
    #[must_use]
    pub fn is_mutual(&self) -> bool {
        self.mutual
    }
}

/// Hand-written, and the reason is the same one AGENTS.md records for
/// tokens: this type holds a private key, and a derive prints every field.
/// Two booleans is everything a debugger needs from it.
impl fmt::Debug for Tls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tls")
            .field("mutual", &self.mutual)
            .field("alpn", &"http/1.1")
            .finish_non_exhaustive()
    }
}

/// Which file is being read, for a message that says which one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Certificate,
    ClientCa,
}

impl Role {
    fn key(self) -> &'static str {
        match self {
            Self::Certificate => "certificate",
            Self::ClientCa => "client_ca",
        }
    }
}

/// The permission bits a private key may not have: anything for group or
/// other.
///
/// A key readable by every account on the host is not a key, in the same way
/// a four-character token is not authentication — and this crate already
/// refuses that. The fix is one command, and it is named in the message.
#[cfg(unix)]
const FORBIDDEN_BITS: u32 = 0o077;

fn read_certificates(path: &Path, role: Role) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let mut certificates = Vec::new();

    for certificate in
        CertificateDer::pem_file_iter(path).map_err(|source| unreadable(path, role, &source))?
    {
        certificates.push(certificate.map_err(|_| TlsError::UnusablePem {
            path: path.to_owned(),
            key: role.key(),
        })?);
    }

    if certificates.is_empty() {
        return Err(TlsError::NoCertificates {
            path: path.to_owned(),
            key: role.key(),
        });
    }

    Ok(certificates)
}

/// Opens the key, refuses it if its permissions make it not a secret, and
/// parses it.
///
/// The order is the point: the permission check runs against the metadata of
/// the **open file**, so there is no window in which the file this checked
/// and the file this read are two different files.
fn read_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let mut file = File::open(path).map_err(|source| TlsError::Unreadable {
        path: path.to_owned(),
        key: "key",
        source,
    })?;
    let metadata = file.metadata().map_err(|source| TlsError::Unreadable {
        path: path.to_owned(),
        key: "key",
        source,
    })?;

    refuse_permissive(path, &metadata)?;

    let mut pem = Vec::new();

    file.read_to_end(&mut pem)
        .map_err(|source| TlsError::Unreadable {
            path: path.to_owned(),
            key: "key",
            source,
        })?;

    // The parse error is dropped rather than reported. Every other error in
    // this crate carries its source; this one must not, because the one
    // thing a PEM parser has to hand is the bytes it could not parse, and
    // those bytes are the private key.
    let key = PrivateKeyDer::from_pem_slice(&pem).map_err(|_| TlsError::UnusableKey {
        path: path.to_owned(),
    });

    drop(pem);

    key
}

/// Refuses a private key that anyone but its owner can read.
///
/// The bytes never reach a diagnostic, so this is what is left: a key file
/// with permissive bits has already been readable by every process on the
/// host for as long as it has existed, and a server that starts anyway is a
/// server that made that fine.
#[cfg(unix)]
fn refuse_permissive(path: &Path, metadata: &Metadata) -> Result<(), TlsError> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = metadata.permissions().mode() & 0o777;

    if mode & FORBIDDEN_BITS == 0 {
        return Ok(());
    }

    Err(TlsError::PermissiveKey {
        path: path.to_owned(),
        mode,
    })
}

/// Windows has no mode to read, and its ACLs are not a bit pattern this
/// crate can judge. Stated rather than silently skipped.
#[cfg(not(unix))]
fn refuse_permissive(_path: &Path, _metadata: &Metadata) -> Result<(), TlsError> {
    Ok(())
}

fn unreadable(path: &Path, role: Role, source: &rustls::pki_types::pem::Error) -> TlsError {
    // `pem::Error` is either an I/O error or a parse error. The first is
    // worth reporting in full — "no such file", "permission denied" — and
    // the second is not, for the same reason the key's is not: it is the
    // only variant that has seen the file's contents.
    match source {
        rustls::pki_types::pem::Error::Io(error) => TlsError::Unreadable {
            path: path.to_owned(),
            key: role.key(),
            source: std::io::Error::new(error.kind(), error.to_string()),
        },
        _ => TlsError::UnusablePem {
            path: path.to_owned(),
            key: role.key(),
        },
    }
}

/// Why TLS did not start.
///
/// **No variant carries key material, and two of them deliberately carry no
/// source either**: a PEM parse error's one useful field is the input it
/// choked on, and for `key` that input is the private key. A path, the
/// configuration key that names it, and what to do about it is the whole
/// budget.
#[derive(Debug)]
#[non_exhaustive]
pub enum TlsError {
    /// A configured file could not be opened or read.
    Unreadable {
        /// The file.
        path: PathBuf,
        /// The configuration key that named it.
        key: &'static str,
        /// The I/O error. Never a parse error — see the type's note.
        source: std::io::Error,
    },
    /// A private key's permissions let somebody other than its owner read
    /// it.
    PermissiveKey {
        /// The file.
        path: PathBuf,
        /// Its mode, masked to the permission bits.
        mode: u32,
    },
    /// A file that should hold PEM does not.
    UnusablePem {
        /// The file.
        path: PathBuf,
        /// The configuration key that named it.
        key: &'static str,
    },
    /// The key file holds no private key this build can use.
    UnusableKey {
        /// The file.
        path: PathBuf,
    },
    /// A PEM file that should hold certificates holds none.
    NoCertificates {
        /// The file.
        path: PathBuf,
        /// The configuration key that named it.
        key: &'static str,
    },
    /// `client_ca` holds nothing that can act as a trust anchor.
    UnusableClientCa {
        /// The file.
        path: PathBuf,
        /// What rustls made of it. A certificate is public, so this one may
        /// carry its source.
        source: rustls::Error,
    },
    /// The private key is not the certificate's key, or is of a type this
    /// build cannot sign with.
    KeyDoesNotMatch {
        /// The certificate.
        certificate: PathBuf,
        /// The key.
        key: PathBuf,
    },
    /// The cryptography provider would not accept the default protocol
    /// versions, which is a build problem rather than a configuration one.
    Provider,
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { path, key, source } => write!(
                f,
                "`tls.{key}` names `{}`, which cannot be read: {source}",
                path.display()
            ),
            Self::PermissiveKey { path, mode } => write!(
                f,
                "the private key `{}` is mode {mode:04o}, which lets an account other than its \
                 owner read it; a key anybody on the host can read is not a key. `chmod 600` it \
                 — or, on Kubernetes, mount the secret with `defaultMode: 0400`",
                path.display()
            ),
            Self::UnusablePem { path, key } => write!(
                f,
                "`tls.{key}` names `{}`, which is not PEM this build can parse",
                path.display()
            ),
            Self::UnusableKey { path } => write!(
                f,
                "`tls.key` names `{}`, which holds no PKCS#8, PKCS#1 or SEC1 private key. The \
                 file's contents are deliberately not quoted here",
                path.display()
            ),
            Self::NoCertificates { path, key } => write!(
                f,
                "`tls.{key}` names `{}`, which contains no certificate",
                path.display()
            ),
            Self::UnusableClientCa { path, source } => write!(
                f,
                "`tls.client_ca` names `{}`, which cannot be a trust anchor: {source}",
                path.display()
            ),
            Self::KeyDoesNotMatch { certificate, key } => write!(
                f,
                "the private key `{}` is not the key of the certificate `{}`, or is of a type \
                 this build cannot sign with",
                key.display(),
                certificate.display()
            ),
            Self::Provider => f.write_str(
                "the `ring` cryptography provider does not support rustls's default protocol \
                 versions, which is a build problem rather than a configuration one",
            ),
        }
    }
}

impl std::error::Error for TlsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            Self::UnusableClientCa { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key whose bytes are distinctive enough that finding them anywhere is
    /// unambiguous. Not a real key — nothing here parses it.
    const PLANTED: &str =
        "-----BEGIN PRIVATE KEY-----\nPLANTED-KEY-MATERIAL\n-----END PRIVATE KEY-----\n";

    fn written(name: &str, contents: &str, mode: u32) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join(name);

        std::fs::write(&path, contents).expect("writable");
        chmod(&path, mode);

        (directory, path)
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    #[cfg(not(unix))]
    fn chmod(_path: &Path, _mode: u32) {}

    /// The refusal this crate adds for the same reason it refuses a
    /// four-character token, and with the same shape: it names the fix.
    #[cfg(unix)]
    #[test]
    fn a_key_anybody_can_read_is_refused_and_the_message_names_the_fix() {
        let (_directory, path) = written("key.pem", PLANTED, 0o644);
        let error = read_private_key(&path).expect_err("mode 644 is not a secret");

        assert!(
            matches!(error, TlsError::PermissiveKey { mode: 0o644, .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("chmod 600"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_key_only_its_owner_can_read_passes_the_permission_check() {
        // 0600 gets past the permission check and fails at the parse, which
        // is the next check rather than this one.
        let (_directory, path) = written("key.pem", PLANTED, 0o600);
        let error = read_private_key(&path).expect_err("the planted key is not a key");

        assert!(matches!(error, TlsError::UnusableKey { .. }), "{error:?}");
    }

    /// The rule this module exists to keep. Every error a key file can
    /// produce, rendered both ways, and none of them holds the key.
    #[test]
    fn no_error_about_a_key_file_carries_the_key() {
        let (_directory, path) = written("key.pem", PLANTED, 0o644);
        let missing = read_private_key(Path::new("/nonexistent/key.pem")).unwrap_err();
        let permissive = read_private_key(&path).unwrap_err();

        chmod(&path, 0o600);

        let unusable = read_private_key(&path).unwrap_err();

        for error in [missing, permissive, unusable] {
            let rendered = format!("{error} / {error:?}");

            assert!(
                !rendered.contains("PLANTED-KEY-MATERIAL"),
                "a private key escaped through an error: {rendered}"
            );
        }
    }

    #[test]
    fn a_certificate_file_that_is_not_pem_is_refused_without_being_quoted() {
        let (_directory, path) = written("cert.pem", "not a certificate: hunter2\n", 0o644);
        let error = read_certificates(&path, Role::Certificate).unwrap_err();
        let rendered = format!("{error} / {error:?}");

        assert!(rendered.contains("tls.certificate"), "{rendered}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
    }
}
