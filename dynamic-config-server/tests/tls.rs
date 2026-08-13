//! TLS, mutual TLS, and the thing mutual TLS must not become.
//!
//! Every test here drives a **real handshake against a real socket**: a
//! router driven by `oneshot` cannot tell you whether a certificate was
//! demanded, and that is the whole feature. Certificates are generated per
//! test (see `common::certificates`) and never checked in.
//!
//! The suite is organised around one claim: *a client certificate is a gate
//! and not an identity*. So half of it is about handshakes, and the other
//! half is about the fact that nothing behind the handshake moved — the
//! token still decides, the grants still scope, and the 404 is still the
//! same 404.
//!
//! # What this suite does not cover
//!
//! **A handshake under load.** `HANDSHAKE_TIMEOUT` and the per-connection
//! task are asserted by construction rather than by a test that opens ten
//! thousand sockets and sends nothing on any of them — a test like that
//! measures the runner more than the server, and a flaky security test is
//! worse than an absent one. If this workspace ever grows a load harness,
//! this is the first case for it.

#![cfg(feature = "tls")]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use common::certificates::{self, Authority, Pair};
use common::{client, section, Log};
use dynamic_config_server::{router, serve_tls, Server, ServerConfig, TlsConfig};

/// A started server on an ephemeral loopback port, its certificates, and its
/// audit log.
struct Fixture {
    address: SocketAddr,
    authority: Authority,
    /// A second authority nobody configured, for the certificate that should
    /// not get in.
    stranger: Authority,
    client: Pair,
    log: Log,
    directory: tempfile::TempDir,
    serving: tokio::task::JoinHandle<std::io::Result<()>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Fixture {
    /// `mutual` configures `client_ca`; without it the server terminates TLS
    /// and asks for nothing back.
    async fn start(mutual: bool) -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let authority = Authority::new(directory.path(), "configured");
        let stranger = Authority::new(directory.path(), "stranger");
        let server_pair = authority.sign(directory.path(), "server", true, 0o600);
        let client_pair = authority.sign(directory.path(), "billing-pod", false, 0o600);

        let billing = directory.path().join("billing.toml");
        std::fs::write(
            &billing,
            "[billing]\nhost = 'db.internal'\npassword = 'hunter2'\n",
        )
        .expect("writable");
        let payroll = directory.path().join("payroll.toml");
        std::fs::write(&payroll, "[payroll]\nhost = 'payroll.internal'\n").expect("writable");

        let config = ServerConfig {
            watch_debounce_ms: 0,
            bind: "127.0.0.1:0".to_owned(),
            tls: Some(TlsConfig {
                certificate: server_pair.certificate.display().to_string(),
                key: server_pair.key.display().to_string(),
                client_ca: mutual.then(|| authority.certificate.display().to_string()),
                crl: None,
            }),
            sections: vec![
                section("billing", "prod", billing.display().to_string()),
                section("payroll", "prod", payroll.display().to_string()),
            ],
            clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
            ..ServerConfig::default()
        };

        let log = Log::default();
        let server = Arc::new(
            Server::start_with(&config, log.clone()).expect("the fixture's configuration is valid"),
        );
        let listener = tokio::net::TcpListener::bind(server.address())
            .await
            .expect("loopback, port zero");
        let address = listener.local_addr().expect("a bound listener has one");
        let (stop, stopped) = tokio::sync::oneshot::channel();

        let held = Arc::clone(&server);
        let serving = tokio::spawn(async move {
            serve_tls(listener, router(Arc::clone(&held)), &held, async {
                let _ = stopped.await;
            })
            .await
        });

        Self {
            address,
            authority,
            stranger,
            client: client_pair,
            log,
            directory,
            serving,
            stop: Some(stop),
        }
    }

    /// A request presenting `certificate`, and `token` if there is one.
    async fn get(
        &self,
        path: &str,
        certificate: Option<&Pair>,
        token: Option<&str>,
    ) -> Result<String, String> {
        let config = certificates::client(&self.authority.certificate, certificate);

        certificates::get(self.address, config, path, token).await
    }

    async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }

        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.serving).await;
        drop(self.directory);
    }
}

