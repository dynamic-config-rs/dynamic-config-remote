//! Vault, watched by polling the KV v2 *metadata* version — the secret
//! itself is read only when the version moves.
//!
//! ```text
//! docker run --rm -p 8200:8200 \
//!   -e VAULT_DEV_ROOT_TOKEN_ID=myroot hashicorp/vault:1.17
//!
//! curl -s -X POST http://127.0.0.1:8200/v1/secret/data/myapp/db \
//!   -H 'X-Vault-Token: myroot' \
//!   -d '{"data": {"host": "db.internal", "port": 5432}}'
//!
//! cargo run -p dynamic-config-vault --example vault_watching
//! ```
//!
//! Vault cannot push — no watch, no stream — so this polls. What it does
//! *not* do is pull the secret every tick: an unchanged secret is never
//! transferred, never decrypted, and never written to an audit log as a read.

use std::time::Duration;

use dynamic_config::{dynamic_config, RemoteSource, RemoteWatch};
use dynamic_config_vault::Vault;
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct DbConfig {
    host: String,
    port: u16,
}

fn source(address: &str, token: &str) -> Vault {
    // The dev token here; a real deployment logs in instead —
    // `Auth::kubernetes(..)`, `Auth::app_role(..)` — and the session renews
    // the lease before it expires.
    Vault::new(address, "secret", "myapp/db")
        .with_key("db")
        .with_token(token)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address =
        std::env::var("VAULT_ADDR").unwrap_or_else(|_| "http://127.0.0.1:8200".to_owned());
    let token = std::env::var("VAULT_TOKEN").unwrap_or_else(|_| "myroot".to_owned());

    let reader = source(&address, &token);

    println!("built a source for {}\n", reader.describe());

    // Fetch first: a watch reports *changes*, so the starting value is
    // explicitly loaded rather than waited for.
    DbConfig::set_remote(reader);
    DbConfig::refresh_remote()?;

    // No files: the fetched secret is the whole configuration. Initializing
    // through the builder is also what lets the sink from `remote_sink()` reload later.
    DbConfig::builder("db").env("APP_").init()?;

    // The sink is taken here, at wiring: it remembers which source is
    // installed, and a sink whose source is later replaced refuses to push.
    let sink = DbConfig::remote_sink();

    println!("host = {}", DbConfig::current().host);
    println!("port = {}", DbConfig::current().port);
    println!("traced back to: {:?}", DbConfig::source_of("host")?);

    // ---------------------------------------------------------------------
    // Watching: blocking, so it lives on a thread. Each tick reads
    // `secret/metadata/myapp/db` for `current_version`; only a new version
    // costs a read of the secret itself.
    // ---------------------------------------------------------------------
    println!("\nwatching for 30 seconds — try another `curl -X POST ...`");

    let watcher = source(&address, &token);
    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let thread = std::thread::spawn(move || {
        // the sink from `remote_sink()` is the same reload path a file edit takes:
        // validation, the reload hooks, the cache. A document that does not
        // fit leaves the previous snapshot serving.
        watcher.watch(&watching, Duration::from_secs(2), move |document| {
            sink.apply(document)
        })
    });

    std::thread::sleep(Duration::from_secs(30));

    // `stop()` — or dropping `watch` — ends the loop within a quarter
    // second, whatever the poll interval is.
    watch.stop();
    thread.join().expect("the watch thread should end")?;

    println!("\nfinal host = {}", DbConfig::current().host);

    Ok(())
}
