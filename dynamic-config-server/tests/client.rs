//! The client half, against the real server.
//!
//! ```text
//! cargo test -p dynamic-config-server --features client
//! ```
//!
//! Both halves live in one crate so that they are tested against each other
//! rather than against a fixture of what each believes the other returns.
//! Every test here starts the real router on a real socket and reads it with
//! the real [`ConfigServer`] source.

#![cfg(feature = "client")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{client, section};
use dynamic_config::{ErrorKind, RemoteSource};
use dynamic_config_server::client::ConfigServer;
use dynamic_config_server::{router, NoAudit, Server, ServerConfig};

/// A server on an ephemeral loopback port, plus the handle that stops it.
struct Serving {
    url: String,
    stop: tokio::sync::oneshot::Sender<()>,
    _directory: tempfile::TempDir,
}

async fn serve(document: &str) -> Serving {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let file = directory.path().join("billing.toml");
    std::fs::write(&file, document).expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        bind: "127.0.0.1:0".to_owned(),
        sections: vec![section("billing", "prod", file.display().to_string())],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };

    let server =
        Arc::new(Server::start_with(&config, NoAudit).expect("the configuration is valid"));
    let listener = tokio::net::TcpListener::bind(server.address())
        .await
        .expect("loopback, port zero");
    let address = listener.local_addr().expect("a bound listener has one");
    let (stop, stopped) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let _ = axum::serve(listener, router(server))
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await;
    });

    Serving {
        url: format!("http://{address}"),
        stop,
        _directory: directory,
    }
}

/// A `RemoteSource` runs its own runtime, so it is driven off the reactor.
async fn fetched(source: ConfigServer) -> Result<dynamic_config::Fetched, dynamic_config::Error> {
    tokio::task::spawn_blocking(move || source.fetch())
        .await
        .expect("the blocking task ran")
}

#[tokio::test]
async fn a_document_is_read_from_the_server_that_serves_it() {
    let serving = serve("[billing]\nhost = 'db.internal'\nport = 5432\n").await;

    let document = fetched(
        ConfigServer::new(&serving.url, "billing", "prod").with_token(common::BILLING_TOKEN),
    )
    .await
    .expect("the section is served and the token grants it");

    let value = dynamic_config::Value::parse(&document.text, document.format)
        .expect("the server answers JSON");

    assert!(value.get("host").is_some(), "{}", document.text);
    assert!(
        !document.text.contains("\"config\""),
        "the envelope is unwrapped, so the engine sees the document rather \
         than the server's reply shape: {}",
        document.text
    );
}

#[tokio::test]
async fn a_source_with_no_token_is_refused_by_the_server() {
    let serving = serve("[billing]\nhost = 'db.internal'\n").await;

    let error = fetched(ConfigServer::new(&serving.url, "billing", "prod"))
        .await
        .expect_err("the server demands a credential");

    assert_eq!(
        error.kind(),
        ErrorKind::Auth,
        "a refused credential is worth telling apart from an unreachable \
         server: {error}"
    );
}

/// The server answers the same 404 for a section that is not yours and one
/// that does not exist — deliberately, so that a caller cannot enumerate what
/// it may not read. The client keeps that ambiguity rather than inventing a
/// distinction, and classifies it as `Auth`: both readings are things a
/// human has to fix, and neither is fixed by trying again in a minute, which
/// is the only thing the reload logic does with this.
#[tokio::test]
async fn a_section_the_token_does_not_grant_is_refused_rather_than_retried() {
    let serving = serve("[billing]\nhost = 'db.internal'\n").await;

    let error = fetched(
        ConfigServer::new(&serving.url, "shipping", "prod").with_token(common::BILLING_TOKEN),
    )
    .await
    .expect_err("this token grants billing and nothing else");

    assert_eq!(
        error.kind(),
        ErrorKind::Auth,
        "waiting fixes neither a missing grant nor a missing section: {error}"
    );

    let rendered = error.to_string();

    assert!(
        rendered.contains("grant") && rendered.contains("section"),
        "the message has to name both readings, because the server will not \
         say which: {error}"
    );
}

/// The property the client half exists for. A configuration server is a
/// dependency at *start-up*; once a document has been fetched, the engine's
/// last-known-good cache is what keeps a pod serving through a server outage,
/// and that is the client's cache rather than the server's.
#[tokio::test]
async fn a_server_that_goes_away_does_not_take_its_clients_with_it() {
    let serving = serve("[billing]\nhost = 'db.internal'\n").await;
    let source = ConfigServer::new(&serving.url, "billing", "prod")
        .with_token(common::BILLING_TOKEN)
        .with_timeout(Duration::from_secs(2));

    let first = fetched(
        ConfigServer::new(&serving.url, "billing", "prod")
            .with_token(common::BILLING_TOKEN)
            .with_timeout(Duration::from_secs(2)),
    )
    .await
    .expect("the server is up");

    assert!(first.text.contains("db.internal"));

    // Kill it mid-run.
    let _ = serving.stop.send(());
    tokio::time::sleep(Duration::from_millis(200)).await;

    let error = fetched(source).await.expect_err("the server is gone");

    assert_eq!(
        error.kind(),
        ErrorKind::Remote,
        "an unreachable server is a remote failure, not an auth one: {error}"
    );
    assert!(
        !error.to_string().contains(common::BILLING_TOKEN),
        "a failure must not carry the credential out with it: {error}"
    );
}

