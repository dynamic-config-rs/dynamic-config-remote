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
use dynamic_config_firestore::{Auth, Firestore, Keys};

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

    assert_eq!(
        error.kind(),
        dynamic_config::ErrorKind::Remote,
        "a server that is down comes back; a watch loop must back off rather \
         than stop, which is what `Auth` would tell it to do"
    );
    assert!(error.to_string().contains("127.0.0.1:9"), "{error}");
}

/// A rejected access token is `Auth`, not `Remote` — the distinction a caller
/// branches on to page somebody instead of retrying forever.
#[test]
fn a_rejected_token_is_an_auth_failure_that_never_repeats_the_token() {
    let (address, _requests, server) = scripted(vec![http(
        "401 Unauthorized",
        r#"{"error": {"code": 401, "status": "UNAUTHENTICATED"}}"#,
    )]);

    let source = Firestore::new("my-project", "config/db")
        .with_endpoint(&address)
        .with_auth(Auth::access_token("hunter2-access-token"));

    let error = source.fetch().expect_err("the server said 401");

    server.join().unwrap();

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Auth, "{error}");
    let printed = format!("{error} {error:?} {source:?}");
    assert!(!printed.contains("hunter2"), "{printed}");
}

/// A 403 is `PERMISSION_DENIED` here — the token is fine and the identity
/// behind it is not allowed to read the document. Minting another token is
/// the same identity, so it is still an auth failure to the caller.
#[test]
fn a_forbidden_document_is_an_auth_failure_too() {
    let (address, requests, server) = scripted(vec![http(
        "403 Forbidden",
        r#"{"error": {"code": 403, "status": "PERMISSION_DENIED"}}"#,
    )]);

    let source = Firestore::new("my-project", "config/db")
        .with_endpoint(&address)
        .with_auth(Auth::access_token("supplied"));

    let error = source.fetch().expect_err("the server said 403");

    server.join().unwrap();

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Auth, "{error}");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "a fresh token is the same identity, so a 403 earns no second request"
    );
}

/// Exhausted quota is a 429 and comes right on its own, so it stays `Remote`.
/// This is the over-classification the 403 arm is one status away from.
#[test]
fn exhausted_quota_is_not_an_auth_failure() {
    let (address, _requests, server) = scripted(vec![http(
        "429 Too Many Requests",
        r#"{"error": {"code": 429, "status": "RESOURCE_EXHAUSTED"}}"#,
    )]);

    let source = Firestore::new("my-project", "config/db")
        .with_endpoint(&address)
        .with_auth(Auth::access_token("supplied"));

    let error = source.fetch().expect_err("the quota is spent");

    server.join().unwrap();

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote, "{error}");
}

// ---------------------------------------------------------------------------
// Several documents as one section
// ---------------------------------------------------------------------------

/// A `:batchGet` result for a document that exists.
fn found(path: &str, fields: &str) -> String {
    format!(
        r#"{{"found": {{"name": "projects/my-project/databases/(default)/documents/{path}", "fields": {fields}, "updateTime": "2026-01-01T00:00:00Z"}}}}"#
    )
}

/// A source over `paths`, pointed at a scripted server.
fn several(address: &str, paths: [&str; 2]) -> Firestore {
    Firestore::new("my-project", Keys::several(paths))
        .with_endpoint(address)
        .with_auth(Auth::access_token("supplied"))
}

/// The rule a list inherits from a list of files: call order, later wins. One
/// request, because `:batchGet` is Firestore's own answer to a set.
#[test]
fn several_documents_are_one_request_and_merge_in_call_order() {
    let (address, requests, server) = scripted(vec![http(
        "200 OK",
        &format!(
            "[{}, {}]",
            found(
                "config/db",
                r#"{"host": {"stringValue": "shared"}, "port": {"integerValue": "5432"}}"#
            ),
            found("overrides/db", r#"{"host": {"stringValue": "override"}}"#),
        ),
    )]);

    let source = several(&address, ["config/db", "overrides/db"]);

    let document = source.fetch().expect("both documents came back");

    server.join().unwrap();

    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "`:batchGet` reads the set in one round trip"
    );

    let merged: serde_json::Value =
        serde_json::from_str(&document.text).expect("the merged section is JSON");

    assert_eq!(merged["db"]["host"], "override", "the later document wins");
    assert_eq!(
        merged["db"]["port"], 5432,
        "a field only the earlier document has survives the merge"
    );
}