/// The happy path, and it takes both: a certificate the configured authority
/// signed, and the bearer token that names a principal.
#[tokio::test]
async fn a_client_with_a_certificate_and_a_token_is_served() {
    let fixture = Fixture::start(true).await;

    let response = fixture
        .get(
            "/billing/prod",
            Some(&fixture.client),
            Some(common::BILLING_TOKEN),
        )
        .await
        .expect("the handshake and the request both succeed");

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("db.internal"), "{response}");

    fixture.shutdown().await;
}

/// **The decision this crate made, asserted.** A certificate is a gate, not
/// a credential: it gets a caller a connection and nothing else. If mTLS
/// were ever quietly turned into an alternative to the token, this is the
/// test that would fail.
#[tokio::test]
async fn a_certificate_is_not_a_credential() {
    let fixture = Fixture::start(true).await;

    let response = fixture
        .get("/billing/prod", Some(&fixture.client), None)
        .await
        .expect("the handshake succeeds; the request does not");

    assert!(
        response.starts_with("HTTP/1.1 401 Unauthorized"),
        "{response}"
    );
    assert!(response.contains("www-authenticate: Bearer"), "{response}");
    assert!(!response.contains("db.internal"), "{response}");

    fixture.shutdown().await;
}

/// And the other half of it: a certificate cannot widen a grant either. The
/// caller is fully authenticated at both layers and still cannot read a
/// section it was not granted — the same 404, with the same body, as a
/// section nobody serves.
#[tokio::test]
async fn a_certificate_does_not_widen_a_grant() {
    let fixture = Fixture::start(true).await;

    let refused = fixture
        .get(
            "/payroll/prod",
            Some(&fixture.client),
            Some(common::BILLING_TOKEN),
        )
        .await
        .expect("the handshake succeeds");
    let missing = fixture
        .get(
            "/ledger/prod",
            Some(&fixture.client),
            Some(common::BILLING_TOKEN),
        )
        .await
        .expect("the handshake succeeds");

    assert!(refused.starts_with("HTTP/1.1 404 Not Found"), "{refused}");
    assert_eq!(
        body(&refused),
        body(&missing),
        "a section that is not yours and one that does not exist stay indistinguishable"
    );

    fixture.shutdown().await;
}

/// No certificate, no request. The refusal happens before HTTP exists, so
/// there is no status code — and the audit log says so once, saying nothing
/// else.
#[tokio::test]
async fn a_client_with_no_certificate_never_becomes_a_request() {
    let fixture = Fixture::start(true).await;

    let outcome = fixture
        .get("/billing/prod", None, Some(common::BILLING_TOKEN))
        .await;

    match outcome {
        Err(error) => assert!(
            error.contains("handshake") || error.contains("read"),
            "the connection has to end at the handshake: {error}"
        ),
        Ok(response) => assert!(
            !response.contains("HTTP/1.1 200"),
            "a caller with no certificate was served: {response}"
        ),
    }

    // The audit line, which is what an operator with a misconfigured client
    // CA is going to go looking for.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

    while fixture.log.lines().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    assert_eq!(
        fixture.log.lines(),
        [
            "audit caller=- application=- profile=- endpoint=tls outcome=unauthenticated \
          generation=-"
        ],
        "a refused handshake is recorded, and records nothing about the certificate"
    );

    fixture.shutdown().await;
}

/// A certificate is only a gate if the gate has a lock. This one is signed
/// by a perfectly valid authority that this server was not told about.
#[tokio::test]
async fn a_certificate_from_another_authority_is_refused() {
    let fixture = Fixture::start(true).await;
    let stranger = fixture
        .stranger
        .sign(fixture.directory.path(), "impostor", false, 0o600);

    let outcome = fixture
        .get(
            "/billing/prod",
            Some(&stranger),
            Some(common::BILLING_TOKEN),
        )
        .await;

    match outcome {
        Err(error) => assert!(!error.is_empty(), "{error}"),
        Ok(response) => assert!(
            !response.contains("HTTP/1.1 200"),
            "a certificate from an unconfigured authority was admitted: {response}"
        ),
    }

    fixture.shutdown().await;
}

