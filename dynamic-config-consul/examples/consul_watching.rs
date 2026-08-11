//! Consul, watched with blocking queries — change-driven, not a poll.
//!
//! ```text
//! docker run --rm -p 8500:8500 hashicorp/consul:1.16.1
//!
//! docker exec -i $(docker ps -q -f ancestor=hashicorp/consul:1.16.1) \
//!   consul kv put myapp/db.json '{"db": {"host": "db.internal", "port": 5432}}'
//!
//! cargo run -p dynamic-config-consul --example consul_watching
//! ```
//!
//! Each request carries the index the last one returned, and the agent holds
//! it open until the key moves or `with_wait` expires — so the callback runs
//! the moment the value changes, not on the next tick of a timer.

use std::time::Duration;

use dynamic_config::{dynamic_config, RemoteSource, RemoteWatch};
use dynamic_config_consul::Consul;
use serde::Deserialize;

#[dynamic_config]
#[derive(Debug, Deserialize)]
struct DbConfig {
    host: String,
    port: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address =
        std::env::var("CONSUL_HTTP_ADDR").unwrap_or_else(|_| "http://127.0.0.1:8500".to_owned());

    // With ACLs on, add `.with_token(...)` — or `.with_auth(Auth::kubernetes(..))`
    // in a cluster, where there is no secret to distribute at all.
    let reader = Consul::new(&address, "myapp/db.json");

    println!("built a source for {}\n", reader.describe());

    // Fetch first: a watch reports *changes*, so the starting value is
    // explicitly loaded rather than waited for.
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
    // Watching: blocking, so it lives on a thread. The `Watching` token is
    // how the thread learns it is time to stop — a blocking query cannot be
    // cancelled from outside.
    // ---------------------------------------------------------------------
    println!("\nwatching for 30 seconds — try another `consul kv put ...`");

    // A short `wait` keeps this example snappy to stop; the one-minute
    // default trades that for fewer requests, which is what a real
    // deployment wants.
    let watcher = Consul::new(&address, "myapp/db.json").with_wait(Duration::from_secs(10));
    let watch = RemoteWatch::new();
    let watching = watch.watching();

    let thread = std::thread::spawn(move || {
        // the sink from `remote_sink()` is the same reload path a file edit takes:
        // validation, the reload hooks, the cache. A document that does not
        // fit leaves the previous snapshot serving.
        watcher.watch(&watching, move |document| sink.apply(document))
    });

    std::thread::sleep(Duration::from_secs(30));

    // `stop()` — or dropping `watch` — ends the loop; the blocking query is
    // waited out, which is why `with_wait` is also the worst case for exit.
    watch.stop();
    thread.join().expect("the watch thread should end")?;

    println!("\nfinal host = {}", DbConfig::current().host);

    Ok(())
}
