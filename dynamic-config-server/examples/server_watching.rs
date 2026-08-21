//! The config server, watched over its change stream — a push, not a poll.
//!
//! ```text
//! cd ../examples/compose && docker compose up -d
//! cargo run -p dynamic-config-server --example server_watching --features client
//! ```
//!
//! While it runs, install a new document on the server — the generation
//! moves, the stream says so, and this process re-fetches. `served.rs` is
//! the same wiring with a poll in place of the stream; the difference is
//! how quickly an edit arrives and how much traffic asking costs.

use std::time::Duration;

use dynamic_config::{dynamic_config, RemoteWatch};
use dynamic_config_server::client::ConfigServer;
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct Billing {
    host: String,
    port: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base =
        std::env::var("CONFIG_SERVER").unwrap_or_else(|_| "http://localhost:8155".to_owned());
    let token = std::env::var("CONFIG_SERVER_TOKEN")
        .unwrap_or_else(|_| "dev-token-not-for-production-32ch".to_owned());

    let source = ConfigServer::new(&base, "billing", "prod").with_token(&token);

    Billing::set_remote(source);
    Billing::refresh_remote()?;

    // No files: the served document is the whole configuration. Initializing
    // through the builder is also what lets the sink reload later.
    Billing::builder("billing").init()?;

    // Taken at wiring, because a sink remembers which source is installed:
    // one whose source is later replaced refuses to push.
    let sink = Billing::remote_sink();

    println!("{}:{}", Billing::current().host, Billing::current().port);

    // ---------------------------------------------------------------------
    // Watching: blocking, so it lives on a thread. The `Watching` token is
    // how the thread learns it is time to stop.
    // ---------------------------------------------------------------------
    println!("\nwatching for 60 seconds — install a new document to move it");

    let watcher = ConfigServer::new(&base, "billing", "prod").with_token(&token);
    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let thread = std::thread::spawn(move || {
        // The interval here is the *reconnect* pace, not a poll: the stream
        // pushes, and this is how long to wait before trying again when it
        // ends. The waits are spread and grow after a failure, so a server
        // coming back up is not met by every pod at once.
        watcher.watch(&watching, Duration::from_secs(5), |document| {
            sink.apply(document)
                .map_err(|error| {
                    eprintln!("a document did not apply: {error}");
                    error
                })
                // A bad document is logged and survived; only a decision to
                // stop should end a watch.
                .or(Ok(()))
        })
    });

    std::thread::sleep(Duration::from_secs(60));

    // `stop()` — or dropping `watch` — ends the loop; an in-flight
    // connection is not waited on.
    watch.stop();
    thread.join().expect("the watch thread should end")?;

    println!(
        "\nfinal {}:{}",
        Billing::current().host,
        Billing::current().port
    );

    Ok(())
}