/// Without `client_ca` the server terminates TLS and asks for nothing back —
/// the shape for a deployment that wants the wire encrypted and authenticates
/// with the token alone.
#[tokio::test]
async fn tls_without_a_client_ca_asks_for_no_certificate() {
    let fixture = Fixture::start(false).await;

    let response = fixture
        .get("/billing/prod", None, Some(common::BILLING_TOKEN))
        .await
        .expect("no certificate is asked for");

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");

    fixture.shutdown().await;
}

/// The posture is rustls's, not this crate's: default protocol versions,
/// default suites, in the provider's own order. What that gets a modern
/// client is TLS 1.3, and the one thing this crate does set is ALPN.
#[tokio::test]
async fn the_posture_is_rustls_defaults_and_alpn_is_http11() {
    let fixture = Fixture::start(true).await;
    let mut config = certificates::client(&fixture.authority.certificate, Some(&fixture.client));

    // A client that would rather speak HTTP/2, which this build cannot: the
    // point of setting ALPN at all is that this is settled during the
    // handshake instead of becoming a connection that fails after it.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let stream = tokio::net::TcpStream::connect(fixture.address)
        .await
        .expect("the server is listening");
    let stream = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(
            rustls::pki_types::ServerName::from(fixture.address.ip()),
            stream,
        )
        .await
        .expect("the handshake succeeds");

    let (_, connection) = stream.get_ref();

    assert_eq!(
        connection.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_3),
        "a client offering rustls's defaults gets 1.3"
    );
    assert_eq!(
        connection.alpn_protocol(),
        Some(&b"http/1.1"[..]),
        "this build serves HTTP/1.1 and says so, rather than letting a client pick h2"
    );

    drop(stream);
    fixture.shutdown().await;
}

/// The refusal that is this crate's own opinion, at the layer it belongs:
/// startup, before a socket exists, naming the fix.
#[cfg(unix)]
#[tokio::test]
async fn a_private_key_anybody_can_read_refuses_to_start() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let authority = Authority::new(directory.path(), "configured");
    // 0644 — the mode a Kubernetes secret volume mounts with by default,
    // which is exactly why this refusal is worth having.
    let pair = authority.sign(directory.path(), "server", true, 0o644);
    let billing = directory.path().join("billing.toml");
    std::fs::write(&billing, "[billing]\nhost = 'db'\n").expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        bind: "127.0.0.1:0".to_owned(),
        tls: Some(TlsConfig {
            certificate: pair.certificate.display().to_string(),
            key: pair.key.display().to_string(),
            client_ca: None,
            crl: None,
        }),
        sections: vec![section("billing", "prod", billing.display().to_string())],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };

    let error = Server::start_with(&config, dynamic_config_server::NoAudit)
        .expect_err("mode 644 is not a secret");
    let rendered = error.to_string();

    assert!(rendered.contains("0644"), "{rendered}");
    assert!(rendered.contains("chmod 600"), "{rendered}");
    assert!(rendered.contains("defaultMode: 0400"), "{rendered}");
}

/// A key that is not the certificate's key is caught at startup rather than
/// at the first handshake, and the message says which two files disagree
/// without quoting either.
#[tokio::test]
async fn a_key_that_does_not_match_the_certificate_refuses_to_start() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let authority = Authority::new(directory.path(), "configured");
    let server = authority.sign(directory.path(), "server", true, 0o600);
    let other = authority.sign(directory.path(), "other", true, 0o600);
    let billing = directory.path().join("billing.toml");
    std::fs::write(&billing, "[billing]\nhost = 'db'\n").expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        bind: "127.0.0.1:0".to_owned(),
        tls: Some(TlsConfig {
            certificate: server.certificate.display().to_string(),
            key: other.key.display().to_string(),
            client_ca: None,
            crl: None,
        }),
        sections: vec![section("billing", "prod", billing.display().to_string())],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };

    let error = Server::start_with(&config, dynamic_config_server::NoAudit)
        .expect_err("the key belongs to another certificate");
    let rendered = format!("{error} / {error:?}");

    assert!(
        rendered.contains("is not the key of the certificate"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("BEGIN"),
        "a refusal quoted a PEM file: {rendered}"
    );
}

