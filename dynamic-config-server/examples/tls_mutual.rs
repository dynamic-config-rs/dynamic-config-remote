//! A config server over TLS, demanding a client certificate, end to end.
//!
//! ```text
//! cargo run -p dynamic-config-server --features tls --example tls_mutual
//! ```
//!
//! It needs no network, no `openssl` binary and no files of your own: it
//! generates a certificate authority, a server certificate and a client
//! certificate into a temporary directory, writes the `server.toml` that
//! names them, starts the server on a loopback port, and then makes three
//! requests that are the whole point of the feature:
//!
//! 1. a client presenting its certificate **and** its bearer token — served;
//! 2. the same client presenting its certificate and **no** token — 401,
//!    because a certificate is a gate and not a credential;
//! 3. a client presenting **no** certificate — the handshake never
//!    completes, so there is no request to answer.
//!
//! In a real deployment the three PEM files come from whatever issues
//! certificates for the fleet — a private CA, cert-manager, SPIFFE — and the
//! only thing this crate cares about is that the client certificates chain
//! to the CA named in `client_ca`.

use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dynamic_config::Builder;
use dynamic_config_server::{router, serve_tls, Server, ServerConfig};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// The token the served client presents. Thirty-two characters, which is the
/// shortest this server accepts.
const TOKEN: &str = "example-token-0123456789abcdefg0";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let files = Files::new(directory.path())?;

    println!(
        "certificates and configuration in {}\n",
        files.root.display()
    );
    println!(
        "--- server.toml ---\n{}",
        std::fs::read_to_string(&files.server_toml)?
    );

    // Read exactly the way the binary reads it: this is the file an operator
    // writes, not a struct an example built.
    let config: ServerConfig = Builder::new("server")
        .file(files.server_toml.to_str().unwrap())
        .load()?;
    let server = Arc::new(Server::start(&config)?);

    // Port zero, so the example can run twice at once and a busy port is
    // never its failure.
    let listener = tokio::net::TcpListener::bind(server.address()).await?;
    let address = listener.local_addr()?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();

    println!(
        "listening on {address} (protection: {})\n",
        server.posture()
    );

    // The task owns its own handle on the server: `serve_tls` borrows one,
    // and a spawned task cannot borrow from its spawner.
    let held = Arc::clone(&server);
    let serving = tokio::spawn(async move {
        serve_tls(listener, router(Arc::clone(&held)), &held, async {
            let _ = stopped.await;
        })
        .await
    });

    // 1. The certificate and the token. Both are required and both are here.
    let served = request(address, &files, true, Some(TOKEN)).await?;
    println!("with a certificate and a token:\n  {served}\n");

    // 2. The certificate alone. A certificate says the connection came from
    //    a machine the deployment provisioned; it does not say who is
    //    calling or what they may read, so this is a 401 exactly as it would
    //    be over plain HTTP.
    let refused = request(address, &files, true, None).await?;
    println!("with a certificate and no token:\n  {refused}\n");

    // 3. No certificate. There is no HTTP status for this, because there is
    //    no HTTP: the handshake ends before a request exists, and the audit
    //    log records `endpoint=tls outcome=unauthenticated`.
    match request(address, &files, false, Some(TOKEN)).await {
        Ok(response) => println!("without a certificate: {response} — expected a refusal!"),
        Err(error) => println!("without a certificate:\n  refused at the handshake: {error}"),
    }

    let _ = stop.send(());
    let _ = serving.await?;

    Ok(())
}

/// One request over TLS, written by hand because the interesting part is the
/// handshake rather than the HTTP.
async fn request(
    address: SocketAddr,
    files: &Files,
    certificate: bool,
    token: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let mut roots = RootCertStore::empty();

    roots.add(CertificateDer::from_pem_file(&files.ca)?)?;

    let builder =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots);

    let config = if certificate {
        builder.with_client_auth_cert(
            CertificateDer::pem_file_iter(&files.client_certificate)?
                .collect::<Result<Vec<_>, _>>()?,
            PrivateKeyDer::from_pem_file(&files.client_key)?,
        )?
    } else {
        builder.with_no_client_auth()
    };

    let stream = TcpStream::connect(address).await?;
    let name = ServerName::from(address.ip());
    let mut stream = TlsConnector::from(Arc::new(config))
        .connect(name, stream)
        .await?;

    let authorization = token.map_or_else(String::new, |token| {
        format!("Authorization: Bearer {token}\r\n")
    });

    stream
        .write_all(
            format!(
                "GET /billing/prod HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\
                 {authorization}\r\n"
            )
            .as_bytes(),
        )
        .await?;

    let mut response = String::new();

    // A client that presented no certificate reaches here and *then* learns
    // the server rejected it: under TLS 1.3 the server's alert arrives after
    // the client believes the handshake is done.
    stream
        .read_to_string(&mut response)
        .await
        .map_err(|error| io::Error::new(error.kind(), format!("the connection ended: {error}")))?;

    Ok(response.lines().next().unwrap_or_default().to_owned())
}

