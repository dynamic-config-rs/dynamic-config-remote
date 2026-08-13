//! The two things `oneshot` cannot prove: that the router serves over a real
//! socket, and that a section reloads because the watcher noticed rather than
//! because something polled.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{client, section, start};
use dynamic_config_server::{router, NoAudit, Server, ServerConfig};

/// A raw `GET`, because this suite is about the socket and a client library
/// would be one more thing between the assertion and the thing asserted.
fn get(address: std::net::SocketAddr, path: &str, token: Option<&str>) -> String {
    let mut stream = TcpStream::connect(address).expect("the server is listening");
    let authorization = token.map_or_else(String::new, |token| {
        format!("Authorization: Bearer {token}\r\n")
    });

    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{authorization}\r\n"
    )
    .expect("the request is written");

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("the server closes the connection");

    response
}

#[tokio::test]
async fn a_real_server_on_an_ephemeral_port_serves_over_tcp() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let file = directory.path().join("billing.toml");
    std::fs::write(&file, "[billing]\nhost = 'db.internal'\n").expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 0,
        // Port zero: the operating system picks, so two of these can run at
        // once and a busy port is never a test failure.
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

    let serving = tokio::spawn(async move {
        let _ = axum::serve(listener, router(server)).await;
    });

    let healthz = tokio::task::spawn_blocking(move || get(address, "/healthz", None))
        .await
        .expect("the blocking task ran");

    assert!(healthz.starts_with("HTTP/1.1 200 OK"), "{healthz}");
    assert!(healthz.contains(r#"{"status":"ok"}"#), "{healthz}");

    let document = tokio::task::spawn_blocking(move || {
        get(address, "/billing/prod", Some(common::BILLING_TOKEN))
    })
    .await
    .expect("the blocking task ran");

    assert!(document.starts_with("HTTP/1.1 200 OK"), "{document}");
    assert!(document.contains("db.internal"), "{document}");

    let refused = tokio::task::spawn_blocking(move || get(address, "/billing/prod", None))
        .await
        .expect("the blocking task ran");

    assert!(
        refused.starts_with("HTTP/1.1 401 Unauthorized"),
        "{refused}"
    );
    assert!(refused.contains("www-authenticate: Bearer"), "{refused}");

    serving.abort();
}

/// The server has no loop of its own: a section moves because the library's
/// file watcher said so. If the watcher were never registered, this test
/// would wait out its whole deadline and fail — which is the only way to
/// tell "event-driven" from "nobody wired it up" from the outside.
#[tokio::test]
async fn a_file_change_reloads_a_section_with_nothing_polling() {
    // Under the crate rather than the system temporary directory, which is
    // what the engine's own watcher tests do and for the same reason: on
    // macOS `/var` is a symlink to `/private/var`, so FSEvents reports the
    // resolved path and never matches a watch registered on the link; on
    // Windows the runner's TEMP is an 8.3 short name and the events carry the
    // long one. Neither is this crate's bug and neither is worth a
    // canonicalisation dance in a test — a directory with no symlink over it
    // has neither problem.
    let root = std::path::Path::new("tests/scratch");
    std::fs::create_dir_all(root).expect("a scratch directory");

    let directory = tempfile::Builder::new()
        .prefix("live")
        .tempdir_in(root)
        .expect("a temporary directory under the crate");
    let file = directory.path().join("billing.toml");
    std::fs::write(&file, "[billing]\nhost = 'db.internal'\n").expect("writable");

    let config = ServerConfig {
        watch_debounce_ms: 50,
        sections: vec![section("billing", "prod", file.display().to_string())],
        clients: vec![client("billing-pod", common::BILLING_TOKEN, &["billing"])],
        ..ServerConfig::default()
    };
    let fixture = start(directory, config);
    let section = fixture
        .server
        .section("billing", "prod")
        .expect("the server serves it");

    assert_eq!(section.generation(), 1);

    std::fs::write(&file, "[billing]\nhost = 'db.moved'\n").expect("writable");

    // Generous, because a filesystem event's latency is the operating
    // system's business: the assertion is that it arrives at all.
    let deadline = Instant::now() + Duration::from_secs(10);

    while section.generation() < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(
        section.generation(),
        2,
        "the watcher never reloaded the section"
    );

    let (status, body) = fixture
        .json("/billing/prod", Some(common::BILLING_TOKEN))
        .await;

    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["config"]["host"], "db.moved");
    assert_eq!(body["generation"], 2);
}
