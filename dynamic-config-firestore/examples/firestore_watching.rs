//! Firestore (here: the emulator), watched by polling the document's
//! `updateTime`.
//!
//! ```text
//! docker run --rm -p 8080:8080 \
//!   gcr.io/google.com/cloudsdktool/google-cloud-cli:emulators \
//!   gcloud emulators firestore start --host-port=0.0.0.0:8080
//!
//! # Write a document through the REST surface:
//! curl -X PATCH \
//!   'http://127.0.0.1:8080/v1/projects/my-project/databases/(default)/documents/config/db' \
//!   -H 'Content-Type: application/json' \
//!   -d '{"fields": {"host": {"stringValue": "db.internal"},
//!                   "port": {"integerValue": "5432"}}}'
//!
//! cargo run -p dynamic-config-firestore --example firestore_watching
//! ```
//!
//! Against real Firestore, drop `with_endpoint` and authenticate instead:
//! `Auth::metadata_server()` on GKE / Cloud Run / GCE, or
//! `Auth::access_token(..)` with a token minted outside the process.

use std::time::Duration;

use dynamic_config::{dynamic_config, RemoteSource, RemoteWatch};
use dynamic_config_firestore::Firestore;
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct DbConfig {
    host: String,
    port: u16,
}

fn source(endpoint: &str) -> Firestore {
    // `Auth::Emulator` is the default: no token at all, which is right for
    // the emulator and wrong for everything else.
    Firestore::new("my-project", "config/db").with_endpoint(endpoint)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint =
        std::env::var("FIRESTORE_EMULATOR").unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned());

    let reader = source(&endpoint);

    println!("built a source for {}\n", reader.describe());

    DbConfig::set_remote(reader);
    DbConfig::refresh_remote()?;

    // No files: the fetched document is the whole configuration. Initializing
    // through the builder is also what lets the sink from `remote_sink()` reload later.
    DbConfig::builder("db").env("APP_").init()?;

    // The sink is taken here, at wiring: it remembers which source is
    // installed, and a sink whose source is later replaced refuses to push.
    let sink = DbConfig::remote_sink();

    println!("host = {}", DbConfig::current().host);
    println!("port = {}", DbConfig::current().port);
    println!("traced back to: {:?}", DbConfig::source_of("host")?);

    // ---------------------------------------------------------------------
    // Watching: blocking, so it lives on a thread. Each tick reads the one
    // small document and compares `updateTime`.
    // ---------------------------------------------------------------------
    println!("\nwatching for 30 seconds — try another `curl -X PATCH ...`");

    // `reporting_to` is the other half of the sink: `apply` records a
    // delivery, and this records a poll that came back with nothing — so a
    // project that stopped answering stops looking like a document that simply
    // has not changed.
    let watcher = source(&endpoint).reporting_to(sink);
    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let thread = std::thread::spawn(move || {
        watcher.watch(&watching, Duration::from_secs(2), move |document| {
            sink.apply(document)
        })
    });

    std::thread::sleep(Duration::from_secs(30));

    // `stop()` — or dropping `watch` — ends the loop within a quarter second.
    watch.stop();
    thread.join().expect("the watch thread should end")?;

    println!("\nfinal host = {}", DbConfig::current().host);

    Ok(())
}
