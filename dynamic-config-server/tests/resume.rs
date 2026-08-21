//! What a watch does with the change it could not read.
//!
//! One property, and it needs a server that fails on purpose: **the resume
//! point does not move until the document behind it has been read.** The
//! real server never fails a fetch mid-stream, so the server here is a
//! socket that speaks exactly enough HTTP to be wrong in the one way that
//! matters — it refuses a document once, and records what every
//! reconnection asked to resume from.
//!
//! The failure it pins: advancing the resume point on the *event* meant a
//! reconnect carried a `Last-Event-ID` for a generation this client had
//! never actually read. The server, seeing nothing newer than what the
//! client claimed, sent nothing — and the change stayed lost until the next
//! install, which on a quiet configuration is hours.

#![cfg(feature = "client")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use dynamic_config_server::client::ConfigServer;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// What the fake server was asked, in the order it was asked.
#[derive(Default)]
struct Asked {
    /// One entry per subscription: what it wanted to resume from.
    resumed: Vec<Option<String>>,
    /// How many documents have been requested.
    fetches: usize,
}

/// The head of a request: its path, and the resume point it carried.
async fn head(socket: &mut tokio::net::TcpStream) -> Option<(String, Option<String>)> {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];

    while !request.ends_with(b"\r\n\r\n") {
        match socket.read(&mut byte).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => request.push(byte[0]),
        }
    }

    let request = String::from_utf8_lossy(&request).into_owned();
    let path = request
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .to_owned();
    let resume = request.lines().find_map(|line| {
        line.strip_prefix("last-event-id: ")
            .or_else(|| line.strip_prefix("Last-Event-ID: "))
            .map(|value| value.trim().to_owned())
    });

    Some((path, resume))
}

/// A config server that refuses the first document it is asked for.
///
/// Every subscription is answered with two events: the opening one, which
/// says where the document stands, and one more that says it moved. The
/// first fetch behind that second event fails.
async fn serve(listener: TcpListener, asked: Arc<Mutex<Asked>>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };

        let asked = Arc::clone(&asked);

        tokio::spawn(async move {
            let Some((path, resume)) = head(&mut socket).await else {
                return;
            };

            if path.ends_with("/stream") {
                asked.lock().unwrap().resumed.push(resume);

                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\n\
                          content-type: text/event-stream\r\n\
                          cache-control: no-cache\r\n\
                          connection: close\r\n\r\n",
                    )
                    .await;

                // Where the document stands, then that it moved.
                let _ = socket
                    .write_all(b"id: 7\ndata: {\"generation\":7}\n\n")
                    .await;
                let _ = socket.flush().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = socket
                    .write_all(b"id: 8\ndata: {\"generation\":8}\n\n")
                    .await;
                let _ = socket.flush().await;

                // Long enough for the client to try the fetch and give up on
                // this connection, short enough not to slow the test down.
                tokio::time::sleep(Duration::from_millis(600)).await;

                return;
            }

            let refused = {
                let mut asked = asked.lock().unwrap();
                asked.fetches += 1;

                asked.fetches == 1
            };

            if refused {
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\n\
                          content-length: 0\r\n\
                          connection: close\r\n\r\n",
                    )
                    .await;

                return;
            }

            let body = br#"{"application":"billing","profile":"prod","generation":8,"config":{"port":8080}}"#;

            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
            let _ = socket.write_all(body).await;
            let _ = socket.flush().await;
        });
    }
}

#[tokio::test]
async fn a_change_whose_fetch_failed_is_resumed_from_before_it() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback, port zero");
    let address = listener.local_addr().expect("a bound listener has one");
    let asked = Arc::new(Mutex::new(Asked::default()));

    tokio::spawn(serve(listener, Arc::clone(&asked)));

    let source = ConfigServer::new(format!("http://{address}"), "billing", "prod")
        .with_token("a token this server does not check")
        .with_timeout(Duration::from_secs(2));

    let handle = dynamic_config::RemoteWatch::new();
    let watching = handle.watching();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));

    let watcher = tokio::task::spawn_blocking({
        let seen = Arc::clone(&seen);

        move || {
            source.watch(&watching, Duration::from_millis(100), move |document| {
                seen.lock().unwrap().push(document.text);

                Ok(())
            })
        }
    });

    // The second subscription is the one under test, and it takes a
    // reconnect to reach: wait for the document rather than for a clock.
    for _ in 0..100 {
        if !seen.lock().unwrap().is_empty() {
            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    handle.stop();
    let _ = watcher.await;

    let asked = asked.lock().unwrap();
    let seen = seen.lock().unwrap().clone();

    assert!(
        asked.resumed.len() >= 2,
        "the refused fetch should have ended the connection and reconnected: {:?}",
        asked.resumed
    );

    assert_eq!(
        asked.resumed[0], None,
        "a first subscription resumes from nowhere"
    );

    assert_eq!(
        asked.resumed[1].as_deref(),
        Some("7"),
        "the reconnect resumes from the last generation this client actually \
         read — 8 was announced, refused, and never read: {:?}",
        asked.resumed
    );

    assert_eq!(
        seen,
        vec![r#"{"port":8080}"#.to_owned()],
        "the change arrives once the fetch behind it succeeds"
    );
}
