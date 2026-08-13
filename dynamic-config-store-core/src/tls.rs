//! One TLS vocabulary for the seven store crates.
//!
//! Every store here already had a door to TLS, and every one of them opened
//! onto a different type: `ConnectOptions` for etcd, a `ureq::Agent` for
//! Vault, Consul and Firestore, an `SdkConfig` for S3, nothing at all for
//! Redis. That is the right door for options nobody anticipated — it still
//! is, and none of it was removed — but it has two costs the repository
//! owner asked to fix. A deployment behind a private CA could not reach
//! four of the seven at all, and *nothing* here could ever cross into the
//! Python wheels, because there is no Python spelling for a `tonic` TLS
//! configuration or a `ureq` agent.
//!
//! So this module holds data and nothing else: paths and PEM bytes, no
//! client type anywhere in a signature. A caller says what it has, and each
//! store translates that into whatever its own client understands.
//!
//! ```
//! # use dynamic_config_store_core::tls::TlsConfig;
//! let tls = TlsConfig::new()
//!     .with_ca_certificate_file("/etc/ssl/private-ca.pem")
//!     .with_client_certificate_files("/etc/ssl/app.crt", "/etc/ssl/app.key");
//! ```
//!
//! # Not every store can express all of it
//!
//! The clients differ, and where one cannot express a setting **the store
//! refuses the whole configuration and says which setting and why**. A
//! silently ignored `ca_certificate` is a program that believes it is
//! pinned to a private CA and is not, which is worse than a program that
//! will not start.
//!
//! | Store | CA from a file | CA from bytes | Client certificate |
//! |---|---|---|---|
//! | etcd, Consul, Vault, Firestore, Redis, git | yes | yes | yes |
//! | NATS | yes | **no** — its client takes paths | file paths only |
//! | S3 | yes | yes | **no** — the SDK's TLS context has no client-certificate slot |
//!
//! git is in the table but not in this crate's dependents: `dynamic-config-git`
//! re-exports [`TlsConfig`] and speaks the same vocabulary over `https://`
//! only, because an `ssh://` remote's trust is `known_hosts` and its identity
//! is a key rather than a certificate.
//!
//! Each store's own documentation repeats its row, because that is where
//! somebody reads it.
//!
//! # There is no `skip_verification`
//!
//! Deliberately, and the reasoning is worth stating rather than leaving as
//! an absence.
//!
//! **It could not be uniform.** `tonic` offers no such switch, and neither
//! does the AWS SDK's TLS context; `async-nats` reaches it only through a
//! hand-built `rustls::ClientConfig`. A knob in this type that four of
//! seven stores had to refuse would be a vocabulary word that mostly means
//! "error", which is the opposite of what one vocabulary is for.
//!
//! **It answers nothing [`with_ca_certificate_file`](TlsConfig::with_ca_certificate_file)
//! does not.** The two situations people reach for it in — a development
//! server with a self-signed certificate, an enterprise private CA — are
//! both a matter of trusting one more certificate, which is one line here
//! and keeps the server authenticated. Turning verification off does not
//! make TLS weaker in the way a checklist means; it makes it *absent*,
//! and leaves a connection that any party on the path can read and rewrite.
//!
//! **The escape hatch is still there for the case nobody anticipated.**
//! `with_agent`, `with_options`, `from_client` and `with_config` all
//! survive, and every client underneath has its own dangerous switch under
//! its own frightening name. A caller who genuinely needs it names that
//! API, in their own code, where a reviewer sees it — rather than reaching
//! for a short word on a type whose other options are safe.

use std::path::{Path, PathBuf};

use dynamic_config::Error;

/// A PEM document: a file to read at connect time, or bytes already in hand.
///
/// Both spellings exist because both deployments do. A file is what a
/// Kubernetes secret mount or a `/etc/ssl` layout produces; bytes are what a
/// program that already fetched its material from a secrets manager has,
/// and writing those to a temporary file so a client could read them back
/// would put a private key on a disk that never asked for one.
///
/// Never `Debug`-derived: a [`Pem::Bytes`] may be a private key.
#[derive(Clone, PartialEq, Eq)]
pub enum Pem {
    /// A path, read when the store builds its client — not when this is
    /// constructed, so a missing file is an error from the store that names
    /// it rather than a panic from a builder.
    File(PathBuf),
    /// PEM bytes.
    Bytes(Vec<u8>),
}

