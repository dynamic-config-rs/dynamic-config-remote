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
use dynamic_config::{Format, RemoteSource, Value};
use dynamic_config_consul::{Auth, Consul, Keys};

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

/// As [`scripted`], but keeping every request line the agent was sent.
///
/// The multi-key reads are the first here whose *shape* is the thing under
/// test — one request per named key, one `?recurse` request for a prefix — so
/// the URL has to be visible, not just the count.
fn scripted_seen(
    responses: Vec<String>,
) -> (
    String,
    Arc<std::sync::Mutex<Vec<String>>>,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let asked: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();

    let seen = Arc::clone(&asked);
    let server = std::thread::spawn(move || {
        for response in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };

            let mut head = Vec::new();
            let mut byte = [0u8; 1];

            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                head.push(byte[0]);
            }

            let head = String::from_utf8_lossy(&head).to_string();
            let line = head.lines().next().unwrap_or_default().to_owned();

            seen.lock().unwrap().push(line);

            let _ = stream.write_all(response.as_bytes());
        }
    });

    (address, asked, server)
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

/// A Consul KV answer holding every `(key, document)` pair — the shape a
/// `?recurse` read comes back in.
fn kv_entries(pairs: &[(&str, &str)]) -> String {
    let entries: Vec<String> = pairs
        .iter()
        .map(|(key, document)| {
            let encoded = base64::engine::general_purpose::STANDARD.encode(document);

            format!(r#"{{"Key": "{key}", "Value": "{encoded}"}}"#)
        })
        .collect();

    http("200 OK", &format!("[{}]", entries.join(",")))
}

/// The same, carrying the `X-Consul-Index` a blocking query reads to decide
/// what to ask for next. Without it every query would repeat the same index and
/// the loop would never move on.
fn kv_entries_at(index: u64, pairs: &[(&str, &str)]) -> String {
    let entries: Vec<String> = pairs
        .iter()
        .map(|(key, document)| {
            let encoded = base64::engine::general_purpose::STANDARD.encode(document);

            format!(r#"{{"Key": "{key}", "Value": "{encoded}"}}"#)
        })
        .collect();

    let body = format!("[{}]", entries.join(","));

    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\
         X-Consul-Index: {index}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A subtree stamped with one generation in both of its sections.
fn stamped(generation: u64) -> Vec<(String, String)> {
    vec![
        (
            "stamped/db.json".to_owned(),
            format!(r#"{{"db": {{"generation": {generation}}}}}"#),
        ),
        (
            "stamped/server.json".to_owned(),
            format!(r#"{{"server": {{"generation": {generation}}}}}"#),
        ),
    ]
}

/// A `?recurse` answer holding [`stamped`] at `index`.
fn stamped_at(index: u64, generation: u64) -> String {
    let pairs = stamped(generation);
    let borrowed: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(key, document)| (key.as_str(), document.as_str()))
        .collect();

    kv_entries_at(index, &borrowed)
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

    assert_eq!(
        error.kind(),
        dynamic_config::ErrorKind::Remote,
        "an agent that is down comes back; a watch loop must back off rather \
         than stop, which is what `Auth` would tell it to do"
    );
    assert!(error.to_string().contains("127.0.0.1:9"), "{error}");
}

/// A refused ACL token is `Auth`, not `Remote` — the distinction a caller
/// branches on to page somebody instead of retrying forever.
#[test]
fn a_refused_token_is_an_auth_failure_that_never_repeats_the_token() {
    let (address, _requests, server) = scripted(vec![http("403 Forbidden", "Permission denied")]);

    let source = Consul::new(&address, "myapp/db.json").with_auth(Auth::token("hunter2-acl-token"));
    let error = source.fetch().expect_err("the agent said 403");

    server.join().unwrap();

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Auth, "{error}");
    // The rule the whole crate is built around: the credential that was
    // refused must not be quoted back in the complaint about it.
    let printed = format!("{error} {error:?} {source:?}");
    assert!(!printed.contains("hunter2"), "{printed}");
}

/// A 500 stays `Remote`: the agent may be restarting, and a watch loop that
/// stopped for one would be a configuration that stops updating for one.
#[test]
fn a_server_failure_is_not_an_auth_failure() {
    let (address, _requests, server) = scripted(vec![http("500 Internal Server Error", "boom")]);

    let source = Consul::new(&address, "myapp/db.json").with_auth(Auth::token("supplied"));
    let error = source.fetch().expect_err("the agent failed");

    server.join().unwrap();

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote, "{error}");
}

/// The bearer token is refused at the login endpoint, before any read: still
/// a credential that could not be obtained, so still `Auth`.
#[test]
fn a_refused_login_is_an_auth_failure() {
    let (address, _requests, server) = scripted(vec![http("403 Forbidden", "Permission denied")]);

    let source =
        Consul::new(&address, "myapp/db.json").with_auth(Auth::jwt("jwt-auth", "hunter2-bearer"));
    let error = source
        .fetch()
        .expect_err("the agent refused the bearer token");

    server.join().unwrap();

    assert_eq!(error.kind(), dynamic_config::ErrorKind::Auth, "{error}");
    assert!(!format!("{error} {error:?}").contains("hunter2"), "{error}");
}

// ---------------------------------------------------------------------------
// Several keys as one document
// ---------------------------------------------------------------------------

/// A named list is one request per key, in the order the caller wrote them,
/// and the later key wins where two of them meet.
#[test]
fn a_named_list_is_one_request_per_key_in_call_order() {
    let (address, asked, server) = scripted_seen(vec![
        kv_answer(r#"{"db": {"host": "base", "port": 5432}}"#),
        kv_answer(r#"{"db": {"port": 6432}}"#),
    ]);

    let source = Consul::new(
        &address,
        Keys::several(["myapp/base.json", "myapp/local.json"]),
    );
    let fetched = source.fetch().expect("both keys answered");

    server.join().unwrap();

    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(asked[0].contains("myapp/base.json"), "{asked:?}");
    assert!(asked[1].contains("myapp/local.json"), "{asked:?}");
    assert!(
        !asked.iter().any(|line| line.contains("recurse")),
        "a named list is not a subtree read: {asked:?}"
    );

    let merged = Value::parse(&fetched.text, Format::Json).unwrap();

    assert_eq!(
        merged.get("db.host"),
        Some(&Value::String("base".to_owned())),
        "a key the later document never mentions survives"
    );
    assert_eq!(
        merged.get("db.port"),
        Some(&Value::Integer(6432)),
        "and the later document wins where they meet"
    );
}

/// A prefix is one `?recurse` request, not one request per key — the whole
/// reason a prefix is worth having as a separate shape.
#[test]
fn a_prefix_is_one_recursive_request() {
    let (address, asked, server) = scripted_seen(vec![kv_entries(&[
        ("myapp/db.json", r#"{"db": {"host": "db.internal"}}"#),
        ("myapp/server.json", r#"{"server": {"port": 8080}}"#),
    ])]);

    let source = Consul::new(&address, Keys::prefix("myapp/")).with_format(Format::Json);
    let fetched = source.fetch().expect("the subtree answered");

    server.join().unwrap();

    let asked = asked.lock().unwrap();
    assert_eq!(
        asked.len(),
        1,
        "one request for the whole subtree: {asked:?}"
    );
    assert!(asked[0].contains("recurse=true"), "{asked:?}");

    let merged = Value::parse(&fetched.text, Format::Json).unwrap();

    assert_eq!(
        merged.get("db.host"),
        Some(&Value::String("db.internal".to_owned()))
    );
    assert_eq!(merged.get("server.port"), Some(&Value::Integer(8080)));
}

/// A prefix says "these are disjoint sections". Two of them supplying one path
/// is a deployment bug, named rather than silently resolved — and the report
/// names paths, never values.
#[test]
fn two_keys_under_a_prefix_supplying_one_path_are_refused_by_name() {
    let (address, _asked, server) = scripted_seen(vec![kv_entries(&[
        ("myapp/db.json", r#"{"db": {"password": "hunter2-first"}}"#),
        (
            "myapp/extra.json",
            r#"{"db": {"password": "hunter2-second"}}"#,
        ),
    ])]);

    let source = Consul::new(&address, Keys::prefix("myapp/")).with_format(Format::Json);
    let error = source.fetch().expect_err("both keys supply db.password");

    server.join().unwrap();

    let printed = format!("{error} {error:?}");

    assert!(printed.contains("myapp/db.json"), "{printed}");
    assert!(printed.contains("myapp/extra.json"), "{printed}");
    assert!(printed.contains("db.password"), "{printed}");
    assert!(
        !printed.contains("hunter2"),
        "a collision report names paths and never values: {printed}"
    );
}

/// Naming the keys *is* saying which one wins, so the same overlap is fine.
#[test]
fn the_same_overlap_is_fine_once_the_caller_names_an_order() {
    let (address, _asked, server) = scripted_seen(vec![
        kv_answer(r#"{"db": {"host": "first"}}"#),
        kv_answer(r#"{"db": {"host": "second"}}"#),
    ]);

    let source = Consul::new(
        &address,
        Keys::several(["myapp/db.json", "myapp/extra.json"]),
    );
    let fetched = source.fetch().expect("a named order resolves it");

    server.join().unwrap();

    assert!(fetched.text.contains("second"), "{}", fetched.text);
}

/// Fail-whole, not merge-what-came-back: a configuration quietly missing a
/// section is worse than a refresh that failed and left the last one serving.
#[test]
fn one_unreadable_key_fails_the_whole_fetch_and_names_it() {
    let (address, asked, server) = scripted_seen(vec![
        kv_answer(r#"{"db": {"host": "here"}}"#),
        // Consul's answer for a key that is not there.
        http("200 OK", "[]"),
    ]);

    let source = Consul::new(
        &address,
        Keys::several(["myapp/db.json", "myapp/absent.json"]),
    );
    let error = source.fetch().expect_err("the second key is not there");

    server.join().unwrap();

    assert_eq!(asked.lock().unwrap().len(), 2);
    assert_eq!(error.kind(), dynamic_config::ErrorKind::Remote);
    assert!(error.to_string().contains("myapp/absent.json"), "{error}");
    assert!(error.to_string().contains("holds no value"), "{error}");
}

/// Consul's own spelling of a folder is a key ending in `/` with no value,
/// which the UI creates for a directory. It is not a document, and it must not
/// read as one that is missing.
#[test]
fn a_folder_marker_under_the_prefix_is_not_a_missing_document() {
    let (address, _asked, server) = scripted_seen(vec![http(
        "200 OK",
        &format!(
            r#"[{{"Key": "myapp/", "Value": null}},{{"Key": "myapp/db.json", "Value": "{}"}}]"#,
            base64::engine::general_purpose::STANDARD.encode(r#"{"db": {"host": "a"}}"#)
        ),
    )]);

    let source = Consul::new(&address, Keys::prefix("myapp/")).with_format(Format::Json);
    let fetched = source.fetch().expect("a folder marker is skipped");

    server.join().unwrap();

    assert!(fetched.text.contains("host"), "{}", fetched.text);
}

/// A key the agent answers with that is not under the prefix asked for is
/// refused rather than merged. One comparison, and it makes a prefix mean what
/// it says whatever sits in front of the agent.
#[test]
fn a_key_outside_the_prefix_is_refused() {
    let (address, _asked, server) = scripted_seen(vec![kv_entries(&[
        ("myapp/db.json", r#"{"db": {"host": "a"}}"#),
        ("other/db.json", r#"{"db": {"host": "b"}}"#),
    ])]);

    let source = Consul::new(&address, Keys::prefix("myapp/")).with_format(Format::Json);
    let error = source.fetch().expect_err("one key escaped the prefix");

    server.join().unwrap();

    assert!(error.to_string().contains("other/db.json"), "{error}");
}

/// A prefix has no extension to infer a format from, and a list whose keys
/// name two formats has one too many. Both refuse before any round trip, and
/// both name the call that settles it.
#[test]
fn a_format_that_cannot_be_inferred_is_refused_before_any_request() {
    let error = Consul::new("http://127.0.0.1:9", Keys::prefix("myapp/"))
        .fetch()
        .expect_err("a prefix names no format");

    assert!(error.to_string().contains("with_format"), "{error}");

    let error = Consul::new(
        "http://127.0.0.1:9",
        Keys::several(["myapp/db.json", "myapp/server.toml"]),
    )
    .fetch()
    .expect_err("json and toml cannot both be it");

    assert!(error.to_string().contains("myapp/db.json"), "{error}");
    assert!(error.to_string().contains("myapp/server.toml"), "{error}");
}

/// Refused, not approximated: Consul blocks on one key or one subtree, so
/// watching a caller's list would mean waking on one key and reading the rest —
/// the new value of one beside the old value of another.
#[test]
fn a_named_list_refuses_to_be_watched_and_says_what_to_do() {
    let watch = dynamic_config::RemoteWatch::new();

    let source = Consul::new(
        "http://127.0.0.1:9",
        Keys::several(["myapp/db.json", "myapp/server.json"]),
    );

    let error = source
        .watch(&watch.watching(), |_| Ok(()))
        .expect_err("a named list cannot be watched");

    assert!(error.to_string().contains("cannot be watched"), "{error}");
    assert!(error.to_string().contains("refresh_remote"), "{error}");
    assert!(
        error.to_string().contains("watch a prefix"),
        "the refusal must name the shape that does work: {error}"
    );
}

/// The property that makes a prefix watch worth having, proved where a live
/// agent cannot prove it: **one request per delivery**.
///
/// A recursive blocking query's answer *is* the subtree at the index it woke
/// on, so the document is folded from those exact bytes and nothing is read
/// back. An implementation that re-fetched after waking would issue two
/// requests per change — and would have a window for a second write to land in,
/// which is how a document that never existed gets delivered. The scripted
/// agent counts the requests, so that window cannot open unnoticed.
///
/// Each answer stamps one generation into both sections, so a delivery whose
/// halves disagree is a torn document by construction.
#[test]
fn a_prefix_watch_folds_the_answer_it_blocked_for_and_never_re_reads() {
    // One priming answer — the loop's first query carries index 0 and reports
    // nothing, the same way a file watcher does not announce an edit when it
    // starts — and then three changes.
    let (address, asked, server) = scripted_seen(vec![
        stamped_at(5, 0),
        stamped_at(6, 1),
        stamped_at(7, 2),
        stamped_at(8, 3),
    ]);

    let source = Consul::new(&address, Keys::prefix("stamped/"))
        .with_format(Format::Json)
        .with_wait(std::time::Duration::from_secs(1));

    let watch = dynamic_config::RemoteWatch::new();
    let watching = watch.watching();
    let (sender, receiver) = std::sync::mpsc::channel();

    let watcher = std::thread::spawn(move || {
        source.watch(&watching, move |document| {
            let _ = sender.send(document.text);

            Ok(())
        })
    });

    let mut delivered = Vec::new();

    while delivered.len() < 3 {
        match receiver.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(text) => delivered.push(text),
            Err(_) => break,
        }
    }

    watch.stop();

    let outcome = watcher.join().expect("the watch thread");
    server.join().unwrap();

    assert!(
        outcome.is_ok(),
        "the watch ended for a reason other than being stopped: {outcome:?}"
    );

    assert_eq!(delivered.len(), 3, "one delivery per change: {delivered:?}");

    for (change, text) in delivered.iter().enumerate() {
        let tree = Value::parse(text, Format::Json).expect("every delivery is a document");

        let db = tree.get("db.generation").cloned();
        let server = tree.get("server.generation").cloned();

        assert_eq!(
            db, server,
            "a delivery whose two halves disagree is a document that never \
             existed at any index: {text}"
        );
        assert_eq!(
            db,
            Some(Value::Integer(i128::try_from(change).unwrap() + 1)),
            "and the changes arrive in order: {text}"
        );
    }

    let asked = asked.lock().unwrap();

    assert_eq!(
        asked.len(),
        4,
        "one blocking query per answer and not one more — a re-fetch after \
         waking would double this, and would open the window a torn document \
         comes through: {asked:?}"
    );
    assert!(
        asked.iter().all(|line| line.contains("recurse=true")),
        "every query is the recursive one: {asked:?}"
    );
    assert!(
        asked[1].contains("index=5") && asked[2].contains("index=6"),
        "each query carries the index the last answer returned: {asked:?}"
    );
}

/// Two sections under a watched prefix supplying one path is a deployment bug,
/// and no retry cures it — so the watch reports it and ends rather than looping
/// forever with nothing said. The report names the paths and never the values.
#[test]
fn a_watched_prefix_whose_sections_collide_ends_the_watch_by_name() {
    let (address, _asked, server) = scripted_seen(vec![
        stamped_at(5, 0),
        kv_entries_at(
            6,
            &[
                ("stamped/db.json", r#"{"db": {"password": "hunter2-left"}}"#),
                (
                    "stamped/extra.json",
                    r#"{"db": {"password": "hunter2-right"}}"#,
                ),
            ],
        ),
    ]);

    let source = Consul::new(&address, Keys::prefix("stamped/"))
        .with_format(Format::Json)
        .with_auth(Auth::token("hunter2-acl-token"))
        .with_wait(std::time::Duration::from_secs(1));

    let watch = dynamic_config::RemoteWatch::new();
    let watching = watch.watching();

    let error = source
        .watch(&watching, |_| Ok(()))
        .expect_err("the two sections both supply db.password");

    server.join().unwrap();

    let printed = format!("{error} {error:?}");

    assert!(printed.contains("db.password"), "{printed}");
    assert!(printed.contains("stamped/extra.json"), "{printed}");
    assert!(!printed.contains("hunter2"), "{printed}");
}

/// The credential rule, at the new error paths: a token must not be quoted
/// back in a complaint about a set of keys, whichever of them failed.
#[test]
fn no_multi_key_error_path_prints_a_credential() {
    let (address, _asked, server) = scripted_seen(vec![
        kv_answer(r#"{"db": {"host": "here"}}"#),
        http("403 Forbidden", "Permission denied"),
    ]);

    let source = Consul::new(
        &address,
        Keys::several(["myapp/db.json", "myapp/second.json"]),
    )
    .with_auth(Auth::token("hunter2-acl-token"));

    let error = source
        .fetch()
        .expect_err("the agent refused the second key");

    server.join().unwrap();

    let printed = format!("{error} {error:?} {source:?}");

    assert!(!printed.contains("hunter2"), "{printed}");
    assert!(printed.contains("myapp/second.json"), "{printed}");
}

// ---------------------------------------------------------------------------
// Reporting what the watch loop cannot otherwise say.
//
// `apply` records a delivery, so a working watch keeps the status current. A
// blocking query that keeps erroring delivers nothing — and used to say
// nothing, which left `remote_up` reporting the last delivery rather than the
// last attempt. These prove the other half: the agent goes away mid-watch, and
// the store stops looking healthy without the staleness clock being reset.
// ---------------------------------------------------------------------------

use dynamic_config::dynamic_config;
use serde::Deserialize;

/// One config type per test, as the repository's own rule requires: the
/// snapshot, the source and the status all live in `static`s keyed by the type.
#[dynamic_config]
#[derive(Debug, Deserialize)]
struct ReportingDb {
    host: String,
}

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct SilentDb {
    host: String,
}

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct DeletedKeyDb {
    host: String,
}

/// Polls `status()` until the store stops looking reachable, or gives up.
///
/// A watch loop is a thread: the failure lands when the query does, and
/// waiting a fixed time here would be either flaky or slow.
fn wait_for_unreachable(sink: &dynamic_config::RemoteSink) -> dynamic_config::RemoteStatus {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);

    loop {
        let status = sink.status();

        if status.reachable() == Some(false) || std::time::Instant::now() > deadline {
            return status;
        }

        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// The claim in one test: an agent that answered once and then went away is
/// **visible** — `reachable()` goes to `Some(false)` — and the staleness clock
/// keeps its value, because how long the served document has been stale is the
/// other half of what an alert needs.
#[test]
fn a_watch_whose_blocking_query_fails_reports_it_without_resetting_the_fetch_clock() {
    // One scripted answer, for the fetch that establishes a healthy store. The
    // listener closes behind it, so every blocking query the watch then issues
    // is refused — an agent that went away mid-watch.
    let (address, _requests, server) = scripted(vec![kv_answer(r#"{"db": {"host": "a"}}"#)]);

    ReportingDb::set_remote(Consul::new(&address, "myapp/db.json"));
    ReportingDb::refresh_remote().expect("the agent answers the first fetch");
    ReportingDb::builder("db")
        .init()
        .expect("the fetched document is the whole configuration");

    server.join().unwrap();

    // Taken once, where the loop is wired — which is also the only handle a
    // caller has on the status behind it.
    let sink = ReportingDb::remote_sink();
    let answering = sink.status();

    assert_eq!(answering.reachable(), Some(true), "the fetch answered");

    let watcher = Consul::new(&address, "myapp/db.json")
        .with_wait(std::time::Duration::from_secs(1))
        .reporting_to(sink);

    let watch = dynamic_config::RemoteWatch::new();
    let watching = watch.watching();
    let thread = std::thread::spawn(move || watcher.watch(&watching, |_| Ok(())));

    let failing = wait_for_unreachable(&sink);

    watch.stop();

    let outcome = thread.join().expect("the watch thread");

    assert!(
        outcome.is_ok(),
        "a query that fails is retried, not returned: {outcome:?}"
    );

    assert_eq!(
        failing.reachable(),
        Some(false),
        "a watch that cannot reach the agent must not go on reporting the \
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
        ReportingDb::current().host,
        "a",
        "and the document the agent did answer with is still serving: a \
         failed attempt is no reason to stop serving the last good one"
    );
}

/// The default is *nobody asked*: a watch wired without `reporting_to` still
/// survives the agent going away, and records nothing about it.
#[test]
fn a_watch_that_was_given_no_sink_reports_nowhere_and_still_survives() {
    let (address, _requests, server) = scripted(vec![kv_answer(r#"{"db": {"host": "a"}}"#)]);

    SilentDb::set_remote(Consul::new(&address, "myapp/db.json"));
    SilentDb::refresh_remote().expect("the agent answers the first fetch");
    SilentDb::builder("db")
        .init()
        .expect("the fetched document is the whole configuration");

    server.join().unwrap();

    let sink = SilentDb::remote_sink();

    let watcher =
        Consul::new(&address, "myapp/db.json").with_wait(std::time::Duration::from_secs(1));

    let watch = dynamic_config::RemoteWatch::new();
    let watching = watch.watching();
    let thread = std::thread::spawn(move || watcher.watch(&watching, |_| Ok(())));

    // Long enough for several refused queries: the point is that none of them
    // is recorded anywhere.
    std::thread::sleep(std::time::Duration::from_millis(500));

    watch.stop();

    let outcome = thread.join().expect("the watch thread");

    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(
        sink.status().reachable(),
        Some(true),
        "a loop nobody asked to report reports nothing — and pays for no \
         branch it did not want"
    );
    assert_eq!(SilentDb::current().host, "a");
}

/// A key that is deleted while it is being watched: the agent keeps answering,
/// with nothing. The loop waits — a key that does not exist *yet* is what a
/// watch is for — but a `fetch` of the same key calls an empty answer a
/// failure, so a watch that called it health would let a deleted key read as a
/// healthy store for as long as nobody recreated it.
#[test]
fn a_watched_key_that_holds_no_value_is_reported_rather_than_waited_out_in_silence() {
    let (address, requests, server) = scripted(vec![
        // The fetch that establishes a healthy store.
        kv_answer(r#"{"db": {"host": "a"}}"#),
        // Then the blocking query is answered with the empty array Consul
        // sends for a key that is not there.
        kv_entries_at(5, &[]),
    ]);

    DeletedKeyDb::set_remote(Consul::new(&address, "myapp/db.json"));
    DeletedKeyDb::refresh_remote().expect("the agent answers the first fetch");
    DeletedKeyDb::builder("db")
        .init()
        .expect("the fetched document is the whole configuration");

    let sink = DeletedKeyDb::remote_sink();
    let answering = sink.status();

    let watcher = Consul::new(&address, "myapp/db.json")
        .with_wait(std::time::Duration::from_secs(1))
        .reporting_to(sink);

    let watch = dynamic_config::RemoteWatch::new();
    let watching = watch.watching();
    let thread = std::thread::spawn(move || watcher.watch(&watching, |_| Ok(())));

    let failing = wait_for_unreachable(&sink);

    // Read before the loop's five-second pause is over, so the empty answer is
    // the only thing that can have been recorded — the next query would fail
    // on its own, the listener having run out of responses.
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "the fetch and one blocking query: the failure recorded here is the \
         empty answer, not the query that follows it"
    );

    watch.stop();

    let outcome = thread.join().expect("the watch thread");
    server.join().unwrap();

    assert!(
        outcome.is_ok(),
        "an empty answer is waited out: {outcome:?}"
    );
    assert_eq!(failing.reachable(), Some(false));
    assert_eq!(
        failing.last_fetch, answering.last_fetch,
        "the staleness clock keeps ageing"
    );
    assert_eq!(DeletedKeyDb::current().host, "a");
}

// ---------------------------------------------------------------------------
// TLS: the shared vocabulary, and where it refuses.
//
// The translation into `ureq`'s own types is unit-tested in `src/tls.rs`,
// against a certificate authority generated there. What belongs here is what
// only the source can answer: when the refusal happens, and what it says.
// ---------------------------------------------------------------------------

use dynamic_config_consul::TlsConfig;

/// An agent already carries a complete TLS configuration. Applying a second
/// one on top can only mean discarding one of them, and the one that would be
/// discarded is a certificate authority the caller believes is pinned — so it
/// is refused, at the first request, naming both calls.
#[test]
fn with_agent_and_with_tls_together_are_refused_rather_than_resolved() {
    let source = Consul::new("https://127.0.0.1:9", "myapp/db.json")
        .with_agent(ureq::Agent::new_with_defaults())
        .with_tls(TlsConfig::new().with_ca_certificate_file("/etc/consul.d/ca.pem"));

    let error = source.fetch().expect_err("both doors were opened");

    assert!(error.to_string().contains("with_agent"), "{error}");
    assert!(error.to_string().contains("with_tls"), "{error}");
    assert!(error.to_string().contains("refused"), "{error}");
}

/// A CA file that is not there is reported at the first request, naming the
/// path — the same laziness every other constructor in this family has, rather
/// than a panic in a builder chain.
#[test]
fn a_missing_ca_file_is_reported_at_the_first_request_and_names_the_path() {
    let source = Consul::new("https://127.0.0.1:9", "myapp/db.json")
        .with_tls(TlsConfig::new().with_ca_certificate_file("/nonexistent/consul-agent-ca.pem"));

    let error = source.fetch().expect_err("the CA file is not there");

    assert!(
        error
            .to_string()
            .contains("/nonexistent/consul-agent-ca.pem"),
        "{error}"
    );
    assert!(error.to_string().contains("the CA certificate"), "{error}");
}

/// A blocking query builds its own agent with a longer timeout, so the TLS
/// configuration has to reach that path too — a watch that ignored the
/// certificate authority would be a watch that never connects, discovered
/// hours after the fetch that worked.
#[test]
fn a_watch_reports_the_same_tls_failure_the_fetch_does() {
    let source = Consul::new("https://127.0.0.1:9", "myapp/db.json")
        .with_tls(TlsConfig::new().with_ca_certificate_file("/nonexistent/consul-agent-ca.pem"));

    let watch = dynamic_config::RemoteWatch::new();
    let watching = watch.watching();

    let error = source
        .watch(&watching, |_| Ok(()))
        .expect_err("the CA file is not there");

    assert!(
        error
            .to_string()
            .contains("/nonexistent/consul-agent-ca.pem"),
        "{error}"
    );
}

/// Every new error path gets the redaction test every old one has. Consul's
/// credential is an ACL token in a header, so what a TLS failure must not
/// disclose is that token — from the error, the `Debug` or the source.
#[test]
fn a_tls_failure_never_carries_the_token_out_with_it() {
    let source = Consul::new("https://127.0.0.1:9", "myapp/db.json")
        .with_token("hunter2-consul-token")
        .with_tls(TlsConfig::new().with_ca_certificate_file("/nonexistent/consul-agent-ca.pem"));

    let error = source.fetch().expect_err("the CA file is not there");
    let printed = format!("{error} {error:?} {source:?}");

    assert!(!printed.contains("hunter2"), "{printed}");
    assert!(
        printed.contains("/nonexistent/consul-agent-ca.pem"),
        "{printed}"
    );
}
