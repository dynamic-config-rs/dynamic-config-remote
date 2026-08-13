//! Shared fixture: a real server over real files, driven through the real
//! router.
//!
//! Every test builds its own temporary directory and its own sections. The
//! mistake AGENTS.md records is tests that share state and pass alone; a
//! served section is a `Dynamic`, whose storage is per instance, so the only
//! thing left to keep apart is the files — one directory per test does it.

#![allow(dead_code)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use dynamic_config_server::{
    router, AuditEntry, AuditSink, ClientConfig, SectionConfig, Server, ServerConfig, Token,
};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Thirty-two characters, which is the shortest this server accepts.
pub const BILLING_TOKEN: &str = "billing-token-0123456789abcdefg0";
pub const PAYROLL_TOKEN: &str = "payroll-token-0123456789abcdefg0";

/// The audit log, in a `Vec` a test can read.
#[derive(Clone, Default)]
pub struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.0.lock().expect("no test panics holding this").clone()
    }

    #[must_use]
    pub fn joined(&self) -> String {
        self.lines().join("\n")
    }
}

impl AuditSink for Log {
    fn record(&self, entry: &AuditEntry) {
        self.0
            .lock()
            .expect("no test panics holding this")
            .push(entry.to_string());
    }
}

/// A started server, its router, and the log it writes to.
pub struct Fixture {
    pub directory: tempfile::TempDir,
    pub server: Arc<Server>,
    pub router: Router,
    pub log: Log,
}

impl Fixture {
    /// A `GET`, optionally with a bearer token, through the real router.
    pub async fn get(&self, uri: &str, token: Option<&str>) -> (StatusCode, String) {
        let mut request = Request::builder().method("GET").uri(uri);

        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }

        self.send(request.body(Body::empty()).expect("a well-formed request"))
            .await
    }

    /// A `GET`, with the response's headers kept — for the endpoints whose
    /// content type is part of what they promise.
    pub async fn get_with_headers(
        &self,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut request = Request::builder().method("GET").uri(uri);

        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }

        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::empty()).expect("a well-formed request"))
            .await
            .expect("the router is infallible");
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("a complete body")
            .to_bytes();

        (
            status,
            headers,
            String::from_utf8(body.to_vec()).expect("every body here is text"),
        )
    }

    /// Any request at all, for the tests about verbs and headers.
    pub async fn send(&self, request: Request<Body>) -> (StatusCode, String) {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("the router is infallible");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("a complete body")
            .to_bytes();

        (
            status,
            String::from_utf8(body.to_vec()).expect("every body here is text"),
        )
    }

    /// A response body as JSON.
    pub async fn json(&self, uri: &str, token: Option<&str>) -> (StatusCode, serde_json::Value) {
        let (status, body) = self.get(uri, token).await;
        let json = serde_json::from_str(&body).unwrap_or_else(|error| {
            panic!("`{uri}` answered something that is not JSON: {error}: {body}")
        });

        (status, json)
    }

    pub fn path(&self, name: &str) -> String {
        self.directory.path().join(name).display().to_string()
    }

    /// The audit log, one entry per line.
    #[must_use]
    pub fn log_lines(&self) -> Vec<String> {
        self.log.lines()
    }

    /// The audit log as one string, for a `contains` assertion.
    #[must_use]
    pub fn joined_log(&self) -> String {
        self.log.joined()
    }

    /// Rewrites one of the fixture's files.
    pub fn write(&self, name: &str, contents: &str) {
        write(self.directory.path(), name, contents);
    }
}

fn write(directory: &Path, name: &str, contents: &str) -> String {
    let path = directory.join(name);
    std::fs::write(&path, contents).expect("the temporary directory is writable");

    path.display().to_string()
}