/// A `client_ca` that is not an authority is a startup refusal too: the
/// alternative is a server that starts and rejects every client, which looks
/// like a client problem for as long as it takes to work out that it is not.
#[tokio::test]
async fn a_client_ca_that_is_not_pem_refuses_to_start() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let authority = Authority::new(directory.path(), "configured");
    let server = authority.sign(directory.path(), "server", true, 0o600);
    let not_an_authority = directory.path().join("nonsense.pem");
    std::fs::write(&not_an_authority, "this is not a certificate\n").expect("writable");
    let billing = directory.path().join("billing.toml");
    std::fs::write(&billing, "[billing]\nhost = 'db'\n").expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        bind: "127.0.0.1:0".to_owned(),
        tls: Some(TlsConfig {
            certificate: server.certificate.display().to_string(),
            key: server.key.display().to_string(),
            client_ca: Some(not_an_authority.display().to_string()),
            crl: None,
        }),
        sections: vec![section("billing", "prod", billing.display().to_string())],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };

    let error = Server::start_with(&config, dynamic_config_server::NoAudit)
        .expect_err("a file of prose is not a certificate authority");

    assert!(error.to_string().contains("tls.client_ca"), "{error}");
}

/// Revocation is refused rather than half-built, and it is refused at
/// startup with the certificates otherwise perfectly good — so an operator
/// who reaches for a CRL is told that this server checks none, instead of
/// getting a server that starts and quietly checks nothing.
#[tokio::test]
async fn a_configured_crl_refuses_to_start_and_names_what_can_be_revoked() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let authority = Authority::new(directory.path(), "configured");
    let server = authority.sign(directory.path(), "server", true, 0o600);
    let list = directory.path().join("clients.crl");
    std::fs::write(&list, "irrelevant: this file is never opened\n").expect("writable");
    let billing = directory.path().join("billing.toml");
    std::fs::write(&billing, "[billing]\nhost = 'db'\n").expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        bind: "127.0.0.1:0".to_owned(),
        tls: Some(TlsConfig {
            certificate: server.certificate.display().to_string(),
            key: server.key.display().to_string(),
            client_ca: Some(authority.certificate.display().to_string()),
            crl: Some(list.display().to_string()),
        }),
        sections: vec![section("billing", "prod", billing.display().to_string())],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };

    let error = Server::start_with(&config, dynamic_config_server::NoAudit)
        .expect_err("this server checks no revocation and says so");
    let rendered = error.to_string();

    assert!(rendered.contains("`tls.crl`"), "{rendered}");
    // A refusal has to leave the operator somewhere to go, and the answer is
    // the credential this server can actually withdraw.
    assert!(rendered.contains("token"), "{rendered}");
    assert!(rendered.contains("short-lived"), "{rendered}");
}

