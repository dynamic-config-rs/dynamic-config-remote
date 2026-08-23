//! The retry decision and the v1-mount refusal, against a scripted server.
//!
//! No Docker: these start a `TcpListener`, speak just enough HTTP/1.1 for
//! `ureq`, and count requests. They pin down decisions the container tests
//! cannot see: when a refused request earns a second one, and that a v1 mount
//! ends a watch instead of silently never firing.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dynamic_config::{RemoteSource, RemoteWatch};
use dynamic_config_vault::{Keys, Vault};

/// Serves `responses` in order, one per connection, and counts requests.
fn scripted(responses: Vec<String>) -> (String, Arc<AtomicUsize>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));

    let counter = Arc::clone(&requests);
    let server = std::thread::spawn(move || {
        for response in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };

            let mut seen = Vec::new();
            let mut byte = [0u8; 1];

            while !seen.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                seen.push(byte[0]);
            }

            counter.fetch_add(1, Ordering::SeqCst);

            let _ = stream.write_all(response.as_bytes());
        }
    });

    (address, requests, server)
}

fn http(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn a_supplied_token_is_never_retried_because_it_cannot_change() {
    let (address, requests, server) = scripted(vec![http(
        "403 Forbidden",
        r#"{"errors": ["permission denied"]}"#,
    )]);

    let source = Vault::new(&address, "secret", "myapp/db").with_token("supplied");
    let error = source.fetch().expect_err("the server said 403");

    server.join().unwrap();

    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "invalidating a supplied token just retries the identical string; \
         the second request is pure waste: {error}"
    );
}

#[test]
fn a_500_on_a_path_named_403_is_not_mistaken_for_a_refused_token() {
    // `describe()` puts the path in every error message, so a path named
    // `myapp/403` makes every error's *text* contain "403". Only the typed
    // status may drive the retry.
    let (address, requests, server) = scripted(vec![http("500 Internal Server Error", "boom")]);

    let source = Vault::new(&address, "secret", "myapp/403").with_token("supplied");
    let error = source.fetch().expect_err("the server failed");

    server.join().unwrap();

    assert!(error.to_string().contains("myapp/403"), "{error}");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "a 500 is not an auth failure, whatever the path is called"
    );
}

/// A v1 mount has no version counter, so a watch cannot work — and must say
/// so instead of polling forever and never firing.
#[test]
fn a_v1_mount_ends_the_watch_with_an_error_instead_of_never_firing() {
    // A v1 mount answers metadata reads with the secret's own shape: a `data`
    // block with the fields, and no `current_version` anywhere.
    let (address, _requests, server) = scripted(vec![http(
        "200 OK",
        r#"{"data": {"host": "db", "port": 5432}}"#,
    )]);

    let source = Vault::new(&address, "kv-v1", "myapp/db").with_token("supplied");
    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let error = source
        .watch(&watching, Duration::from_millis(100), |_| Ok(()))
        .expect_err("a mount with no version counter cannot be watched");

    server.join().unwrap();

    assert!(error.to_string().contains("KV v2"), "{error}");
}

#[test]
fn an_unreachable_vault_is_a_prompt_error_naming_the_address() {
    let source = Vault::new("http://127.0.0.1:9", "secret", "myapp/db").with_token("supplied");

    let error = source.fetch().expect_err("nothing is listening");

    assert_eq!(
        error.kind(),
        dynamic_config::ErrorKind::Remote,
        "a Vault that is down comes back; a watch loop must back off rather \
         than stop, which is what `Auth` would tell it to do"
    );
    assert!(error.to_string().contains("127.0.0.1:9"), "{error}");
}

/// A refused token is `Auth`, not `Remote` — the distinction a caller
/// branches on to page somebody instead of retrying forever.
#[test]
fn a_refused_token_is_an_auth_failure_that_never_repeats_the_token() {
    let (address, _requests, server) = scripted(vec![http(
        "403 Forbidden",
        r#"{"errors": ["permission denied"]}"#,
    )]);

    let source = Vault::new(&address, "secret", "myapp/db").with_token("hunter2-vault-token");
    let error = source.fetch().expect_err("the server said 403");

    server.join().unwrap();

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Auth, "{error}");
    let printed = format!("{error} {error:?} {source:?}");
    assert!(!printed.contains("hunter2"), "{printed}");
}

/// A sealed Vault answers 503 and un-seals later, so it stays `Remote`: a
/// watch loop should wait it out rather than give up on it.
#[test]
fn a_sealed_vault_is_not_an_auth_failure() {
    let (address, _requests, server) = scripted(vec![http(
        "503 Service Unavailable",
        r#"{"errors": ["Vault is sealed"]}"#,
    )]);

    let source = Vault::new(&address, "secret", "myapp/db").with_token("supplied");
    let error = source.fetch().expect_err("the Vault is sealed");

    server.join().unwrap();

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote, "{error}");
}

/// No credentials at all: nothing was ever sent, so the store is not what
/// failed and no retry will produce one.
#[test]
fn missing_credentials_are_an_auth_failure_before_anything_is_sent() {
    let source = Vault::new("http://127.0.0.1:9", "secret", "myapp/db");

    let error = source.fetch().expect_err("no token was supplied");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Auth, "{error}");
    assert!(error.to_string().contains("with_token"), "{error}");
}

