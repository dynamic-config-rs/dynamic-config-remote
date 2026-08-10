//! The retry decision, against a scripted agent.
//!
//! No Docker: these start a `TcpListener`, speak just enough HTTP/1.1 for
//! `ureq`, and count requests. What they pin down is *when a refused request
//! earns a second one* — a decision that once depended on whether the error's
//! text contained `"403"`, which read true for any error mentioning a key
//! like `myapp/403.json`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use base64::Engine;
use dynamic_config::RemoteSource;
use dynamic_config_consul::{Auth, Consul};

/// Serves `responses` in order, one per connection, and counts requests.
///
/// The listener closes after the last scripted response, so a client that
/// asks once too often gets a connection error rather than a hang.
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

            // Read until the end of the request head. The requests here have
            // no body, so the blank line is the whole story.
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

/// A Consul KV answer holding `document`.
fn kv_answer(document: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(document);

    http("200 OK", &format!(r#"[{{"Value": "{encoded}"}}]"#))
}

/// A login answer minting `token`.
fn login_answer(token: &str) -> String {
    http("200 OK", &format!(r#"{{"SecretID": "{token}"}}"#))
}

#[test]
fn a_supplied_token_is_never_retried_because_it_cannot_change() {
    // One response only: if the client asks twice, the second connection
    // fails and the count still tells the story.
    let (address, requests, server) = scripted(vec![http("403 Forbidden", "Permission denied")]);

    let source = Consul::new(&address, "myapp/db.json").with_auth(Auth::token("supplied"));
    let error = source.fetch().expect_err("the agent said 403");

    server.join().unwrap();

    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "invalidating a supplied token just retries the identical string; \
         the second request is pure waste: {error}"
    );
}

#[test]
fn a_500_on_a_key_named_403_is_not_mistaken_for_a_refused_token() {
    // The trap: `describe()` puts the key in every error message, so a key
    // named `403.json` makes every error's *text* contain "403". Only the
    // typed status may drive the retry.
    let (address, requests, server) = scripted(vec![http("500 Internal Server Error", "boom")]);

    let source = Consul::new(&address, "myapp/403.json").with_auth(Auth::jwt("jwt-auth", "token"));
    let error = source.fetch().expect_err("the agent failed");

    server.join().unwrap();

    assert!(error.to_string().contains("403.json"), "{error}");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "a 500 is not an auth failure, whatever the key is called"
    );
}

#[test]
fn a_refused_login_token_is_traded_for_a_fresh_one_exactly_once() {
    let (address, requests, server) = scripted(vec![
        // First read: login, then the read is refused.
        login_answer("stale"),
        http("403 Forbidden", "Permission denied"),
        // The retry: a fresh login, then the read succeeds.
        login_answer("fresh"),
        kv_answer(r#"{"db": {"host": "a"}}"#),
    ]);

    let source = Consul::new(&address, "myapp/db.json").with_auth(Auth::jwt("jwt-auth", "token"));
    let fetched = source.fetch().expect("the second token works");

    server.join().unwrap();

    assert!(fetched.text.contains("host"), "{}", fetched.text);
    assert_eq!(requests.load(Ordering::SeqCst), 4);
}

#[test]
fn an_unreachable_agent_is_a_prompt_error_naming_the_address() {
    // Port 9 is discard; nothing listens there.
    let source = Consul::new("http://127.0.0.1:9", "myapp/db.json");

    let error = source.fetch().expect_err("nothing is listening");

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("127.0.0.1:9"), "{error}");
}