/// Certificates, generated per test and never checked in.
///
/// A repository with a private key in it has a private key in it, whatever
/// the README says about the key being a test one — and a checked-in
/// certificate expires, which turns a security test into a flake with a
/// date on it. So every TLS test makes its own, in memory, in a few
/// milliseconds.
#[cfg(feature = "tls")]
pub mod certificates {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
    use rustls::{ClientConfig, RootCertStore};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// One certificate authority, and the leaves it has signed.
    pub struct Authority {
        pub certificate: PathBuf,
        issuer: Issuer<'static, KeyPair>,
    }

    /// A certificate and the key that goes with it, on disk.
    pub struct Pair {
        pub certificate: PathBuf,
        pub key: PathBuf,
    }

    impl Authority {
        /// A self-signed CA, written to `directory` as `<name>-ca.pem`.
        #[must_use]
        pub fn new(directory: &Path, name: &str) -> Self {
            let mut params = CertificateParams::new(Vec::new()).expect("no SANs is a valid CA");

            params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
            params.distinguished_name.push(DnType::CommonName, name);
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

            let key = KeyPair::generate().expect("a key pair");
            let certificate = params.self_signed(&key).expect("a self-signed CA");
            let path = directory.join(format!("{name}-ca.pem"));

            write(&path, &certificate.pem(), 0o644);

            Self {
                certificate: path,
                issuer: Issuer::new(params, key),
            }
        }

        /// A leaf signed by this authority. `mode` is the key file's, which
        /// the server has an opinion about.
        #[must_use]
        pub fn sign(&self, directory: &Path, name: &str, server: bool, mode: u32) -> Pair {
            let names = if server {
                vec!["127.0.0.1".to_owned(), "localhost".to_owned()]
            } else {
                vec![name.to_owned()]
            };
            let mut params = CertificateParams::new(names).expect("valid names");

            params.distinguished_name.push(DnType::CommonName, name);
            params.extended_key_usages = vec![if server {
                ExtendedKeyUsagePurpose::ServerAuth
            } else {
                ExtendedKeyUsagePurpose::ClientAuth
            }];
            params.key_usages = vec![KeyUsagePurpose::DigitalSignature];

            let key = KeyPair::generate().expect("a key pair");
            let certificate = params.signed_by(&key, &self.issuer).expect("a signature");
            let pair = Pair {
                certificate: directory.join(format!("{name}.pem")),
                key: directory.join(format!("{name}.key")),
            };

            write(&pair.certificate, &certificate.pem(), 0o644);
            write(&pair.key, &key.serialize_pem(), mode);

            pair
        }
    }

    /// A rustls client, trusting `authority` and optionally presenting
    /// `certificate`.
    #[must_use]
    pub fn client(authority: &Path, certificate: Option<&Pair>) -> ClientConfig {
        let mut roots = RootCertStore::empty();

        roots
            .add(CertificateDer::from_pem_file(authority).expect("a PEM authority"))
            .expect("a usable authority");

        let builder =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("ring supports the defaults")
                .with_root_certificates(roots);

        match certificate {
            Some(pair) => builder
                .with_client_auth_cert(
                    CertificateDer::pem_file_iter(&pair.certificate)
                        .expect("a PEM certificate")
                        .collect::<Result<Vec<_>, _>>()
                        .expect("a PEM certificate"),
                    PrivateKeyDer::from_pem_file(&pair.key).expect("a PEM key"),
                )
                .expect("the certificate and key agree"),
            None => builder.with_no_client_auth(),
        }
    }

