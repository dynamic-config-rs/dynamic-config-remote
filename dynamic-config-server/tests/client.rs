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
