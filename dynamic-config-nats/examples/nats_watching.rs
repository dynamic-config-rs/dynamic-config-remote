//! NATS JetStream, connected with `ConnectOptions`, watched with a KV stream.
//!
//! ```text
//! docker run --rm -p 4222:4222 nats:2.10.14 --jetstream
//!
//! # The `nats` CLI lives in `nats-box`, not in the server image:
//! docker run --rm --network host natsio/nats-box nats kv add config
//! docker run --rm --network host natsio/nats-box \
//!   nats kv put config db.json '{"db": {"host": "db.internal", "port": 5432}}'
//!
//! cargo run -p dynamic-config-nats --example nats_watching
//! ```

use std::time::Duration;

use dynamic_config::{dynamic_config, AsyncRemoteSource};
use dynamic_config_nats::{ConnectOptions, Nats};
use serde::Deserialize;

#[dynamic_config(files = [], key = "db", env = "APP_", async, diff)]
#[derive(Debug, Deserialize)]
struct DbConfig {
    host: String,
    port: u16,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_owned());

    // One connection, shared by the reader and the watcher. NATS clients are
    // handles, so this is cheaper than dialling twice — and it is the shape a
    // program that already talks to NATS will be in anyway.
    //
    // Credentials go here:
    //
    //     ConnectOptions::with_credentials_file("/etc/myapp/nats.creds").await?
    let client = ConnectOptions::new()
        .connect(&server)
        .await
        .map_err(|error| dynamic_config::Error::remote(error.to_string()))?;

    let reader = Nats::from_client(client.clone(), "config", "db.json").await?;

    println!("built a source for {}\n", reader.describe());

    DbConfig::set_remote_async(reader);
    DbConfig::refresh_remote_async().await?;
    DbConfig::init_async().await?;

    println!("host = {}", DbConfig::current().host);
    println!("port = {}", DbConfig::current().port);
    println!("traced back to: {:?}", DbConfig::source_of("host")?);

    // ---------------------------------------------------------------------
    // Watching: a future. Cancelled by dropping it, on any executor.
    //
    // The client reconnects on its own, indefinitely, and re-establishes the
    // subscription when it does — which is why there is no retry loop here.
    // ---------------------------------------------------------------------
    println!("\nwatching for 10 seconds — try another `nats kv put ...`");

    let watcher = Nats::from_client(client, "config", "db.json").await?;

    let task = tokio::spawn(async move { watcher.watch(DbConfig::apply_remote).await });

    let mut changes = DbConfig::changes();

    let waiter = tokio::spawn(async move {
        loop {
            let config = changes.changed().await;

            println!("  a reader woke up: host is now {}", config.host);
        }
    });

    tokio::time::sleep(Duration::from_secs(10)).await;

    task.abort();
    waiter.abort();

    println!("\nfinal host = {}", DbConfig::current().host);

    Ok(())
}