    /// One `GET` over TLS, written by hand: what is being tested is the
    /// handshake, and a client library between the assertion and the thing
    /// asserted is one more thing to explain when it fails.
    ///
    /// The whole response, headers included, or the error that ended the
    /// connection. Under TLS 1.3 a client that presented no certificate
    /// learns of the refusal here rather than at the handshake, because the
    /// server's alert arrives after the client thinks it has finished.
    pub async fn get(
        address: std::net::SocketAddr,
        config: ClientConfig,
        path: &str,
        token: Option<&str>,
    ) -> Result<String, String> {
        let stream = tokio::net::TcpStream::connect(address)
            .await
            .map_err(|error| error.to_string())?;
        let mut stream = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(ServerName::from(address.ip()), stream)
            .await
            .map_err(|error| format!("handshake: {error}"))?;

        let authorization = token.map_or_else(String::new, |token| {
            format!("Authorization: Bearer {token}\r\n")
        });

        stream
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
                     {authorization}\r\n"
                )
                .as_bytes(),
            )
            .await
            .map_err(|error| format!("write: {error}"))?;

        let mut response = String::new();

        stream
            .read_to_string(&mut response)
            .await
            .map_err(|error| format!("read: {error}"))?;

        Ok(response)
    }

    fn write(path: &Path, contents: &str, mode: u32) {
        std::fs::write(path, contents).expect("the temporary directory is writable");
        chmod(path, mode);
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    #[cfg(not(unix))]
    fn chmod(_path: &Path, _mode: u32) {}
}

/// A section, given the file it reads.
#[must_use]
pub fn section(application: &str, profile: &str, file: String) -> SectionConfig {
    SectionConfig {
        application: application.to_owned(),
        profile: profile.to_owned(),
        files: vec![file],
        env_prefix: None,
        whole_document: false,
    }
}

/// A client with a token.
#[must_use]
pub fn client(name: &str, token: &str, applications: &[&str]) -> ClientConfig {
    ClientConfig {
        name: name.to_owned(),
        token: Some(Token::new(token)),
        applications: applications.iter().map(|it| (*it).to_owned()).collect(),
    }
}

/// A client with no token — the anonymous caller.
#[must_use]
pub fn anonymous(name: &str, applications: &[&str]) -> ClientConfig {
    ClientConfig {
        name: name.to_owned(),
        token: None,
        applications: applications.iter().map(|it| (*it).to_owned()).collect(),
    }
}

/// Starts a server over `config`, in a directory the caller has filled.
#[must_use]
pub fn start(directory: tempfile::TempDir, config: ServerConfig) -> Fixture {
    let log = Log::default();
    let server = Arc::new(
        Server::start_with(&config, log.clone()).expect("the fixture's configuration is valid"),
    );

    Fixture {
        directory,
        router: router(Arc::clone(&server)),
        server,
        log,
    }
}

/// The fixture almost every test wants: two applications, two profiles of
/// one of them, and a client that may read exactly one.
///
/// `billing` carries a planted secret. Nothing in this workspace calls a
/// value secret by declaration in a schemaless document — so the server
/// treats *every* value as one, and the assertion throughout the suite is
/// that `hunter2` reaches exactly one response body and no log line.
#[must_use]
pub fn fixture() -> Fixture {
    let directory = tempfile::tempdir().expect("a temporary directory");

    let billing = write(
        directory.path(),
        "billing.toml",
        "[billing]\nhost = 'db.internal'\npassword = 'hunter2'\n\n[billing.pool]\nmax_size = 8\n",
    );
    let billing_staging = write(
        directory.path(),
        "billing-staging.toml",
        "[billing]\nhost = 'db.staging'\npassword = 'staging-secret'\n",
    );
    let payroll = write(
        directory.path(),
        "payroll.toml",
        "[payroll]\nhost = 'payroll.internal'\n",
    );

    let config = ServerConfig {
        // Zero disables the watcher: these tests drive reloads themselves, so
        // nothing here depends on a filesystem event arriving in time.
        watch_debounce_ms: 0,
        sections: vec![
            section("billing", "prod", billing),
            section("billing", "staging", billing_staging),
            section("payroll", "prod", payroll),
        ],
        clients: vec![
            client("billing-pod", BILLING_TOKEN, &["billing"]),
            client("payroll-pod", PAYROLL_TOKEN, &["payroll"]),
        ],
        ..ServerConfig::default()
    };

    start(directory, config)
}