impl Pem {
    /// The path, for the one client in this family that takes paths rather
    /// than bytes.
    ///
    /// `None` for [`Pem::Bytes`], which is how [`Nats`] knows it has been
    /// handed something it cannot pass on.
    ///
    /// [`Nats`]: https://docs.rs/dynamic-config-nats
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::Bytes(_) => None,
        }
    }

    /// The PEM bytes, reading the file if that is what this is.
    ///
    /// `what` names the material — `"the CA certificate"` — and `described`
    /// is the store's own `describe()`, so a failure says which source and
    /// which of its three files.
    ///
    /// # Errors
    ///
    /// If the file cannot be read. The message carries the path and the
    /// operating system's reason and **never the contents**: this is the
    /// call that reads private keys.
    pub fn read(&self, described: &str, what: &str) -> Result<Vec<u8>, Error> {
        match self {
            Self::Bytes(bytes) => Ok(bytes.clone()),
            Self::File(path) => std::fs::read(path).map_err(|error| {
                Error::remote(format!(
                    "{described}: reading {what} from {}: {error}",
                    path.display()
                ))
            }),
        }
    }
}

// Hand-written, never derived. `Pem::Bytes` is the shape a private key
// arrives in, and a derive would print it — into a `dbg!`, into a
// `tracing::debug!(?tls)`, into whatever caught the panic. Shape only: what
// a debugger needs to tell "the file is wrong" from "the bytes are empty",
// and nothing a key could hide in.
impl std::fmt::Debug for Pem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => write!(f, "file {}", path.display()),
            Self::Bytes(_) => f.write_str("<pem bytes>"),
        }
    }
}

/// A client certificate's PEM bytes and its private key's, in that order.
///
/// Named only so the pair reads as one thing at a call site — the two halves
/// travel together everywhere, because presenting either alone is not mTLS.
pub type CertificateAndKey = (Vec<u8>, Vec<u8>);

/// A client certificate and the private key that goes with it.
///
/// The pair is one thing because presenting either half alone is not mTLS,
/// it is a misconfiguration — so there is no way to set one and forget the
/// other.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientCertificate {
    certificate: Pem,
    key: Pem,
}

impl ClientCertificate {
    /// The certificate, or the chain ending in it.
    #[must_use]
    pub fn certificate(&self) -> &Pem {
        &self.certificate
    }

    /// The private key.
    #[must_use]
    pub fn key(&self) -> &Pem {
        &self.key
    }
}

// The private key half is never rendered, whatever it holds: a path is a
// diagnostic and its bytes are the sharpest secret in this module.
impl std::fmt::Debug for ClientCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCertificate")
            .field("certificate", &self.certificate)
            .field(
                "key",
                &match &self.key {
                    // A path names which key, which is the question a
                    // debugger is actually asking, and is not itself secret.
                    Pem::File(path) => format!("file {}", path.display()),
                    Pem::Bytes(_) => "<redacted>".to_owned(),
                },
            )
            .finish()
    }
}

/// What a store needs to speak TLS to somewhere this machine does not
/// already trust.
///
/// Data only — no client type appears anywhere in it, which is what makes
/// it the same three settings in all seven crates and what makes it
/// expressible from a language that has never heard of `tonic`.
///
/// Empty by default, and an empty one is not "no TLS": it is the platform's
/// own trust store, which is what a public certificate authority needs and
/// what every store already did.
///
/// See the [module documentation](self) for what each store can express and
/// for why there is no way to turn verification off.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct TlsConfig {
    ca: Option<Pem>,
    client: Option<ClientCertificate>,
}