#[tokio::test]
async fn a_failure_never_carries_the_token() {
    let serving = serve("[billing]\nhost = 'db.internal'\n").await;

    let error = fetched(
        ConfigServer::new(&serving.url, "billing", "prod").with_token("hunter2-do-not-print"),
    )
    .await
    .expect_err("that token belongs to nobody");

    let rendered = format!("{error} {error:?}");

    assert!(!rendered.contains("hunter2"), "{rendered}");
}

// ---------------------------------------------------------------------------
// Watching
// ---------------------------------------------------------------------------

/// The stream, end to end: an edit on disk reaches a watching client.
///
/// The whole chain in one test, because every link of it is where this could
/// silently do nothing — the server notices the file, installs a generation,
/// pushes an event, the client re-fetches, and the document differs.
#[tokio::test]
async fn an_edit_reaches_a_watching_client() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let file = directory.path().join("billing.toml");
    std::fs::write(&file, "[billing]\nport = 1\n").expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        bind: "127.0.0.1:0".to_owned(),
        sections: vec![section("billing", "prod", file.display().to_string())],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };

    let server =
        Arc::new(Server::start_with(&config, NoAudit).expect("the configuration is valid"));
    let listener = tokio::net::TcpListener::bind(server.address())
        .await
        .expect("loopback, port zero");
    let address = listener.local_addr().expect("a bound listener has one");

    let serving = Arc::clone(&server);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(serving)).await;
    });

    let source = ConfigServer::new(format!("http://{address}"), "billing", "prod")
        .with_token(common::BILLING_TOKEN)
        .with_timeout(Duration::from_secs(5));

    let handle = dynamic_config::RemoteWatch::new();
    let watching = handle.watching();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    let watcher = tokio::task::spawn_blocking({
        let seen = Arc::clone(&seen);

        move || {
            source.watch(&watching, Duration::from_millis(200), move |document| {
                seen.lock().unwrap().push(document.text);

                Ok(())
            })
        }
    });

    // **The current document is not delivered at startup**, which is the
    // contract every source in this family keeps — the server's opening
    // event says where the document stands, and where it stands is not a
    // change. Waited on rather than asserted immediately, so a delivery
    // that arrives late still fails the assertion below rather than
    // slipping past it.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        seen.lock().unwrap().is_empty(),
        "the subscription delivered the current document at startup: {:?}",
        seen.lock().unwrap()
    );

    std::fs::write(&file, "[billing]\nport = 2\n").expect("writable");

    // The reload a deployment gets from the server's own file watcher, done
    // by hand: this test is about the client following the stream, not about
    // how the server came to install something.
    server
        .section("billing", "prod")
        .expect("the section is served")
        .reload()
        .expect("the edited file loads");

    let mut arrived = false;

    for _ in 0..100 {
        if seen.lock().unwrap().iter().any(|text| text.contains('2')) {
            arrived = true;
            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    handle.stop();
    let _ = watcher.await;

    let seen = seen.lock().unwrap().clone();

    assert!(arrived, "the edit never reached the client: {seen:?}");
    assert_eq!(
        seen.len(),
        1,
        "the edit is the only delivery: the opening event is where the \
         document stands, not that it moved: {seen:?}"
    );
}

/// **A callback that panics ends the watch, and does not take the thread.**
///
/// The eight store crates all deliver through
/// `dynamic_config_store_core::guarded` for this reason, and this client
/// called the callback directly: a panic unwound out of `watch`, through
/// `block_on`, and killed the caller's thread — with the `RemoteWatch`
/// handle still looking alive to everyone holding one.
#[tokio::test]
async fn a_panicking_callback_ends_the_watch_rather_than_the_thread() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let file = directory.path().join("billing.toml");
    std::fs::write(&file, "[billing]\nport = 1\n").expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        bind: "127.0.0.1:0".to_owned(),
        sections: vec![section("billing", "prod", file.display().to_string())],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };

    let server =
        Arc::new(Server::start_with(&config, NoAudit).expect("the configuration is valid"));
    let listener = tokio::net::TcpListener::bind(server.address())
        .await
        .expect("loopback, port zero");
    let address = listener.local_addr().expect("a bound listener has one");

    let serving = Arc::clone(&server);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(serving)).await;
    });

    let source = ConfigServer::new(format!("http://{address}"), "billing", "prod")
        .with_token(common::BILLING_TOKEN)
        .with_timeout(Duration::from_secs(5));

    let handle = dynamic_config::RemoteWatch::new();
    let watching = handle.watching();

    let watcher = tokio::task::spawn_blocking(move || {
        source.watch(&watching, Duration::from_millis(200), |_document| {
            panic!("a callback somebody else wrote")
        })
    });

    // An install, so there is something to deliver into the panic.
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(&file, "[billing]\nport = 2\n").expect("writable");
    server
        .section("billing", "prod")
        .expect("the section is served")
        .reload()
        .expect("the edited file loads");

    let ended = tokio::time::timeout(Duration::from_secs(10), watcher)
        .await
        .expect("the watch ends rather than hanging")
        .expect("the thread survived the panic");

    let error = ended.expect_err("a panicking callback is an error, not a quiet stop");

    assert!(
        error.to_string().contains("panicked"),
        "the error says what happened: {error}"
    );

    handle.stop();
}

/// The capability a client reports is what an agent plans around.
#[test]
fn the_client_says_it_is_native() {
    use dynamic_config::WatchCapability;

    let source = ConfigServer::new("http://localhost:1", "billing", "prod");

    assert_eq!(source.watch_capability(), WatchCapability::Native);
}