/// The measurement the refusal above rests on, kept executable.
///
/// AGENTS.md records "believing a manifest" as a mistake this repository has
/// actually paid for, and `Refusal::RevocationUnsupported`'s message makes a
/// factual claim about somebody else's library. So the claim is measured
/// rather than cited: if rustls changes either default, this fails, and the
/// refusal gets reconsidered instead of quietly becoming wrong.
///
/// Two handshakes, and together they are the whole decision:
///
/// - a CRL that went out of date in 2020 is used **without complaint** by
///   default, so the twenty lines that look like revocation stop being true
///   the moment the file stops being refreshed — and nothing reports it;
/// - the one switch that refuses a stale list refuses a *clean, unrevoked*
///   client along with it, which is a fleet-wide outage rather than a
///   revocation.
///
/// A file watcher rescues neither, which is why the shape the notes
/// suggested was tried and dropped: what has to be noticed is the **absence**
/// of a write, and no filesystem event fires for that.
#[tokio::test]
async fn the_measurement_behind_refusing_revocation_still_holds() {
    use rcgen::{
        BasicConstraints, CertificateParams, CertificateRevocationListParams, DnType,
        ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, SerialNumber,
    };
    use rustls::pki_types::{
        CertificateDer, CertificateRevocationListDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName,
    };
    use rustls::server::WebPkiClientVerifier;
    use rustls::RootCertStore;

    let mut authority = CertificateParams::new(Vec::new()).expect("no SANs is a valid CA");
    authority.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    authority
        .distinguished_name
        .push(DnType::CommonName, "measured");
    authority.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

    let authority_key = KeyPair::generate().expect("a key pair");
    let authority_der = authority
        .self_signed(&authority_key)
        .expect("a self-signed CA")
        .der()
        .clone();
    let issuer = Issuer::new(authority, authority_key);

    let leaf = |name: &str, server: bool| {
        let names = if server {
            vec!["127.0.0.1".to_owned()]
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
        let certificate = params.signed_by(&key, &issuer).expect("a signature");

        (
            vec![certificate.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
    };

    let (server_chain, server_key) = leaf("server", true);
    let (client_chain, client_key) = leaf("billing-pod", false);

    // Signed by the same authority, revoking nobody, and out of date since
    // 2020 — the shape a CRL takes when whatever was writing it stopped.
    let stale: CertificateRevocationListDer<'static> = CertificateRevocationListParams {
        this_update: rcgen::date_time_ymd(2020, 1, 1),
        next_update: rcgen::date_time_ymd(2020, 1, 2),
        crl_number: SerialNumber::from(1u64),
        issuing_distribution_point: None,
        revoked_certs: Vec::new(),
        key_identifier_method: rcgen::KeyIdMethod::Sha256,
    }
    .signed_by(&issuer)
    .expect("the CA may sign a CRL")
    .der()
    .clone();

    // One real mutual handshake against the stale list, with expiry enforced
    // or not. What is returned is the *server's* side, which is where a
    // client certificate is judged.
    async fn handshake(
        authority: &CertificateDer<'static>,
        server: (&[CertificateDer<'static>], &PrivateKeyDer<'static>),
        client: (&[CertificateDer<'static>], &PrivateKeyDer<'static>),
        stale: &CertificateRevocationListDer<'static>,
        enforce_expiry: bool,
    ) -> Result<(), String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = RootCertStore::empty();

        roots.add(authority.clone()).expect("a usable authority");

        let mut builder =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                .with_crls(vec![stale.clone()]);

        if enforce_expiry {
            builder = builder.enforce_revocation_expiration();
        }

        let verifier = builder.build().expect("one anchor and one usable CRL");
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .expect("ring supports the defaults")
            .with_client_cert_verifier(verifier)
            .with_single_cert(server.0.to_vec(), server.1.clone_key())
            .expect("the certificate and key agree");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback, port zero");
        let address = listener.local_addr().expect("a bound listener has one");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let accepting = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("the client connects");

            acceptor
                .accept(stream)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        });

        let mut client_roots = RootCertStore::empty();

        client_roots
            .add(authority.clone())
            .expect("a usable authority");

        let client_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the defaults")
            .with_root_certificates(client_roots)
            .with_client_auth_cert(client.0.to_vec(), client.1.clone_key())
            .expect("the certificate and key agree");
        let stream = tokio::net::TcpStream::connect(address)
            .await
            .expect("a listening socket");

        // The client's own result is not the assertion — under TLS 1.3 it can
        // finish before the server's alert arrives — so it is driven and
        // dropped.
        let _ = tokio_rustls::TlsConnector::from(Arc::new(client_config))
            .connect(ServerName::from(address.ip()), stream)
            .await;

        accepting.await.expect("the accepting task does not panic")
    }

    let server = (server_chain.as_slice(), &server_key);
    let client = (client_chain.as_slice(), &client_key);

    let by_default = handshake(&authority_der, server, client, &stale, false).await;
    let enforced = handshake(&authority_der, server, client, &stale, true).await;

    assert!(
        by_default.is_ok(),
        "rustls accepted a CRL years out of date without a word, which is half of why this \
         crate ships none. If it no longer does, revisit `Refusal::RevocationUnsupported`: \
         {by_default:?}"
    );

    let refused = enforced.expect_err(
        "`enforce_revocation_expiration` refused a stale list, which is the other half. If it \
         no longer does, the refusal's reasoning has changed",
    );

    assert!(
        refused.contains("expired"),
        "the refusal should be about the list's age rather than about the client: {refused}"
    );
}

/// The body of an HTTP response, for the comparison that has to be exact.
fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}