impl TlsConfig {
    /// An empty configuration: the platform's trust store, no client
    /// certificate.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Trust the certificate authority in this PEM file.
    ///
    /// The file may hold several certificates; all of them are trusted,
    /// which is what a private CA with an intermediate needs.
    ///
    /// The file is read when the store builds its client, not here — so a
    /// rotated CA is picked up by rebuilding the source, and a missing file
    /// is an error naming it rather than a panic in a builder chain.
    #[must_use]
    pub fn with_ca_certificate_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.ca = Some(Pem::File(path.into()));
        self
    }

    /// Trust the certificate authority in these PEM bytes.
    ///
    /// For a program that already has the material — from a secrets
    /// manager, from its own configuration — and should not have to put it
    /// on a disk for a client to read back.
    #[must_use]
    pub fn with_ca_certificate_pem(mut self, pem: impl Into<Vec<u8>>) -> Self {
        self.ca = Some(Pem::Bytes(pem.into()));
        self
    }

    /// Present this client certificate and private key (mTLS).
    ///
    /// Both are PEM files. `certificate` may be a chain; the leaf comes
    /// first, as every TLS stack here expects.
    #[must_use]
    pub fn with_client_certificate_files(
        mut self,
        certificate: impl Into<PathBuf>,
        key: impl Into<PathBuf>,
    ) -> Self {
        self.client = Some(ClientCertificate {
            certificate: Pem::File(certificate.into()),
            key: Pem::File(key.into()),
        });
        self
    }

    /// Present this client certificate and private key (mTLS), from bytes.
    ///
    /// The private key is the sharpest secret this crate handles. It is
    /// never rendered by [`Debug`](std::fmt::Debug), never quoted into an
    /// error, and never written anywhere: it goes from here into the
    /// client's own key type and stops.
    #[must_use]
    pub fn with_client_certificate_pem(
        mut self,
        certificate: impl Into<Vec<u8>>,
        key: impl Into<Vec<u8>>,
    ) -> Self {
        self.client = Some(ClientCertificate {
            certificate: Pem::Bytes(certificate.into()),
            key: Pem::Bytes(key.into()),
        });
        self
    }

    /// Whether this asks for nothing at all.
    ///
    /// A store uses it to tell "the caller wants the platform defaults"
    /// from "the caller wants something", which is the difference between
    /// leaving its client alone and building one.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ca.is_none() && self.client.is_none()
    }

    /// The certificate authority to trust, if one was named.
    #[must_use]
    pub fn ca_certificate(&self) -> Option<&Pem> {
        self.ca.as_ref()
    }

    /// The client certificate to present, if one was named.
    #[must_use]
    pub fn client_certificate(&self) -> Option<&ClientCertificate> {
        self.client.as_ref()
    }

    /// The CA certificate's PEM bytes, reading the file if that is what it
    /// is.
    ///
    /// # Errors
    ///
    /// If the file cannot be read; the message names the path.
    pub fn ca_certificate_pem(&self, described: &str) -> Result<Option<Vec<u8>>, Error> {
        self.ca
            .as_ref()
            .map(|pem| pem.read(described, "the CA certificate"))
            .transpose()
    }

    /// The client certificate and key as PEM bytes, reading the files if
    /// that is what they are.
    ///
    /// # Errors
    ///
    /// If either file cannot be read; the message names the path and never
    /// the contents.
    pub fn client_certificate_pem(
        &self,
        described: &str,
    ) -> Result<Option<CertificateAndKey>, Error> {
        let Some(client) = &self.client else {
            return Ok(None);
        };

        let certificate = client
            .certificate
            .read(described, "the client certificate")?;
        let key = client.key.read(described, "the client private key")?;

        Ok(Some((certificate, key)))
    }
}

// Hand-written for the reason every store's `Debug` is: this type's fields
// include a private key.
impl std::fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsConfig")
            .field("ca_certificate", &self.ca)
            .field("client_certificate", &self.client)
            .finish()
    }
}