/// A secret whose fields are `values`, at version 1.
fn secret(values: &str) -> String {
    http(
        "200 OK",
        &format!(r#"{{"data": {{"data": {values}, "metadata": {{"version": 1}}}}}}"#),
    )
}

/// The rule a list of paths inherits from a list of files: call order, and the
/// later path wins. For Vault that is layering one section — a shared secret
/// and an override — which is the shape its per-path policies produce.
#[test]
fn several_paths_merge_in_call_order_and_the_later_path_wins() {
    let (address, requests, server) = scripted(vec![
        secret(r#"{"host": "shared", "port": 5432}"#),
        secret(r#"{"host": "override"}"#),
    ]);

    let source = Vault::new(
        &address,
        "secret",
        Keys::several(["myapp/db-defaults", "myapp/db-credentials"]),
    )
    .with_token("supplied");

    let document = source.fetch().expect("both paths answered");

    server.join().unwrap();

    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "KV v2 has no batch read, so a list is one request per path"
    );

    let merged: serde_json::Value =
        serde_json::from_str(&document.text).expect("the merged section is JSON");

    assert_eq!(merged["db"]["host"], "override", "the later path wins");
    assert_eq!(
        merged["db"]["port"], 5432,
        "a field only the earlier path has survives the merge"
    );
}

/// One unreadable path fails the whole fetch: a section quietly missing half
/// of itself is worse than a refresh that failed and left the last document
/// serving. And the report names the path that failed, never what the paths
/// that answered were holding.
#[test]
fn one_unreadable_path_fails_the_whole_fetch_and_never_prints_a_value() {
    let (address, _requests, server) = scripted(vec![
        secret(r#"{"password": "hunter2-vault-value"}"#),
        http("404 Not Found", r#"{"errors": []}"#),
    ]);

    let source = Vault::new(
        &address,
        "secret",
        Keys::several(["myapp/db-defaults", "myapp/db-missing"]),
    )
    .with_token("supplied");

    let error = source.fetch().expect_err("the second path is not there");

    server.join().unwrap();

    let printed = format!("{error} {error:?} {source:?}");

    assert!(printed.contains("myapp/db-missing"), "{printed}");
    assert!(
        !printed.contains("hunter2"),
        "a value that was read must never reach a diagnostic: {printed}"
    );
}

/// The version counter a watch polls belongs to one secret; a set of them has
/// none. Refused at `watch`, so it fails now rather than in six hours by
/// never firing.
#[test]
fn a_multi_path_source_refuses_to_be_watched_and_says_what_to_do_instead() {
    let source = Vault::new(
        "http://127.0.0.1:9",
        "secret",
        Keys::several(["myapp/db-defaults", "myapp/db-credentials"]),
    )
    .with_token("supplied");

    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let error = source
        .watch(&watching, Duration::from_millis(50), |_| Ok(()))
        .expect_err("a set of secrets has no version of its own");

    assert!(error.to_string().contains("several paths"), "{error}");
    assert!(error.to_string().contains("refresh_remote"), "{error}");
}

/// A list of one is a list, and it must read exactly like the single-path
/// spelling it is: same request, same document, same section key.
#[test]
fn a_list_of_one_path_reads_exactly_as_the_single_path_spelling_does() {
    let (address, requests, server) = scripted(vec![secret(r#"{"host": "db"}"#)]);

    let source = Vault::new(&address, "secret", Keys::several(["myapp/db"])).with_token("supplied");

    let document = source.fetch().expect("the path answered");

    server.join().unwrap();

    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(document.text, r#"{"db":{"host":"db"}}"#);
}

/// A panicking callback must end the watch with an orderly error — not
/// unwind through the loop and kill the caller's thread with the handle
/// still looking alive.
#[test]
fn a_panicking_callback_ends_the_watch_with_an_error() {
    // Two metadata answers with moving versions: the first tick primes, the
    // second delivers a change into the panicking callback.
    let (address, _requests, server) = scripted(vec![
        http("200 OK", r#"{"data": {"current_version": 1}}"#),
        http("200 OK", r#"{"data": {"current_version": 2}}"#),
        http(
            "200 OK",
            r#"{"data": {"data": {"host": "db"}, "metadata": {"version": 2}}}"#,
        ),
    ]);

    let source = Vault::new(&address, "secret", "myapp/db").with_token("supplied");
    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let error = source
        .watch(&watching, Duration::from_millis(50), |_| {
            panic!("a bug in the caller's callback")
        })
        .expect_err("the panic must surface as an error, not kill the thread");

    server.join().unwrap();

    assert!(error.to_string().contains("panicked"), "{error}");
}

// ---------------------------------------------------------------------------
// Reporting what the watch loop cannot otherwise say.
//
// This watch is a poll of a *version counter*, so a Vault that is healthy and
// a Vault whose token expired yesterday deliver exactly the same thing:
// nothing. `apply` records a delivery and there was never anything to record a
// failed tick — which left `remote_up` reporting the last delivery rather than
// the last attempt. These prove the other half.
// ---------------------------------------------------------------------------

use dynamic_config::dynamic_config;
use serde::Deserialize;

/// One config type per test, as the repository's own rule requires: the
/// snapshot, the source and the status all live in `static`s keyed by the type.
#[dynamic_config]
#[derive(Debug, Deserialize)]
struct SealedDb {
    host: String,
}

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct UnreadableDb {
    host: String,
}

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct WrongMountDb {
    host: String,
}

/// Polls `status()` until the store stops looking reachable, or gives up.
///
/// A watch loop is a thread: the failure lands when the tick does, and waiting
/// a fixed time here would be either flaky or slow.
fn wait_for_unreachable(sink: &dynamic_config::RemoteSink) -> dynamic_config::RemoteStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);

    loop {
        let status = sink.status();

        if status.reachable() == Some(false) || std::time::Instant::now() > deadline {
            return status;
        }

        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The claim in one test: a Vault that answered once and then went away — a
/// seal, a revoked token, a network partition — is **visible**, and the
/// staleness clock keeps its value, because how long the served document has
/// been stale is the other half of what an alert needs.
#[test]
fn a_watch_whose_version_check_fails_reports_it_without_resetting_the_fetch_clock() {
    // One scripted answer, for the fetch that establishes a healthy store. The
    // listener closes behind it, so every metadata check the watch then makes
    // is refused.
    let (address, _requests, server) = scripted(vec![secret(r#"{"host": "a"}"#)]);

    SealedDb::set_remote(Vault::new(&address, "secret", "myapp/db").with_token("supplied"));
    SealedDb::refresh_remote().expect("the Vault answers the first fetch");
    SealedDb::builder("db")
        .init()
        .expect("the fetched secret is the whole configuration");

    server.join().unwrap();

    // Taken once, where the loop is wired — which is also the only handle a
    // caller has on the status behind it.
    let sink = SealedDb::remote_sink();
    let answering = sink.status();

    assert_eq!(answering.reachable(), Some(true), "the fetch answered");

    let watcher = Vault::new(&address, "secret", "myapp/db")
        .with_token("supplied")
        .reporting_to(sink);

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let thread =
        std::thread::spawn(move || watcher.watch(&watching, Duration::from_millis(50), |_| Ok(())));

    let failing = wait_for_unreachable(&sink);

    watch.stop();

    let outcome = thread.join().expect("the watch thread");

    assert!(
        outcome.is_ok(),
        "a failed check is waited out, not returned: {outcome:?}"
    );

    assert_eq!(
        failing.reachable(),
        Some(false),
        "a poll that cannot reach the Vault must not go on reporting the \
         last delivery as health"
    );
    assert!(failing.consecutive_failures >= 1);
    assert_eq!(
        failing.last_fetch, answering.last_fetch,
        "a failed attempt moves the failure streak and nothing else: the \
         staleness clock has to keep ageing, or the pair of metrics cannot \
         say how old the document being served is"
    );
    assert_eq!(
        failing.fetches, answering.fetches,
        "an attempt that came back with nothing is not a fetch"
    );

    // The rule the whole family is built around, at the newest path: only an
    // `ErrorKind` and a key path are recorded, never the store's address.
    let failure = failing.last_failure.expect("the failure was recorded");
    assert_eq!(failure.kind, dynamic_config::ErrorKind::Remote);
    assert!(
        !failure.path.contains(&address),
        "a store's address must never enter a status: {}",
        failure.path
    );

    assert_eq!(
        SealedDb::current().host,
        "a",
        "and the secret the Vault did answer with is still serving: a failed \
         attempt is no reason to stop serving the last good one"
    );
}

/// The other failure site in this loop, and the one that was invisible twice
/// over: the counter moved, so something *did* change — and the read of the
/// secret itself failed. A policy that stopped allowing the read looks exactly
/// like this, and reported nothing.
#[test]
fn a_version_that_moved_and_a_secret_that_will_not_be_read_is_reported() {
    let (address, requests, server) = scripted(vec![
        // The fetch.
        secret(r#"{"host": "a"}"#),
        // The first tick primes the version without firing.
        http("200 OK", r#"{"data": {"current_version": 1}}"#),
        // The second says it moved …
        http("200 OK", r#"{"data": {"current_version": 2}}"#),
        // … and the read of the secret it names fails.
        http("500 Internal Server Error", "boom"),
    ]);

    UnreadableDb::set_remote(Vault::new(&address, "secret", "myapp/db").with_token("supplied"));
    UnreadableDb::refresh_remote().expect("the Vault answers the first fetch");
    UnreadableDb::builder("db")
        .init()
        .expect("the fetched secret is the whole configuration");

    let sink = UnreadableDb::remote_sink();
    let answering = sink.status();

    let watcher = Vault::new(&address, "secret", "myapp/db")
        .with_token("supplied")
        .reporting_to(sink);

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    // A second between ticks, so the read that failed is the only attempt that
    // can have been recorded when the assertion below runs.
    let thread =
        std::thread::spawn(move || watcher.watch(&watching, Duration::from_secs(1), |_| Ok(())));

    let failing = wait_for_unreachable(&sink);

    assert_eq!(
        requests.load(Ordering::SeqCst),
        4,
        "the fetch, two metadata checks and the refused read: what was \
         recorded is the read, not a later tick"
    );

    watch.stop();

    let outcome = thread.join().expect("the watch thread");
    server.join().unwrap();

    assert!(
        outcome.is_ok(),
        "a failed read is waited out, not returned: {outcome:?}"
    );
    assert_eq!(failing.reachable(), Some(false));
    assert_eq!(
        failing.last_fetch, answering.last_fetch,
        "the staleness clock keeps ageing"
    );
    assert_eq!(UnreadableDb::current().host, "a");
}

/// A mount with no version counter ends the watch — and records it on the way
/// out. A watch that has ended is a configuration that has stopped updating
/// for good, which is the last thing that should read as a healthy store.
#[test]
fn a_mount_with_no_version_counter_reports_the_failure_that_ends_the_watch() {
    let (address, _requests, server) = scripted(vec![
        // The fetch.
        secret(r#"{"host": "a"}"#),
        // A v1 mount answers metadata reads with the secret's own shape: no
        // `current_version` anywhere.
        http("200 OK", r#"{"data": {"host": "db", "port": 5432}}"#),
    ]);

    WrongMountDb::set_remote(Vault::new(&address, "secret", "myapp/db").with_token("supplied"));
    WrongMountDb::refresh_remote().expect("the Vault answers the first fetch");
    WrongMountDb::builder("db")
        .init()
        .expect("the fetched secret is the whole configuration");

    let sink = WrongMountDb::remote_sink();
    let answering = sink.status();

    let watcher = Vault::new(&address, "secret", "myapp/db")
        .with_token("supplied")
        .reporting_to(sink);

    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let error = watcher
        .watch(&watching, Duration::from_millis(50), |_| Ok(()))
        .expect_err("a mount with no version counter cannot be watched");

    server.join().unwrap();

    assert!(error.to_string().contains("KV v2"), "{error}");

    let failing = sink.status();

    assert_eq!(
        failing.reachable(),
        Some(false),
        "the watch is over and will deliver nothing else; a status that still \
         said `up` would be describing a loop that no longer exists"
    );
    assert_eq!(
        failing.last_fetch, answering.last_fetch,
        "the staleness clock keeps ageing"
    );
    assert_eq!(WrongMountDb::current().host, "a");
}

// ---------------------------------------------------------------------------
// TLS: a private certificate authority, and a client certificate.
//
// Generated here rather than committed. A certificate expires, and a fixture
// that expires turns into a suite that fails on a date nobody chose — and the
// only honest substrate for "does this actually handshake" is an authority
// nothing on the machine already trusts.
// ---------------------------------------------------------------------------

use dynamic_config_vault::TlsConfig;
use rustls::pki_types::pem::PemObject;

/// A certificate authority, a server certificate for `127.0.0.1`, and a client
/// certificate — each with the key that goes with it, all PEM.
struct Authority {
    ca_pem: String,
    server_chain: Vec<rustls::pki_types::CertificateDer<'static>>,
    server_key: rustls::pki_types::PrivateKeyDer<'static>,
    client_pem: String,
    client_key_pem: String,
}

fn authority() -> Authority {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);

    let server_key = KeyPair::generate().unwrap();
    let server = CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .unwrap()
        .signed_by(&server_key, &issuer)
        .unwrap();

    let client_key = KeyPair::generate().unwrap();
    let client = CertificateParams::new(vec!["myapp".to_owned()])
        .unwrap()
        .signed_by(&client_key, &issuer)
        .unwrap();

    Authority {
        ca_pem: ca.pem(),
        server_chain: vec![server.der().clone(), ca.der().clone()],
        server_key: rustls::pki_types::PrivateKeyDer::try_from(server_key.serialize_der()).unwrap(),
        client_pem: client.pem(),
        client_key_pem: client_key.serialize_pem(),
    }
}

/// The same scripted server as above, wrapped in TLS.
///
/// `demand_client_certificate` is what turns it into an mTLS server: rustls
/// refuses the handshake outright when a client presents none, which is
/// exactly the failure a caller who forgot `with_client_certificate_files`
/// should see.
fn scripted_tls(
    authority: &Authority,
    demand_client_certificate: bool,
    responses: Vec<String>,
) -> (String, std::thread::JoinHandle<()>) {
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());

    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap();

    let config = if demand_client_certificate {
        let mut roots = rustls::RootCertStore::empty();
        for certificate in
            rustls::pki_types::CertificateDer::pem_slice_iter(authority.ca_pem.as_bytes())
        {
            roots.add(certificate.unwrap()).unwrap();
        }

        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            std::sync::Arc::new(roots),
            provider,
        )
        .build()
        .unwrap();

        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    }
    .with_single_cert(
        authority.server_chain.clone(),
        authority.server_key.clone_key(),
    )
    .unwrap();

    let config = std::sync::Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("https://{}", listener.local_addr().unwrap());

    let server = std::thread::spawn(move || {
        for response in responses {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };

            let Ok(connection) = rustls::ServerConnection::new(std::sync::Arc::clone(&config))
            else {
                return;
            };

            let mut tls = rustls::StreamOwned::new(connection, stream);

            let mut seen = Vec::new();
            let mut byte = [0u8; 1];

            while !seen.ends_with(b"\r\n\r\n") && tls.read(&mut byte).is_ok_and(|n| n == 1) {
                seen.push(byte[0]);
            }

            let _ = tls.write_all(response.as_bytes());
            let _ = tls.flush();
        }
    });

    (address, server)
}

/// The whole point of the feature: a Vault whose certificate chains to an
/// authority nothing on this machine has ever heard of, read successfully
/// because the caller named the CA — with no `ureq` type anywhere in the
/// calling code.
#[test]
fn a_secret_is_read_from_a_vault_behind_a_private_certificate_authority() {
    let authority = authority();
    let (address, server) = scripted_tls(&authority, false, vec![secret(r#"{"host": "db"}"#)]);

    let source = Vault::new(&address, "secret", "myapp/db")
        .with_token("supplied")
        .with_tls(TlsConfig::new().with_ca_certificate_pem(authority.ca_pem.clone()));

    let document = source.fetch().expect("the private CA was trusted");

    server.join().unwrap();

    assert_eq!(document.text, r#"{"db":{"host":"db"}}"#);
}

/// The same server, without naming the authority: the handshake must fail.
/// Without this the test above would pass just as well against a machine that
/// happened to trust the certificate for some other reason.
#[test]
fn the_same_vault_is_refused_when_the_authority_is_not_named() {
    let authority = authority();
    let (address, server) = scripted_tls(&authority, false, vec![secret(r#"{"host": "db"}"#)]);

    let source = Vault::new(&address, "secret", "myapp/db").with_token("supplied");

    let error = source
        .fetch()
        .expect_err("nothing on this machine trusts a CA generated a moment ago");

    // The server may never see a byte, so it is not joined: the client hung
    // up during the handshake, and `accept` is left waiting on a connection
    // that will not come.
    drop(server);

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote, "{error}");
}

/// mTLS end to end: a server that refuses a client with no certificate, and a
/// caller that presents one — again as data, with no `ureq` type in sight.
#[test]
fn a_client_certificate_is_presented_to_a_vault_that_demands_one() {
    let authority = authority();
    let (address, server) = scripted_tls(&authority, true, vec![secret(r#"{"host": "db"}"#)]);

    let source = Vault::new(&address, "secret", "myapp/db")
        .with_token("supplied")
        .with_tls(
            TlsConfig::new()
                .with_ca_certificate_pem(authority.ca_pem.clone())
                .with_client_certificate_pem(
                    authority.client_pem.clone(),
                    authority.client_key_pem.clone(),
                ),
        );

    let document = source.fetch().expect("the client certificate was accepted");

    server.join().unwrap();

    assert_eq!(document.text, r#"{"db":{"host":"db"}}"#);
}

/// The negative half of the pair: the same server, the CA trusted, and no
/// client certificate. It must fail — otherwise the test above proves only
/// that a server *can* be talked to.
#[test]
fn a_vault_that_demands_a_client_certificate_refuses_a_caller_without_one() {
    let authority = authority();
    let (address, server) = scripted_tls(&authority, true, vec![secret(r#"{"host": "db"}"#)]);

    let source = Vault::new(&address, "secret", "myapp/db")
        .with_token("supplied")
        .with_tls(TlsConfig::new().with_ca_certificate_pem(authority.ca_pem.clone()));

    let error = source
        .fetch()
        .expect_err("the server demanded a certificate");

    drop(server);

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote, "{error}");
}

/// An agent already carries a complete TLS configuration. Applying a second
/// one on top can only mean discarding one of them, and the one that would be
/// discarded is a certificate authority the caller believes is pinned — so it
/// is refused, at the first request, naming both calls.
#[test]
fn with_agent_and_with_tls_together_are_refused_rather_than_resolved() {
    let source = Vault::new("https://127.0.0.1:9", "secret", "myapp/db")
        .with_token("supplied")
        .with_agent(ureq::Agent::new_with_defaults())
        .with_tls(TlsConfig::new().with_ca_certificate_file("/etc/ssl/private-ca.pem"));

    let error = source.fetch().expect_err("both doors were opened");

    assert!(error.to_string().contains("with_agent"), "{error}");
    assert!(error.to_string().contains("with_tls"), "{error}");
    assert!(error.to_string().contains("refused"), "{error}");
}

/// A CA file that is not there must name the path — and be reported at the
/// first request rather than as a panic in a builder chain, which is the
/// laziness every other constructor in this family has.
#[test]
fn a_missing_ca_file_is_reported_at_the_first_request_and_names_the_path() {
    let source = Vault::new("https://127.0.0.1:9", "secret", "myapp/db")
        .with_token("supplied")
        .with_tls(TlsConfig::new().with_ca_certificate_file("/nonexistent/private-ca.pem"));

    let error = source.fetch().expect_err("the CA file is not there");

    assert!(
        error.to_string().contains("/nonexistent/private-ca.pem"),
        "{error}"
    );
    assert!(error.to_string().contains("the CA certificate"), "{error}");
}

/// The sharpest rule in this feature: a private key that will not parse must
/// not put itself into the error explaining that it will not parse. The PEM
/// parser underneath renders the line it choked on, which is why its message
/// is dropped rather than wrapped.
#[test]
fn a_malformed_private_key_never_quotes_itself_into_the_error() {
    const PLANTED: &str = "PLANTED-PRIVATE-KEY-MATERIAL";

    let source = Vault::new("https://127.0.0.1:9", "secret", "myapp/db")
        .with_token("supplied")
        .with_tls(TlsConfig::new().with_client_certificate_pem(
            "-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n",
            format!("-----BEGIN PRIVATE KEY-----\n{PLANTED}\n-----END PRIVATE KEY-----\n"),
        ));

    let error = source.fetch().expect_err("neither half parses");
    let printed = format!("{error} {error:?} {source:?}");

    assert!(
        !printed.contains(PLANTED),
        "the private key reached a diagnostic: {printed}"
    );
    assert!(printed.contains("not PEM-encoded"), "{printed}");
}

/// Every new error path gets the redaction test every old one has. Vault's
/// credential is a token in a header rather than a password in the address —
/// which is why `describe()` quotes the address whole — so the thing a TLS
/// failure must not disclose is the token, and it must not disclose it from
/// the error, the `Debug` or the source.
#[test]
fn a_tls_failure_never_carries_the_token_out_with_it() {
    let source = Vault::new("https://vault.internal:8200", "secret", "myapp/db")
        .with_token("hunter2-vault-token")
        .with_tls(TlsConfig::new().with_ca_certificate_file("/nonexistent/private-ca.pem"));

    let error = source.fetch().expect_err("the CA file is not there");
    let printed = format!("{error} {error:?} {source:?}");

    assert!(!printed.contains("hunter2"), "{printed}");
    // And it still says what a person needs in order to fix it.
    assert!(printed.contains("/nonexistent/private-ca.pem"), "{printed}");
}

// ---------------------------------------------------------------------------
// Dynamic secrets: the lease, and the two calls that manage it.
// ---------------------------------------------------------------------------

/// [`scripted`], keeping what the client actually sent.
///
/// The lease calls are the first here whose *request* is the thing under
/// test — a renewal that posts the wrong `lease_id` renews somebody else's
/// credential — so the bytes are captured rather than counted.
fn recording(responses: Vec<String>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let recorded = Arc::clone(&seen);
    std::thread::spawn(move || {
        for response in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };

            let mut head = Vec::new();
            let mut byte = [0u8; 1];

            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                head.push(byte[0]);
            }

            // However long the body says it is, exactly — reading to the end
            // of the stream would block on a connection the client keeps.
            let text = String::from_utf8_lossy(&head).into_owned();
            let length: usize = text
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length: ")
                        .or_else(|| line.strip_prefix("content-length: "))
                })
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);

            let mut body = vec![0u8; length];
            let _ = stream.read_exact(&mut body);

            recorded
                .lock()
                .unwrap()
                .push(text + &String::from_utf8_lossy(&body));

            let _ = stream.write_all(response.as_bytes());
        }
    });

    (address, seen)
}

/// What a dynamic engine answers: values at `data`, the lease beside them.
const ISSUED: &str = r#"{
    "lease_id": "database/creds/app/1a2b",
    "lease_duration": 3600,
    "renewable": true,
    "data": {"username": "v-root-app-xyz", "password": "A1b2C3"}
}"#;

#[test]
fn a_dynamic_read_takes_the_values_from_data_and_keeps_the_lease() {
    let (address, seen) = recording(vec![http("200 OK", ISSUED)]);

    let source = Vault::new(&address, "database", "creds/app")
        .with_token("root")
        .with_dynamic()
        .with_key("db");

    let document = source.fetch().expect("the engine issues a credential");

    // Read at the path itself: a dynamic engine has no `data/` segment, and
    // asking for one is how a working mount answers 404.
    let request = &seen.lock().unwrap()[0];
    assert!(request.contains("GET /v1/database/creds/app "), "{request}");

    assert!(document.text.contains("v-root-app-xyz"));

    let lease = document.lease.expect("a dynamic secret carries its lease");

    assert_eq!(lease.id, "database/creds/app/1a2b");
    assert_eq!(lease.ttl, Duration::from_secs(3600));
    assert!(lease.renewable);
}

/// `ureq` can turn verification off and cannot check a *different* name, so
/// a server name is refused by name rather than approximated with
/// `use_sni(false)` — which would connect, look like it worked, and check
/// nothing about who answered.
#[test]
fn a_server_name_is_refused_here_and_says_why() {
    let source = Vault::new("https://10.0.0.5:8200", "secret", "myapp/db")
        .with_token("root")
        .with_tls(dynamic_config_vault::TlsConfig::new().with_server_name("vault.internal"));

    let error = source
        .fetch()
        .expect_err("this client has no name override")
        .to_string();

    assert!(error.contains("vault.internal"), "{error}");
    assert!(error.contains("refused rather than ignored"), "{error}");
}

/// The SOPS shape, end to end: a document whose passwords are ciphertext
/// arrives readable, and everything around them is untouched.
#[test]
fn transit_decrypts_the_sealed_values_and_leaves_the_rest_alone() {
    use dynamic_config_store_core::transform::Transform as _;

    // Two distinct ciphertexts, one of them named twice in the document.
    let (address, seen) = recording(vec![http(
        "200 OK",
        r#"{"data": {"batch_results": [
            {"plaintext": "aHVudGVyMg=="},
            {"plaintext": "cy0zY3ItM3Q="}
        ]}}"#,
    )]);

    let transit = dynamic_config_vault::Transit::new(
        Vault::new(&address, "secret", "unused").with_token("root"),
        "transit",
        "config",
    );

    let sealed = "host: db.internal
                  port: 5432
                  password: vault:v1:AAAA
                  url: pg://app:vault:v1:AAAA@db.internal/app
                  api_key: vault:v1:BBBB
";

    let readable = transit.apply(sealed).expect("the values decrypt");

    assert!(readable.contains("password: hunter2"), "{readable}");
    assert!(
        readable.contains("pg://app:hunter2@db.internal/app"),
        "the same ciphertext is replaced everywhere it appears: {readable}"
    );
    assert!(readable.contains("api_key: s-3cr-3t"), "{readable}");

    // Everything that was not sealed is byte-for-byte what it was.
    assert!(readable.contains("host: db.internal"));
    assert!(readable.contains("port: 5432"));
    assert!(!readable.contains("vault:v1:"));

    // One request for both values, not one per value.
    assert_eq!(seen.lock().unwrap().len(), 1);

    let request = &seen.lock().unwrap()[0];
    assert!(
        request.contains("POST /v1/transit/decrypt/config "),
        "{request}"
    );
    assert!(request.contains("batch_input"), "{request}");
}

/// A document with nothing sealed in it costs no request at all, which is
/// what makes this safe to leave in front of a store whose documents are
/// only sometimes encrypted.
#[test]
fn a_document_with_nothing_sealed_makes_no_request() {
    use dynamic_config_store_core::transform::Transform as _;

    let (address, seen) = recording(vec![]);

    let transit = dynamic_config_vault::Transit::new(
        Vault::new(&address, "secret", "unused").with_token("root"),
        "transit",
        "config",
    );

    let plain = "host: db.internal
port: 5432
";

    assert_eq!(transit.apply(plain).expect("nothing to do"), plain);
    assert!(seen.lock().unwrap().is_empty(), "no request was made");
}

/// Vault reports a per-entry failure inside a 200, so a batch where one key
/// is wrong looks like a success until the entry is read.
#[test]
fn a_key_that_cannot_decrypt_fails_the_document() {
    use dynamic_config_store_core::transform::Transform as _;

    let (address, _seen) = recording(vec![http(
        "200 OK",
        r#"{"data": {"batch_results": [
            {"error": "unable to decrypt: invalid ciphertext version"}
        ]}}"#,
    )]);

    let transit = dynamic_config_vault::Transit::new(
        Vault::new(&address, "secret", "unused").with_token("root"),
        "transit",
        "config",
    );

    let error = transit
        .apply(
            "password: vault:v1:AAAA
",
        )
        .expect_err("the entry carries the failure")
        .to_string();

    assert!(error.contains("could not be decrypted"), "{error}");
    assert!(error.contains("invalid ciphertext version"), "{error}");

    // Vault's sentence is about a key and a version, and quoting it is
    // safe — but the document must not travel with it.
    assert!(!error.contains("vault:v1:AAAA"), "{error}");
}

/// A certificate's own expiry binds the lease, when it is the shorter one.
///
/// A PKI role can hand back a lease longer than the certificate it issued:
/// the lease is Vault's accounting record, and `notAfter` is what a TLS
/// peer enforces. Re-issuing on the lease's clock would leave a pod holding
/// a certificate nothing will accept, which reads as a network fault rather
/// than as an expiry.
#[test]
fn a_certificate_that_expires_before_its_lease_shortens_it() {
    let expiration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;

    let (address, _seen) = recording(vec![http(
        "200 OK",
        &format!(
            r#"{{"lease_id": "pki/issue/web/9f8e", "lease_duration": 3600,
                 "renewable": false,
                 "data": {{"certificate": "-----BEGIN CERTIFICATE-----",
                           "expiration": {expiration}}}}}"#
        ),
    )]);

    let source = Vault::new(&address, "pki", "issue/web")
        .with_token("root")
        .with_dynamic()
        .with_key("tls");

    let lease = source
        .fetch()
        .expect("the engine issues a certificate")
        .lease
        .expect("a certificate carries its lease");

    assert!(
        lease.ttl <= Duration::from_secs(600),
        "the certificate has ten minutes and the lease claimed an hour: {:?}",
        lease.ttl
    );
    assert!(
        lease.ttl > Duration::from_secs(540),
        "and it is the certificate's own number, not a floor: {:?}",
        lease.ttl
    );
    assert!(
        !lease.renewable,
        "pki/issue does not renew — a certificate is re-issued, never extended"
    );
}

/// An `expiration` in the past is a skewed clock, not an expired
/// certificate — Vault would not have issued one. Believing it would
/// schedule the next re-issue immediately and turn one pod into a fetch
/// loop, so the lease's own number wins.
#[test]
fn an_expiration_already_past_falls_back_to_the_lease() {
    let (address, _seen) = recording(vec![http(
        "200 OK",
        r#"{"lease_id": "pki/issue/web/9f8e", "lease_duration": 3600,
            "renewable": false,
            "data": {"certificate": "-----BEGIN CERTIFICATE-----",
                     "expiration": 1000000000}}"#,
    )]);

    let source = Vault::new(&address, "pki", "issue/web")
        .with_token("root")
        .with_dynamic()
        .with_key("tls");

    let lease = source.fetch().expect("issued").lease.expect("leased");

    assert_eq!(lease.ttl, Duration::from_secs(3600));
}

/// A database credential has no `expiration`, and its lease is the only
/// clock there is. The certificate rule must not reach it.
#[test]
fn a_credential_without_an_expiration_keeps_its_lease_duration() {
    let (address, _seen) = recording(vec![http("200 OK", ISSUED)]);

    let source = Vault::new(&address, "database", "creds/app")
        .with_token("root")
        .with_dynamic()
        .with_key("db");

    let lease = source.fetch().expect("issued").lease.expect("leased");

    assert_eq!(lease.ttl, Duration::from_secs(3600));
}

/// The read is at `{mount}/{path}`, not `{mount}/data/{path}` — which is
/// the whole difference between the two modes, and the one worth pinning.
#[test]
fn a_kv2_read_is_still_nested_under_data() {
    let (address, seen) = recording(vec![http(
        "200 OK",
        r#"{"data": {"data": {"host": "db"}, "metadata": {"version": 4}}}"#,
    )]);

    let source = Vault::new(&address, "secret", "myapp/db").with_token("root");

    source.fetch().expect("a KV v2 read");

    let request = &seen.lock().unwrap()[0];
    assert!(
        request.contains("GET /v1/secret/data/myapp/db "),
        "{request}"
    );
}

#[test]
fn a_renewal_names_the_lease_and_answers_with_what_was_granted() {
    use dynamic_config::{Lease, RenewableSource};

    // Vault granted less than was asked for — a role's `max_ttl` is a
    // ceiling the caller cannot see, so the answer is authoritative.
    let (address, seen) = recording(vec![http(
        "200 OK",
        r#"{"lease_id": "database/creds/app/1a2b", "lease_duration": 900, "renewable": true}"#,
    )]);

    let source = Vault::new(&address, "database", "creds/app")
        .with_token("root")
        .with_dynamic();

    let held = Lease {
        id: "database/creds/app/1a2b".to_owned(),
        ttl: Duration::from_secs(3600),
        renewable: true,
    };

    let granted = source.renew(&held).expect("the renewal succeeds");

    let request = &seen.lock().unwrap()[0];

    assert!(request.contains("POST /v1/sys/leases/renew "), "{request}");
    assert!(request.contains("database/creds/app/1a2b"), "{request}");

    assert_eq!(
        granted.ttl,
        Duration::from_secs(900),
        "the next renewal is scheduled from what Vault granted, never from \
         what was asked for"
    );
}

/// Vault answers a revocation with 204 and no body at all. "It worked" is
/// not a parse failure.
#[test]
fn a_revocation_succeeds_on_an_empty_answer() {
    use dynamic_config::{Lease, RenewableSource};

    let (address, seen) = recording(vec![
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
    ]);

    let source = Vault::new(&address, "database", "creds/app")
        .with_token("root")
        .with_dynamic();

    source
        .revoke(&Lease {
            id: "database/creds/app/1a2b".to_owned(),
            ttl: Duration::from_secs(3600),
            renewable: true,
        })
        .expect("204 is how Vault says the lease is gone");

    let request = &seen.lock().unwrap()[0];
    assert!(request.contains("POST /v1/sys/leases/revoke "), "{request}");
}

/// The invariant that matters most here: a dynamic secret's body holds the
/// generated username and password, and a failure must not quote it.
#[test]
fn no_part_of_a_dynamic_response_reaches_an_error() {
    let (address, _seen) = recording(vec![http("500 Internal Server Error", ISSUED)]);

    let source = Vault::new(&address, "database", "creds/app")
        .with_token("root")
        .with_dynamic();

    let error = source.fetch().expect_err("a 500 is a failure").to_string();

    assert!(!error.contains("v-root-app-xyz"), "{error}");
    assert!(!error.contains("A1b2C3"), "{error}");
    assert!(!error.contains("1a2b"), "{error}");
}

/// An unleased engine — `transit`, or a KV v1 mount read this way — answers
/// without a `lease_id`. That is a document, not a credential, and pretending
/// it had a lease would schedule renewals of nothing.
#[test]
fn a_response_without_a_lease_id_carries_no_lease() {
    let (address, _seen) = recording(vec![http(
        "200 OK",
        r#"{"lease_id": "", "lease_duration": 0, "renewable": false, "data": {"host": "db"}}"#,
    )]);

    let source = Vault::new(&address, "kv", "myapp")
        .with_token("root")
        .with_dynamic();

    let document = source.fetch().expect("the read succeeds");

    assert!(document.lease.is_none());
}

/// A deleted secret is `Absent`, not `Remote`. The difference is the whole
/// point: waiting cures an unreachable store and does not bring back a key,
/// and calling the second one the first is how a stale document goes on
/// being served with every health check reporting fine.
#[test]
fn a_deleted_secret_is_absent_rather_than_an_outage() {
    use dynamic_config::ErrorKind;

    let (address, _seen) = recording(vec![http(
        "200 OK",
        r#"{"data": {"data": null, "metadata": {"version": 7, "destroyed": false}}}"#,
    )]);

    let source = Vault::new(&address, "secret", "myapp/db").with_token("root");

    let error = source.fetch().expect_err("a deleted version has no values");

    assert_eq!(error.kind(), ErrorKind::Absent, "{error}");
}

#[test]
fn a_404_is_absent_too() {
    use dynamic_config::ErrorKind;

    let (address, _seen) = recording(vec![http("404 Not Found", r#"{"errors": []}"#)]);

    let source = Vault::new(&address, "secret", "myapp/db").with_token("root");

    let error = source.fetch().expect_err("nothing is there");

    assert_eq!(error.kind(), ErrorKind::Absent, "{error}");
}

/// The two modes name each other in their refusals, because reading a KV
/// mount as dynamic (or the reverse) is the mistake the explicit builder
/// exists to make visible.
#[test]
fn reading_a_kv2_mount_as_dynamic_says_which_builder_to_drop() {
    let (address, _seen) = recording(vec![http("200 OK", r#"{"lease_id": ""}"#)]);

    let source = Vault::new(&address, "secret", "myapp/db")
        .with_token("root")
        .with_dynamic();

    let error = source.fetch().expect_err("no `data` member").to_string();

    assert!(error.contains("with_dynamic()"), "{error}");
}

#[test]
fn a_dynamic_source_reports_interval_because_there_is_no_counter_to_poll() {
    use dynamic_config::WatchCapability;

    let dynamic = Vault::new("http://127.0.0.1:1", "database", "creds/app").with_dynamic();
    let kv2 = Vault::new("http://127.0.0.1:1", "secret", "myapp/db");

    assert_eq!(dynamic.watch_capability(), WatchCapability::Interval);
    assert_eq!(kv2.watch_capability(), WatchCapability::Conditional);
}

/// One credential per source, said out loud. Merging several dynamic reads
/// would leave every lease but one unrenewed and unrevoked — a credential
/// leak wearing a merge's clothes.
#[test]
fn a_dynamic_source_refuses_several_paths() {
    let source = Vault::new("http://127.0.0.1:1", "database", Keys::several(["a", "b"]))
        .with_token("root")
        .with_dynamic();

    let error = source
        .fetch()
        .expect_err("several dynamic paths")
        .to_string();

    assert!(error.contains("one path"), "{error}");
    assert!(error.contains("lease"), "{error}");
}