/// Everything written to disk: three PEM pairs, a served document, and the
/// `server.toml` that names them.
struct Files {
    root: PathBuf,
    ca: PathBuf,
    client_certificate: PathBuf,
    client_key: PathBuf,
    server_toml: PathBuf,
}

impl Files {
    fn new(root: &Path) -> Result<Self, Box<dyn Error>> {
        // The authority. In a deployment this is the thing that already
        // exists and whose public certificate is all this server needs.
        let mut authority = CertificateParams::new(Vec::new())?;

        authority.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        authority
            .distinguished_name
            .push(DnType::CommonName, "dynamic-config example CA");
        authority.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

        let authority_key = KeyPair::generate()?;
        let ca = authority.self_signed(&authority_key)?;
        let issuer = Issuer::new(authority, authority_key);

        // The server's own certificate, valid for the address it binds.
        let server = leaf(
            vec!["127.0.0.1".to_owned(), "localhost".to_owned()],
            "config-server",
            ExtendedKeyUsagePurpose::ServerAuth,
            &issuer,
        )?;

        // And the client's. Its subject is *not* used for anything: this
        // server names its callers by their bearer token, and a certificate
        // subject is issued by whoever holds the CA key rather than by
        // whoever maintains the roster.
        let client = leaf(
            vec!["billing-pod".to_owned()],
            "billing-pod",
            ExtendedKeyUsagePurpose::ClientAuth,
            &issuer,
        )?;

        let files = Self {
            root: root.to_owned(),
            ca: root.join("ca.pem"),
            client_certificate: root.join("client.pem"),
            client_key: root.join("client.key"),
            server_toml: root.join("server.toml"),
        };

        write(&files.ca, &ca.pem(), 0o644)?;
        write(&root.join("server.pem"), &server.0, 0o644)?;
        // 0600, and not by convention: the server refuses to start if
        // anything but its owner can read this file.
        write(&root.join("server.key"), &server.1, 0o600)?;
        write(&files.client_certificate, &client.0, 0o644)?;
        write(&files.client_key, &client.1, 0o600)?;
        write(
            &root.join("billing.toml"),
            "[billing]\nhost = 'db.internal'\npassword = 'hunter2'\n",
            0o600,
        )?;

        write(
            &files.server_toml,
            &format!(
                r#"[server]
bind = "127.0.0.1:0"

[server.tls]
certificate = "{root}/server.pem"
key = "{root}/server.key"
# Present, so every caller must present a certificate chaining to it.
# Remove this one line and the server keeps TLS and stops asking.
client_ca = "{root}/ca.pem"

[[server.sections]]
application = "billing"
profile = "prod"
files = ["{root}/billing.toml"]

[[server.clients]]
name = "billing-pod"
token = "{TOKEN}"
applications = ["billing"]
"#,
                // Forward slashes, which every platform's cargo and this
                // server's own loader accept: a Windows path spelled with
                // backslashes lands in a TOML *basic* string, where `\a` is
                // an escape sequence and the manifest will not parse.
                root = root.display().to_string().replace('\\', "/")
            ),
            0o600,
        )?;

        Ok(files)
    }
}

/// A leaf certificate and its key, both PEM.
fn leaf(
    names: Vec<String>,
    common_name: &str,
    usage: ExtendedKeyUsagePurpose,
    issuer: &Issuer<'_, KeyPair>,
) -> Result<(String, String), Box<dyn Error>> {
    let mut params = CertificateParams::new(names)?;

    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    params.extended_key_usages = vec![usage];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];

    let key = KeyPair::generate()?;
    let certificate = params.signed_by(&key, issuer)?;

    Ok((certificate.pem(), key.serialize_pem()))
}

fn write(path: &Path, contents: &str, mode: u32) -> io::Result<()> {
    std::fs::write(path, contents)?;
    chmod(path, mode)
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn chmod(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}
