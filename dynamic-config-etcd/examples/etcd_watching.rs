//! etcd, authenticated with `ConnectOptions`, watched with a real push stream.
//!
//! ```text
//! docker run --rm -p 2379:2379 quay.io/coreos/etcd:v3.5.17 \
//!   etcd --advertise-client-urls=http://0.0.0.0:2379 \
//!        --listen-client-urls=http://0.0.0.0:2379
//!
//! etcdctl put myapp/db.json '{"db": {"host": "db.internal", "port": 5432}}'
//!
//! cargo run -p dynamic-config-etcd --example etcd_watching
//! ```

use std::time::Duration;

use dynamic_config::{dynamic_config, AsyncRemoteSource};
use dynamic_config_etcd::{ConnectOptions, Etcd};
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct DbConfig {
    host: String,
    port: u16,
}

async fn source(endpoint: &str) -> Result<Etcd, dynamic_config::Error> {
    // Credentials and TLS live on etcd's own options, so there is no second
    // vocabulary to learn:
    //
    //     ConnectOptions::new().with_user("myapp", password)
    //
    // Keep-alives are worth setting for a long-lived watch, which is otherwise
    // a connection with nothing on it for hours at a time.
    Etcd::with_options(
        [endpoint],
        "myapp/db.json",
        ConnectOptions::new().with_keep_alive(Duration::from_secs(30), Duration::from_secs(5)),
    )
    .await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint =
        std::env::var("ETCD_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:2379".to_owned());

    let reader = source(&endpoint).await?;

    // `new` succeeding does not prove etcd is reachable: the client connects
    // lazily, so the first read is where that shows up.
    println!("built a source for {}\n", reader.describe());

    DbConfig::set_remote_async(reader);
    DbConfig::refresh_remote_async().await?;

    // No files: the fetched document is the whole configuration. Initializing
    // through the builder is also what lets `apply_remote` reload later.
    DbConfig::builder("db").env("APP_").init_async().await?;

    println!("host = {}", DbConfig::current().host);
    println!("port = {}", DbConfig::current().port);
    println!("traced back to: {:?}", DbConfig::source_of("host")?);

    // ---------------------------------------------------------------------
    // Watching: a future. Cancelled by dropping it, on any executor.
    // ---------------------------------------------------------------------
    println!("\nwatching for 10 seconds — try another `etcdctl put ...`");

    let watcher = source(&endpoint).await?;

    let task = tokio::spawn(async move {
        // Never returns `Ok`: a watch either runs or has failed, so the only
        // way out is an error — including the connection closing.
        watcher.watch(DbConfig::apply_remote).await
    });

    // A task that also awaits reloads, to show the two halves side by side:
    // the watch pushes, and `changes()` wakes whoever cares.
    let mut changes = DbConfig::changes();

    let reader = tokio::spawn(async move {
        loop {
            let config = changes.changed().await;

            println!("  a reader woke up: host is now {}", config.host);
        }
    });

    tokio::time::sleep(Duration::from_secs(10)).await;

    // Dropping the task is the whole cancellation story.
    task.abort();
    reader.abort();

    println!("\nfinal host = {}", DbConfig::current().host);

    Ok(())
}
