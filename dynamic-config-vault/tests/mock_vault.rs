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
use dynamic_config_vault::Vault;

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

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("127.0.0.1:9"), "{error}");
}