/// `BatchGetDocuments` states that results do not come back in the order they
/// were asked for. The caller's order is the precedence, so it is restored —
/// otherwise which value wins would be a property of the service's mood.
#[test]
fn a_batch_answered_out_of_order_still_merges_in_call_order() {
    let (address, _requests, server) = scripted(vec![http(
        "200 OK",
        &format!(
            "[{}, {}]",
            found("overrides/db", r#"{"host": {"stringValue": "override"}}"#),
            found("config/db", r#"{"host": {"stringValue": "shared"}}"#),
        ),
    )]);

    let source = several(&address, ["config/db", "overrides/db"]);

    let document = source.fetch().expect("both documents came back");

    server.join().unwrap();

    let merged: serde_json::Value = serde_json::from_str(&document.text).unwrap();

    assert_eq!(
        merged["db"]["host"], "override",
        "call order decides, not reply order"
    );
}

/// One missing document fails the whole fetch, naming it — and the report says
/// nothing about what the document that *did* answer was holding.
#[test]
fn a_missing_document_fails_the_whole_fetch_and_never_prints_a_value() {
    let (address, _requests, server) = scripted(vec![http(
        "200 OK",
        &format!(
            r#"[{}, {{"missing": "projects/my-project/databases/(default)/documents/overrides/db", "readTime": "2026-01-01T00:00:00Z"}}]"#,
            found(
                "config/db",
                r#"{"password": {"stringValue": "hunter2-firestore"}}"#
            ),
        ),
    )]);

    let source = several(&address, ["config/db", "overrides/db"]);

    let error = source
        .fetch()
        .expect_err("the second document is not there");

    server.join().unwrap();

    let printed = format!("{error} {error:?} {source:?}");

    assert!(printed.contains("overrides/db"), "{printed}");
    assert!(printed.contains("holds no document"), "{printed}");
    assert!(
        !printed.contains("hunter2"),
        "a value that was read must never reach a diagnostic: {printed}"
    );
}

/// A server can answer with documents nobody asked about. Merging one would
/// put a stranger's fields into this configuration, so it is refused.
#[test]
fn a_document_nobody_asked_for_is_refused_rather_than_merged() {
    let (address, _requests, server) = scripted(vec![http(
        "200 OK",
        &format!(
            "[{}, {}]",
            found("config/db", r#"{"host": {"stringValue": "shared"}}"#),
            found("somebody/else", r#"{"host": {"stringValue": "elsewhere"}}"#),
        ),
    )]);

    let source = several(&address, ["config/db", "overrides/db"]);

    let error = source.fetch().expect_err("that document was not asked for");

    server.join().unwrap();

    assert!(error.to_string().contains("somebody/else"), "{error}");
    assert!(
        error.to_string().contains("not one of the documents"),
        "{error}"
    );
}

/// A server can also answer for fewer documents than it was asked about,
/// without saying any are missing. Merging what came back would be a section
/// quietly missing half of itself.
#[test]
fn a_document_the_store_says_nothing_at_all_about_fails_the_fetch() {
    let (address, _requests, server) = scripted(vec![http(
        "200 OK",
        &format!(
            "[{}]",
            found("config/db", r#"{"host": {"stringValue": "a"}}"#)
        ),
    )]);

    let source = several(&address, ["config/db", "overrides/db"]);

    let error = source.fetch().expect_err("one document was never answered");

    server.join().unwrap();

    assert!(error.to_string().contains("overrides/db"), "{error}");
    assert!(error.to_string().contains("nothing at all"), "{error}");
}

/// The `updateTime` a watch compares belongs to one document; a set of them
/// has none. Refused at `watch`, so it fails now rather than by never firing.
#[test]
fn a_multi_document_source_refuses_to_be_watched_and_says_what_to_do_instead() {
    let source = several("http://127.0.0.1:9", ["config/db", "overrides/db"]);

    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let error = source
        .watch(&watching, Duration::from_millis(50), |_| Ok(()))
        .expect_err("a set of documents has no updateTime of its own");

    assert!(error.to_string().contains("several documents"), "{error}");
    assert!(error.to_string().contains("refresh_remote"), "{error}");
}

/// The metadata server refusing to mint a token is a credential that could
/// not be obtained — `Auth`. Being unable to *reach* it would be `Remote`.
#[test]
fn a_metadata_server_that_refuses_is_an_auth_failure() {
    let (metadata, _tokens, token_server) =
        scripted(vec![http("403 Forbidden", r#"{"error": "no identity"}"#)]);

    let source = Firestore::new("my-project", "config/db")
        .with_endpoint("http://127.0.0.1:9")
        .with_auth(Auth::metadata_server().with_url(format!("{metadata}/token")));

    let error = source.fetch().expect_err("the metadata server refused");

    token_server.join().unwrap();

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Auth, "{error}");
}

// ---------------------------------------------------------------------------
// Reporting what the watch loop cannot otherwise say.
//
// This watch is a poll: a document that has not changed and a project that
// stopped answering deliver exactly the same thing, which is nothing. `apply`
// records a delivery and there was never anything to record a failed tick —
// which left `remote_up` reporting the last delivery rather than the last
// attempt. These prove the other half.
// ---------------------------------------------------------------------------

use dynamic_config::dynamic_config;
use serde::Deserialize;

/// One config type per test, as the repository's own rule requires: the
/// snapshot, the source and the status all live in `static`s keyed by the type.
#[dynamic_config]
#[derive(Debug, Deserialize)]
struct UnreachableDb {
    host: String,
}

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct TimelessDb {
    host: String,
}

/// A document holding one field, read at `updated`.
fn document_at(updated: &str) -> String {
    http(
        "200 OK",
        &format!(r#"{{"fields": {{"host": {{"stringValue": "a"}}}}, "updateTime": "{updated}"}}"#),
    )
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

/// The claim in one test: a project that answered once and then went away is
/// **visible** — `reachable()` goes to `Some(false)` — and the staleness clock
/// keeps its value, because how long the served document has been stale is the
/// other half of what an alert needs.
#[test]
fn a_watch_whose_poll_fails_reports_it_without_resetting_the_fetch_clock() {
    // One scripted answer, for the fetch that establishes a healthy store. The
    // listener closes behind it, so every poll the watch then makes is refused.
    let (address, _requests, server) = scripted(vec![document_at("2026-01-01T00:00:00Z")]);

    UnreachableDb::set_remote(Firestore::new("my-project", "config/db").with_endpoint(&address));
    UnreachableDb::refresh_remote().expect("the store answers the first fetch");
    UnreachableDb::builder("db")
        .init()
        .expect("the fetched document is the whole configuration");

    server.join().unwrap();

    // Taken once, where the loop is wired — which is also the only handle a
    // caller has on the status behind it.
    let sink = UnreachableDb::remote_sink();
    let answering = sink.status();

    assert_eq!(answering.reachable(), Some(true), "the fetch answered");

    let watcher = Firestore::new("my-project", "config/db")
        .with_endpoint(&address)
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
        "a failed poll is waited out, not returned: {outcome:?}"
    );

    assert_eq!(
        failing.reachable(),
        Some(false),
        "a poll that cannot reach the store must not go on reporting the \
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
        UnreachableDb::current().host,
        "a",
        "and the document the store did answer with is still serving: a \
         failed attempt is no reason to stop serving the last good one"
    );
}

/// A document that comes back without an `updateTime` ends the watch — and is
/// recorded on the way out. A watch that has ended is a configuration that has
/// stopped updating for good, which is the last thing that should read as a
/// healthy store.
#[test]
fn a_document_with_no_update_time_reports_the_failure_that_ends_the_watch() {
    let (address, _requests, server) = scripted(vec![
        // The fetch.
        document_at("2026-01-01T00:00:00Z"),
        // Then a document with nothing to compare.
        http("200 OK", r#"{"fields": {"host": {"stringValue": "a"}}}"#),
    ]);

    TimelessDb::set_remote(Firestore::new("my-project", "config/db").with_endpoint(&address));
    TimelessDb::refresh_remote().expect("the store answers the first fetch");
    TimelessDb::builder("db")
        .init()
        .expect("the fetched document is the whole configuration");

    let sink = TimelessDb::remote_sink();
    let answering = sink.status();

    let watcher = Firestore::new("my-project", "config/db")
        .with_endpoint(&address)
        .reporting_to(sink);

    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let error = watcher
        .watch(&watching, Duration::from_millis(50), |_| Ok(()))
        .expect_err("a document with no `updateTime` cannot be watched");

    server.join().unwrap();

    assert!(error.to_string().contains("updateTime"), "{error}");

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
    assert_eq!(TimelessDb::current().host, "a");
}

// ---------------------------------------------------------------------------
// TLS: the shared vocabulary, and where it refuses.
//
// The translation into `ureq`'s own types is unit-tested in `src/tls.rs`,
// against a certificate authority generated there. What belongs here is what
// only the source can answer: when the refusal happens, and what it says.
// ---------------------------------------------------------------------------

use dynamic_config_firestore::TlsConfig;

/// An agent already carries a complete TLS configuration. Applying a second
/// one on top can only mean discarding one of them, and the one that would be
/// discarded is a certificate authority the caller believes is pinned — so it
/// is refused, at the first request, naming both calls.
#[test]
fn with_agent_and_with_tls_together_are_refused_rather_than_resolved() {
    let source = Firestore::new("my-project", "config/db")
        .with_endpoint("https://127.0.0.1:9")
        .with_agent(ureq::Agent::new_with_defaults())
        .with_tls(TlsConfig::new().with_ca_certificate_file("/etc/ssl/private-ca.pem"));

    let error = source.fetch().expect_err("both doors were opened");

    assert!(error.to_string().contains("with_agent"), "{error}");
    assert!(error.to_string().contains("with_tls"), "{error}");
    assert!(error.to_string().contains("refused"), "{error}");
}

/// A CA file that is not there is reported at the first request, naming the
/// path — not as a panic in a builder chain.
#[test]
fn a_missing_ca_file_is_reported_at_the_first_request_and_names_the_path() {
    let source = Firestore::new("my-project", "config/db")
        .with_endpoint("https://127.0.0.1:9")
        .with_tls(TlsConfig::new().with_ca_certificate_file("/nonexistent/private-ca.pem"));

    let error = source.fetch().expect_err("the CA file is not there");

    assert!(
        error.to_string().contains("/nonexistent/private-ca.pem"),
        "{error}"
    );
    assert!(error.to_string().contains("the CA certificate"), "{error}");
}

/// Every new error path gets the redaction test every old one has. Firestore's
/// credential is an access token in a header, so what a TLS failure must not
/// disclose is that token — from the error, the `Debug` or the source.
#[test]
fn a_tls_failure_never_carries_the_access_token_out_with_it() {
    let source = Firestore::new("my-project", "config/db")
        .with_endpoint("https://127.0.0.1:9")
        .with_auth(Auth::access_token("hunter2-gcp-token"))
        .with_tls(TlsConfig::new().with_ca_certificate_file("/nonexistent/private-ca.pem"));

    let error = source.fetch().expect_err("the CA file is not there");
    let printed = format!("{error} {error:?} {source:?}");

    assert!(!printed.contains("hunter2"), "{printed}");
    assert!(printed.contains("/nonexistent/private-ca.pem"), "{printed}");
}

/// The private key is the sharpest secret in this feature, and the parser
/// underneath renders the line it choked on — so a key that will not parse
/// must not put itself into the error explaining that it will not parse.
#[test]
fn a_malformed_private_key_never_quotes_itself_into_the_error() {
    const PLANTED: &str = "PLANTED-PRIVATE-KEY-MATERIAL";

    let source = Firestore::new("my-project", "config/db")
        .with_endpoint("https://127.0.0.1:9")
        .with_tls(TlsConfig::new().with_client_certificate_pem(
            "-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n",
            format!("-----BEGIN PRIVATE KEY-----\n{PLANTED}\n-----END PRIVATE KEY-----\n"),
        ));

    let error = source.fetch().expect_err("neither half parses");
    let printed = format!("{error} {error:?} {source:?}");

    assert!(!printed.contains(PLANTED), "{printed}");
    assert!(printed.contains("not PEM-encoded"), "{printed}");
}
