//! The retry decision and the missing-`updateTime` refusal, against a
//! scripted server.
//!
//! No Docker: these start a `TcpListener`, speak just enough HTTP/1.1 for
//! `ureq`, and count requests. They pin down decisions the emulator tests
//! cannot see: when a refused request earns a second one, and that a document
//! with no `updateTime` ends a watch instead of silently never firing.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dynamic_config::{RemoteSource, RemoteWatch};
use dynamic_config_firestore::{Auth, Firestore};

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
fn a_supplied_access_token_is_never_retried_because_it_cannot_change() {
    let (address, requests, server) = scripted(vec![http(
        "401 Unauthorized",
        r#"{"error": {"code": 401}}"#,
    )]);

    let source = Firestore::new("my-project", "config/db")
        .with_endpoint(&address)
        .with_auth(Auth::access_token("supplied"));

    let error = source.fetch().expect_err("the server said 401");

    server.join().unwrap();

    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "a supplied access token cannot be refreshed; the second request \
         is pure waste: {error}"
    );
}

#[test]
fn a_500_on_a_path_named_401_is_not_mistaken_for_an_expired_token() {
    // `describe()` puts the path in every error message, so a document named
    // `config/401` makes every error's *text* contain "401". Only the typed
    // status may drive the retry.
    //
    // The auth is a mocked metadata server — the one method that *can*
    // refresh — so a misclassification would really produce a second request.
    let (metadata, _tokens, token_server) = scripted(vec![http(
        "200 OK",
        r#"{"access_token": "minted", "expires_in": 3600}"#,
    )]);
    let (address, requests, server) = scripted(vec![http("500 Internal Server Error", "boom")]);

    let source = Firestore::new("my-project", "config/401")
        .with_endpoint(&address)
        .with_auth(Auth::metadata_server().with_url(format!("{metadata}/token")));

    let error = source.fetch().expect_err("the server failed");

    server.join().unwrap();
    token_server.join().unwrap();

    assert!(error.to_string().contains("config/401"), "{error}");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "a 500 is not an auth failure, whatever the document is called"
    );
}

/// A document with no `updateTime` gives a watch nothing to compare, so the
/// watch must end with an error instead of silently never firing.
#[test]
fn a_document_with_no_update_time_ends_the_watch_with_an_error() {
    let (address, _requests, server) = scripted(vec![http(
        "200 OK",
        r#"{"fields": {"host": {"stringValue": "db"}}}"#,
    )]);

    let source = Firestore::new("my-project", "config/db").with_endpoint(&address);

    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let error = source
        .watch(&watching, Duration::from_millis(100), |_| Ok(()))
        .expect_err("a document with no updateTime cannot be watched");

    server.join().unwrap();

    assert!(error.to_string().contains("updateTime"), "{error}");
}

#[test]
fn an_unreachable_server_is_a_prompt_error_naming_the_endpoint() {
    let source = Firestore::new("my-project", "config/db").with_endpoint("http://127.0.0.1:9");

    let error = source.fetch().expect_err("nothing is listening");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("127.0.0.1:9"), "{error}");
}