/// The refusal a store returns for part of a [`TlsConfig`] its client
/// cannot express.
///
/// One wording for all seven, because the important half is the same
/// everywhere: what was asked for, that it was *not* applied, and what to
/// use instead. A store that quietly dropped the setting would leave a
/// program believing it had pinned a private CA when it had not.
///
/// `described` is the store's `describe()`, `setting` names the call —
/// `"a CA certificate from PEM bytes"` — and `instead` is the way out.
#[must_use]
pub fn unsupported(described: &str, setting: &str, instead: &str) -> Error {
    Error::remote(format!(
        "{described}: {setting} cannot be expressed here, and is refused \
         rather than ignored; {instead}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key material every test in this module plants. If this string
    /// ever appears in a rendering, something printed a private key.
    const PLANTED: &str = "PLANTED-PRIVATE-KEY-MATERIAL";

    fn planted_key() -> String {
        format!("-----BEGIN PRIVATE KEY-----\n{PLANTED}\n-----END PRIVATE KEY-----\n")
    }

    #[test]
    fn a_planted_private_key_never_reaches_debug() {
        let tls = TlsConfig::new()
            .with_ca_certificate_pem("-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----")
            .with_client_certificate_pem("cert", planted_key());

        let rendered = format!("{tls:?}");

        assert!(
            !rendered.contains(PLANTED),
            "the private key reached `Debug`: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "the key should be visibly withheld rather than absent: {rendered}"
        );
    }

    #[test]
    fn debug_prints_shape_and_never_material() {
        let files = TlsConfig::new()
            .with_ca_certificate_file("/etc/ssl/private-ca.pem")
            .with_client_certificate_files("/etc/ssl/app.crt", "/etc/ssl/app.key");

        let rendered = format!("{files:?}");

        // A path is a diagnostic, not a secret, and it is the question a
        // debugger is actually asking.
        assert!(rendered.contains("/etc/ssl/private-ca.pem"), "{rendered}");
        assert!(rendered.contains("/etc/ssl/app.key"), "{rendered}");

        let bytes = TlsConfig::new().with_ca_certificate_pem("cert-material-here");
        let rendered = format!("{bytes:?}");

        assert!(rendered.contains("<pem bytes>"), "{rendered}");
        assert!(
            !rendered.contains("cert-material-here"),
            "even a certificate's bytes are noise in a log: {rendered}"
        );
    }

    #[test]
    fn a_planted_private_key_never_reaches_a_read_error() {
        // A directory, so the read fails with the key still in hand: the
        // error must name the path and nothing else.
        let directory = std::env::temp_dir();
        let tls = TlsConfig::new().with_client_certificate_files(&directory, &directory);

        let error = tls
            .client_certificate_pem("vault https://vault.internal path myapp/db")
            .expect_err("a directory is not a PEM file");

        let rendered = error.to_string();

        assert!(!rendered.contains(PLANTED), "{rendered}");
        assert!(rendered.contains("the client certificate"), "{rendered}");
        assert!(
            rendered.contains("vault https://vault.internal"),
            "{rendered}"
        );
    }

    #[test]
    fn a_read_error_names_the_material_that_failed() {
        let tls = TlsConfig::new().with_ca_certificate_file("/nonexistent/private-ca.pem");

        let error = tls
            .ca_certificate_pem("consul http://consul:8500 key myapp/db.json")
            .expect_err("the file is not there");

        assert!(error.to_string().contains("the CA certificate"), "{error}");
        assert!(
            error.to_string().contains("/nonexistent/private-ca.pem"),
            "{error}"
        );
    }

    /// A store URL may embed `user:password@host`, and a `describe()` that
    /// carries one must reach this module already redacted — this pins that
    /// nothing here *un*-redacts it by, say, quoting a raw address instead.
    #[test]
    fn a_refusal_carries_the_description_it_was_given_and_adds_nothing() {
        let error = unsupported(
            "nats nats://***@nats.internal:4222 key db.json",
            "a CA certificate from PEM bytes",
            "name a file with `with_ca_certificate_file`",
        );

        let rendered = error.to_string();

        assert!(
            rendered.contains("nats://***@nats.internal:4222"),
            "{rendered}"
        );
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(
            rendered.contains("refused rather than ignored"),
            "{rendered}"
        );
    }

    #[test]
    fn bytes_and_a_file_resolve_to_the_same_material() {
        let directory = std::env::temp_dir().join("dynamic-config-store-core-tls-test");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("ca.pem");
        std::fs::write(&path, b"-----BEGIN CERTIFICATE-----\nca\n").unwrap();

        let from_file = TlsConfig::new().with_ca_certificate_file(&path);
        let from_bytes =
            TlsConfig::new().with_ca_certificate_pem(&b"-----BEGIN CERTIFICATE-----\nca\n"[..]);

        assert_eq!(
            from_file.ca_certificate_pem("store").unwrap(),
            from_bytes.ca_certificate_pem("store").unwrap()
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_configuration_is_the_platform_trust_store() {
        assert!(TlsConfig::new().is_empty());
        assert!(!TlsConfig::new().with_ca_certificate_pem("x").is_empty());
        assert!(!TlsConfig::new()
            .with_client_certificate_pem("c", "k")
            .is_empty());
    }

    #[test]
    fn a_path_is_offered_only_by_the_file_spelling() {
        let files = TlsConfig::new().with_ca_certificate_file("/etc/ssl/ca.pem");
        assert_eq!(
            files.ca_certificate().and_then(Pem::path),
            Some(Path::new("/etc/ssl/ca.pem"))
        );

        let bytes = TlsConfig::new().with_ca_certificate_pem("x");
        assert_eq!(bytes.ca_certificate().and_then(Pem::path), None);
    }
}
