//! Consul, authenticated the way a workload does, watched with a blocking query.
//!
//! ```text
//! # An agent with ACLs off, which is the development shape:
//! docker run --rm -p 8500:8500 hashicorp/consul:1.16.1
//!
//! # Put a document in it:
//! curl -X PUT --data '{"db": {"host": "db.internal", "port": 5432}}' \
//!   http://127.0.0.1:8500/v1/kv/myapp/db.json
//!
//! cargo run -p dynamic-config-consul --example consul_kubernetes
//! ```

use std::time::Duration;

use dynamic_config::{dynamic_config, RemoteSource, RemoteWatch};
use dynamic_config_consul::{Auth, Consul};
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct DbConfig {
    host: String,
    port: u16,
}

fn source(address: &str) -> Consul {
    // In a cluster, with no secret to distribute:
    //
    //     .with_auth(Auth::kubernetes("kubernetes").with_meta("pod", hostname))
    //
    // On a laptop, whatever the environment holds — which for an agent with
    // ACLs off is nothing at all, and that is a supported answer rather than an
    // error.
    Consul::new(address, "myapp/db.json")
        .with_auth(Auth::from_environment())
        .with_wait(Duration::from_secs(10))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address =
        std::env::var("CONSUL_HTTP_ADDR").unwrap_or_else(|_| "http://127.0.0.1:8500".to_owned());

    let reader = source(&address);

    // Nothing has been reached yet: logging in is lazy, and so is fetching.
    println!("built a source for {}\n", reader.describe());

    DbConfig::set_remote(reader);
    DbConfig::refresh_remote()?;

    // No files: the fetched document is the whole configuration. Initializing
    // through the builder is also what lets `apply_remote` reload later.
    DbConfig::builder("db").env("APP_").init()?;

    println!("host = {}", DbConfig::current().host);
    println!("port = {}", DbConfig::current().port);
    println!("traced back to: {:?}", DbConfig::source_of("host")?);

    // ---------------------------------------------------------------------
    // Watching: a blocking query, so the callback runs when the key moves —
    // not on a timer.
    // ---------------------------------------------------------------------
    println!("\nwatching for 10 seconds — try another `curl -X PUT ...`");

    let watch = RemoteWatch::new();
    let watching = watch.watching();
    let watcher = source(&address);

    let loop_handle = std::thread::spawn(move || {
        watcher.watch(&watching, |document| {
            // An `on_reload` hook — with `changed_paths` for the keys that
            // moved — is where a real program would log this arriving.
            DbConfig::apply_remote(document)
        })
    });

    std::thread::sleep(Duration::from_secs(10));

    // Stopping is bounded by `with_wait`, which is why this example sets it to
    // ten seconds rather than leaving it at a minute.
    watch.stop();
    loop_handle.join().expect("the loop should end, not hang")?;

    println!("\nfinal host = {}", DbConfig::current().host);

    Ok(())
}
